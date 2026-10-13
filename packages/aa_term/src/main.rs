//! 应用入口 —— 目前只做一件事：把一个真实 shell 的输出画到窗口里。
//!
//! 目的是**验证整条链路**，不是做一个完整终端：PTY → 解析 → 网格 → 调色板 →
//! 绘制。选区、滚动、鼠标、输入法都还没有。

use std::io::Read;

use aa_terminal_core::grid::GridGeometry;
use aa_terminal_core::session::{PtySession, ShellSpec, window_size_from};
use aa_terminal_core::{AnsiColors, Buffer, TerminalPalette};
use iced::{Task, window};

/// 初始视口（像素）。
const INITIAL_WIDTH: f32 = 900.;
const INITIAL_HEIGHT: f32 = 600.;

/// 应用状态。
///
/// `Option` 是因为 iced 的 `boot` 是同步的，而起 PTY / 建 Buffer 都会失败。
/// 用 `Option` 把「还没起来」和「起来了」区分开，比 panic 或静默吞掉好。
struct App {
    session: Option<PtySession>,
    buffer: Option<Buffer>,
    palette: TerminalPalette,
    geometry: GridGeometry,
}

/// 应用消息。
#[derive(Debug)]
enum Message {
    /// 启动时上报的错误。
    Failed(String),
}

fn main() -> iced::Result {
    let mut app = iced::application(App::boot, App::update, App::view)
        .title("aa_term")
        .window(window::Settings {
            size: iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT),
            ..Default::default()
        })
        // `Font` 只是「family/weight/style」的**描述**，不带字节；字体文件要用
        // `.font(bytes)` 注册，再用 family 去引用。
        .default_font(iced::Font::MONOSPACE);

    // 终端网格要求等宽字体，而 iced 自带的是比例字体，不换会导致列对不齐。
    // 注册 JetBrains Mono（含 Nerd Font 字形，终端里常要显示图标）。
    // 找不到就跳过：iced 会回退到系统等宽字体，网格仍然对得齐。
    if let Some(bytes) = load_mono_font() {
        app = app.font(bytes);
    }

    app.run()
}

/// 读取一个系统等宽字体文件。
fn load_mono_font() -> Option<Vec<u8>> {
    const CANDIDATES: [&str; 3] = [
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFontMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/NotoSansMono-Regular.ttf",
    ];
    CANDIDATES.iter().find_map(|path| std::fs::read(path).ok())
}

/// 默认调色板。
///
/// 临时用一套固定的十六进制配色，等 `aa_theme_iced` 建好后由主题层注入。
/// 注意这里**不走** `aa_theme_gpui` 的色阶 —— 那是 gpui 侧的，而且那边仍在
/// 改进中，硬绑会两边都卡住。
fn palette() -> TerminalPalette {
    use aa_terminal_core::resolve::rgb;
    use aa_theme_core::{black, hsla};

    let hex = rgb;
    TerminalPalette {
        background: hex(0x1e, 0x1e, 0x2e),
        foreground: hex(0xcd, 0xd6, 0xf4),
        bright_foreground: hex(0xf5, 0xe0, 0xdc),
        dim_foreground: hex(0xa6, 0xad, 0xc4),
        ansi_background: hex(0x1e, 0x1e, 0x2e),
        ansi: AnsiColors {
            normal: [
                hex(0x45, 0x47, 0x5a),
                hex(0xf3, 0x8b, 0xa8),
                hex(0xa6, 0xe3, 0xa1),
                hex(0xf9, 0xe2, 0xaf),
                hex(0x89, 0xb4, 0xfa),
                hex(0xf5, 0xc2, 0xe7),
                hex(0x94, 0xe2, 0xd5),
                hex(0xb6, 0xbf, 0xfb),
            ],
            bright: [
                hex(0x58, 0x5b, 0x70),
                hex(0xf3, 0x8b, 0xa8),
                hex(0xa6, 0xe3, 0xa1),
                hex(0xf9, 0xe2, 0xaf),
                hex(0x89, 0xb4, 0xfa),
                hex(0xf5, 0xc2, 0xe7),
                hex(0x94, 0xe2, 0xd5),
                hex(0xcd, 0xd6, 0xf4),
            ],
            dim: [black(); 8],
        },
        cursor: hex(0xf5, 0xe0, 0xdc),
        selection_background: hex(0x45, 0x47, 0x5a),
        selection_foreground: hex(0xcd, 0xd6, 0xf4),
    }
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let palette = palette();
        let geometry = aa_terminal_iced::geometry_for(
            iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT),
            aa_terminal_iced::BASE_FONT_SIZE,
        );
        let size = window_size_from(&geometry);

        let spec = ShellSpec::new();
        let session = PtySession::spawn(&spec.to_options("aa_term", env!("CARGO_PKG_VERSION")), size);
        let buffer = Buffer::new(size, palette.clone());

        let failed = match &session {
            Err(err) => Some(Message::Failed(format!("启动 shell 失败: {err}"))),
            Ok(_) => None,
        };

        (
            Self {
                session: session.ok(),
                buffer: Some(buffer),
                palette,
                geometry,
            },
            match failed {
                Some(msg) => Task::done(msg),
                None => Task::none(),
            },
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Failed(error) => {
                // 这一版没有错误 UI，先打到 stderr。终端起不来时窗口会是空的，
                // 日志是唯一的线索。
                eprintln!("aa_term: {error}");
                Task::none()
            }
        }
    }

    fn view(&self) -> iced::Element<'_, Message> {
        match (&self.buffer, &self.session) {
            (Some(buffer), Some(_)) => aa_terminal_iced::view(buffer, &self.palette),
            _ => iced::widget::text("终端尚未就绪").into(),
        }
    }

    /// 读一次 PTY 输出并喂进缓冲区。
    ///
    /// 一次性读，不循环 —— 等接上 iced 的 Subscription 后由事件驱动重绘。
    #[allow(dead_code)]
    fn pump_once(&mut self) {
        let (Some(session), Some(buffer)) = (&self.session, &mut self.buffer) else {
            return;
        };
        // SAFETY: fd 来自本进程打开的 PTY 主端，只读，且生命周期与 session 一致。
        let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(session.master_fd()) };
        let mut chunk = [0u8; 8192];
        match file.read(&mut chunk) {
            Ok(0) | Err(_) => {}
            Ok(n) => buffer.advance(&chunk[..n]),
        }
        // 归还 fd 所有权，避免 double close。
        std::mem::forget(file);
    }
}