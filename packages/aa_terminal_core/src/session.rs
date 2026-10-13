//! 会话层 —— 把「跑一个 shell」拆成可测的纯逻辑与必须碰系统的部分。
//!
//! ## 为什么要拆
//!
//! PTY 会话里有两类东西，可测性天差地别：
//!
//! - **纯逻辑**：环境变量怎么拼、`GridGeometry` 怎么换算成内核要的
//!   `WindowSize`、哪些尺寸是非法的。这些是这个模块 bug 的主要来源，且完全
//!   可测 —— 不需要 fork 进程。
//! - **必须碰系统**：`openpty` / `fork` / `exec` / `ioctl`。这些只能在 Unix 上
//!   跑，且测试要真起一个 shell，慢且容易在 CI 上挂。
//!
//! 所以 [`ShellSpec::to_options`] 和 [`WindowSize::from_geometry`] 是纯函数、
//! 全部有测试；[`PtySession`] 只做薄封装，冒烟测试标 `#[ignore]`。
//!
//! ## PTY 选型
//!
//! 直接用 `alacritty_terminal::tty`（底层 `rustix_openpty::openpty`），
//! 与 Zed 完全同源 —— Zed 的 `terminal::alacritty::open_pty` 只是转发给它。
//! 不引入 portable-pty / pty-process 这类额外依赖，也就不会出现
//! 「终端事件循环和 PTY 语义对不上」的问题。

use std::collections::HashMap;
use std::path::PathBuf;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty;

use crate::grid::GridGeometry;

/// 要启动的 shell。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellSpec {
    /// 程序路径。`None` 表示交给 alacritty 探测默认 shell。
    pub program: Option<String>,
    /// 传给程序的参数。
    pub args: Vec<String>,
    /// 启动目录。
    pub working_directory: Option<PathBuf>,
    /// 额外注入的环境变量。
    ///
    /// 用 `BTreeMap` 而非 `HashMap`：构造顺序要可复现，否则同一份配置两次
    /// 启动得到的子进程环境迭代顺序不同，调试时很难比对。
    pub env: std::collections::BTreeMap<String, String>,
}

impl ShellSpec {
    /// 空配置：使用系统默认 shell、当前目录、继承父进程环境。
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定程序与参数。
    pub fn with_program(mut self, program: impl Into<String>, args: Vec<String>) -> Self {
        self.program = Some(program.into());
        self.args = args;
        self
    }

    /// 指定启动目录。
    pub fn with_directory(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// 注入一个环境变量。
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// 拼出最终要注入子进程的环境。
    ///
    /// 注入的这几个不是可选项 —— 少一个都会导致实际行为退化，而且退化方式
    /// 很隐蔽（没有 `TERM` 时程序可能按 dumb 终端输出；没有 `COLORTERM` 时
    /// ripgrep / fzf 不出真彩色）：
    ///
    /// - `TERM=xterm-256color` —— 容量与 terminfo 名称
    /// - `COLORTERM=truecolor` —— 24 位色告知
    /// - `TERM_PROGRAM` / `TERM_PROGRAM_VERSION` —— 部分程序据此改行为
    ///
    /// 顺序：用户配置优先。用户在设置里显式写了 `TERM` 就该听用户的。
    pub fn environment(&self, program_name: &str, version: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        env.insert("TERM_PROGRAM".to_string(), program_name.to_string());
        env.insert("TERM_PROGRAM_VERSION".to_string(), version.to_string());
        // 用户配置最后写入，覆盖上面任何一项
        for (key, value) in &self.env {
            env.insert(key.clone(), value.clone());
        }
        env
    }

    /// 转换成 alacritty 的启动选项。
    pub fn to_options(&self, program_name: &str, version: &str) -> tty::Options {
        tty::Options {
            shell: self
                .program
                .clone()
                .map(|program| tty::Shell::new(program, self.args.clone())),
            working_directory: self.working_directory.clone(),
            drain_on_exit: true,
            env: self.environment(program_name, version),
            ..Default::default()
        }
    }
}

/// 网格几何 → 内核需要的窗口尺寸。
///
/// 用自由函数而非 `impl WindowSize`：`WindowSize` 是 alacritty 的类型，孤儿
/// 规则不允许我们给它加固有方法。
///
/// 内核要的是**格子数**（`u16`）而不是像素 —— `ioctl(TIOCSWINSZ)` 传的是行列数
/// 加像素尺寸，`cols`/`lines` 决定 shell 的换行宽度，错了整屏排版就乱了。
/// 行列数取 `num_columns`/`num_lines`（已含 `next_up()` 修正，是能放下的整格
/// 数），而不是直接 `width / cell_width` 的截断值。
pub fn window_size_from(geometry: &GridGeometry) -> WindowSize {
    WindowSize {
        num_lines: u16::try_from(geometry.num_lines()).unwrap_or(u16::MAX),
        num_cols: u16::try_from(geometry.num_columns()).unwrap_or(u16::MAX),
        cell_width: geometry.cell_width.max(0.) as u16,
        cell_height: geometry.line_height.max(0.) as u16,
    }
}

/// 两个尺寸是否在「影响排版」的字段上一致。
///
/// 像素字段刻意不参与比较：它们不影响 shell 的换行行为，拿它判断「要不要发
/// TIOCSWINSZ」会在拖拽窗口时产生大量无意义的系统调用，而每次 ioctl 都会给
/// 前台进程组发 `SIGWINCH`，满屏输出的程序会被反复唤醒。
pub fn affects_layout(a: &WindowSize, b: &WindowSize) -> bool {
    a.num_lines == b.num_lines && a.num_cols == b.num_cols
}

#[cfg(unix)]
mod imp {
    use super::*;
    use alacritty_terminal::event::OnResize;
    use alacritty_terminal::tty::Pty;
    use std::os::fd::AsRawFd;

    /// 一个已启动的 PTY 会话。
    ///
    /// 薄封装：alacritty 的 `Pty` 已经把 `openpty`/`fork`/`exec`/`ioctl`
    /// 和 SIGCHLD 管道都做完了，这里只补上「按 fd 读输出」和「按几何 resize」
    /// 两个 UI 侧真正需要的入口。
    pub struct PtySession {
        pty: Pty,
        /// 上次发给内核的尺寸，用来去重。
        last_size: WindowSize,
    }

    impl PtySession {
        /// 启动 shell。
        pub fn spawn(options: &tty::Options, window_size: WindowSize) -> std::io::Result<Self> {
            // window_id 只用于 Windows 的伪终端控制台，Unix 下传 0。
            let pty = tty::new(options, window_size, 0)?;
            Ok(Self {
                pty,
                last_size: window_size,
            })
        }

        /// 主端文件描述符，供 iced 侧挂到异步读上。
        pub fn master_fd(&self) -> std::os::fd::RawFd {
            self.pty.file().as_raw_fd()
        }

        /// 子进程 pid。
        pub fn child_id(&self) -> u32 {
            self.pty.child().id()
        }

        /// 调整窗口尺寸；尺寸未变则不发 ioctl。
        ///
        /// 去重是有意义的：拖拽窗口会连续触发上百次 layout，而 `TIOCSWINSZ`
        /// 会给前台进程组发 `SIGWINCH`，满屏输出的程序（`yes`、日志 tail）会
        /// 被反复唤醒。
        pub fn resize(&mut self, window_size: WindowSize) -> bool {
            if affects_layout(&self.last_size, &window_size) {
                return false;
            }
            self.pty.on_resize(window_size);
            self.last_size = window_size;
            true
        }
    }
}

#[cfg(unix)]
pub use imp::PtySession;

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> GridGeometry {
        GridGeometry::new(800., 480., 10., 20.)
    }

    #[test]
    fn window_size_uses_whole_cells_not_truncated_pixels() {
        let size = window_size_from(&geometry());
        assert_eq!(size.num_cols, 80);
        assert_eq!(size.num_lines, 24);
        assert_eq!(size.cell_width, 10);
        assert_eq!(size.cell_height, 20);
    }

    #[test]
    fn window_size_saturates_instead_of_wrapping() {
        // 65536 列会绕回 0，把 shell 的列数变成 0，整屏排版崩掉。
        let huge = GridGeometry::new(10_000_000., 10_000_000., 10., 20.);
        let size = window_size_from(&huge);
        assert_eq!(size.num_cols, u16::MAX, "列数溢出应饱和而不是回绕");
        assert_eq!(size.num_lines, u16::MAX);
    }

    #[test]
    fn degenerate_geometry_yields_zero_sized_window() {
        let empty = GridGeometry::new(0., 0., 10., 20.);
        let size = window_size_from(&empty);
        assert_eq!(size.num_cols, 0);
        assert_eq!(size.num_lines, 0);
    }

    #[test]
    fn layout_comparison_ignores_pixel_fields() {
        // 行列数相同就无需发 TIOCSWINSZ —— 像素变化不影响 shell 换行。
        let a = WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 10,
            cell_height: 20,
        };
        let mut b = a;
        b.cell_width = 12;
        assert!(affects_layout(&a, &b));
        b.num_cols = 81;
        assert!(!affects_layout(&a, &b), "列数变了必须重发");
    }

    #[test]
    fn environment_injects_the_required_terminal_variables() {
        let env = ShellSpec::new().environment("aa_term", "1.2.3");
        assert_eq!(env.get("TERM").unwrap(), "xterm-256color");
        assert_eq!(env.get("COLORTERM").unwrap(), "truecolor");
        assert_eq!(env.get("TERM_PROGRAM").unwrap(), "aa_term");
        assert_eq!(env.get("TERM_PROGRAM_VERSION").unwrap(), "1.2.3");
    }

    #[test]
    fn user_environment_overrides_the_defaults() {
        // 用户在设置里显式写了 TERM 就该听用户的 —— 有些人终端字体不支持
        // 需要的字形，会主动降级到 xterm。
        let spec = ShellSpec::new().with_env("TERM", "xterm");
        let env = spec.environment("aa_term", "1.0");
        assert_eq!(env.get("TERM").unwrap(), "xterm");
        // 没覆盖的仍保留默认值
        assert_eq!(env.get("COLORTERM").unwrap(), "truecolor");
    }

    #[test]
    fn options_default_to_draining_before_exit() {
        // 不 drain 会在退出时丢掉最后一批输出，看起来像「命令结果丢了半截」。
        let options = ShellSpec::new().to_options("aa_term", "1.0");
        assert!(options.drain_on_exit);
    }

    #[test]
    fn options_carry_program_directory_and_env() {
        let spec = ShellSpec::new()
            .with_program("/bin/bash", vec!["-l".to_string()])
            .with_directory("/tmp")
            .with_env("FOO", "bar");
        let options = spec.to_options("aa_term", "1.0");

        assert_eq!(
            options.working_directory.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
        assert_eq!(options.env.get("FOO").unwrap(), "bar");
        let shell = options.shell.expect("指定了程序就应有 shell");
        // Shell 的 program/args 是 pub(crate)，只能确认它存在；具体内容由
        // alacritty 自己使用。
        assert!(!format!("{shell:?}").is_empty());
    }

    #[test]
    fn empty_spec_defers_to_the_default_shell() {
        let options = ShellSpec::new().to_options("aa_term", "1.0");
        assert!(
            options.shell.is_none(),
            "未指定程序时应交给 alacritty 探测默认 shell"
        );
    }

    #[test]
    fn environment_iteration_is_deterministic() {
        // BTreeMap 保证构造顺序稳定，便于复现「同一配置两次启动行为不同」的问题。
        let spec = ShellSpec::new()
            .with_env("Z", "1")
            .with_env("A", "2")
            .with_env("M", "3");
        let keys: Vec<&str> = spec.env.keys().map(String::as_str).collect();
        assert_eq!(keys, ["A", "M", "Z"]);
    }

    /// 真起一个 shell 验证端到端。会 fork 进程、在某些环境下可能不稳定，
    /// 因此默认跳过；需要时用 `cargo test -- --ignored` 跑。
    #[cfg(unix)]
    #[test]
    #[ignore = "会真的 fork 一个 shell，CI 上不稳定"]
    fn spawns_a_real_shell_and_reports_its_pid() {
        let spec =
            ShellSpec::new().with_program("/bin/sh", vec!["-c".to_string(), "exit 0".to_string()]);
        let options = spec.to_options("aa_term", "test");
        let mut session =
            PtySession::spawn(&options, window_size_from(&geometry())).expect("应能启动 /bin/sh");
        assert!(session.child_id() > 0, "子进程 pid 应大于 0");
        assert!(session.master_fd() >= 0);

        // 同一尺寸重复 resize 不应触发 ioctl
        assert!(!session.resize(window_size_from(&geometry())));
        // 换尺寸才发
        let bigger = GridGeometry::new(1600., 480., 10., 20.);
        assert!(session.resize(window_size_from(&bigger)));
    }
}
