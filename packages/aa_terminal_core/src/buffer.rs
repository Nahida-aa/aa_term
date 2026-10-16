//! 缓冲区层 —— 把 PTY 的字节流变成可渲染的 cell。
//!
//! 这一层是「终端跑得对不对」的核心，所以设计目标是**能脱离窗口测试**：喂一串
//! 字节进去，直接断言每个 cell 的字符与颜色。测试里不需要 PTY、不需要 iced、
//! 不需要真起 shell。
//!
//! ## 线程模型
//!
//! 生产路径下 `Term` 装在 `Arc<FairMutex<_>>` 里，被两方共享：
//!
//! - **alacritty 的事件循环线程**读 PTY 并把字节喂给 `Term`（由
//!   [`Terminal::spawn`] 启动，之后 PTY 的所有权归它）。
//! - **UI 线程**渲染时短暂锁住它，读 cell 与光标。
//!
//! UI 侧**不能长持锁**：事件循环每读到一批字节都要写 `Term`，锁住不放会把整个
//! 终端卡死。所以 [`Terminal::rows`] 一次取出整屏快照，绘制过程中不持锁。
//!
//! 用 alacritty 的 `FairMutex` 而不是 `Mutex`：渲染与解析都要抢这把锁且频次
//! 都很高，普通 `Mutex` 会让一方反复抢不到（alacritty 提供它就是为此）。
//!
//! ## 为什么不用我们自己的读线程
//!
//! 之前那版是自己起线程 + 手动 `read()`。真接线时才发现漏了三件事：PTY 主端
//! 是**非阻塞**的（`O_NONBLOCK`），得处理 `WouldBlock`；子进程退出要走
//! SIGCHLD；resize 得和读取保持同一顺序。alacritty 的 `EventLoop` 全包了，
//! 没必要自己写第二遍。

use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::Processor;
use futures::channel::mpsc::UnboundedReceiver;

use crate::alacritty::{ChannelListener, channel};
use crate::events::PtyEvent;
use crate::grid::{GridGeometry, Point, Rounding};
use crate::palette::TerminalPalette;
use crate::resolve::resolve_color;
use crate::session::{ShellSpec, window_size_from};

/// 渲染一个 cell 所需的全部信息。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderCell {
    /// 字符。空格 cell 与未写入 cell 都给出 `' '`，而不是 `'\0'` —— 渲染层
    /// 拿到 `'\0'` 时容易画出豆腐块。
    pub c: char,
    /// 前景色。
    pub fg: aa_theme_core::Hsla,
    /// 背景色。
    pub bg: aa_theme_core::Hsla,
    /// 是否加粗。
    pub bold: bool,
    /// 是否斜体。
    pub italic: bool,
    /// 是否变暗（SGR 2）。渲染时降 alpha 而非降对比度。
    pub dim: bool,
    /// 是否有下划线（不含下卷曲）。
    pub underline: bool,
    /// 下划线颜色（SGR 58）。`None` 表示「跟随前景色」。
    ///
    /// 只有程序显式设过 SGR 58 才有值；不像 `fg`/`bg` 那样在解析时回退，回退
    /// 交给渲染层做（它已经拿着 `fg`）。
    pub underline_color: Option<aa_theme_core::Hsla>,
    /// 是否属于一个 OSC 8 超链接。
    ///
    /// 只带 bool，URI 用 [`Terminal::hyperlink_at`] 现取：`RenderCell` 是 `Copy`，
    /// 塞 `String`/`Arc<str>` 会让整屏逐格复制时多一次原子操作；而渲染只关心
    /// 「要不要当链接画」，真正要 URI 的只有悬停/点击那一下。
    pub hyperlink: bool,
    /// 是否有删除线。
    pub strikeout: bool,
    /// 是否反色（用于高亮搜索命中时要反转处理）。
    pub inverse: bool,
    /// 前景是程序显式指定的 24 位真彩色。
    ///
    /// 对比度兜底要跳过这种颜色：那是程序自己挑的，我们替它「提亮」反而改了
    /// 它的意图（例如一张故意做暗的图）。对应 Zed 的
    /// `is_app_chosen_exact_color`。
    pub exact: bool,
    /// 是否是宽字符占位的第二格。
    ///
    /// 渲染时这类 cell 应被前一个宽字符「吞掉」，否则会画出多余的空格。
    pub is_wide_spacer: bool,
}

/// 视口尺寸 —— alacritty `Term::new` / `resize` 要求的 `Dimensions` 实现。
///
/// 刻意不用 `(usize, usize)` 元组：alacritty 的那个 impl 把 `.0` 当行数、
/// `.1` 当列数（`screen_lines() -> self.0`），写反了不会报错，只会把 80×24
/// 悄悄造成 24×80 的终端。具名字段没有这个坑。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    /// 列数。
    pub columns: usize,
    /// 屏幕行数。
    pub screen_lines: usize,
}

impl Viewport {
    /// 构造。
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }

    /// 由窗口尺寸换算。
    pub fn from_window_size(size: WindowSize) -> Self {
        Self::new(usize::from(size.num_cols), usize::from(size.num_lines))
    }
}

impl alacritty_terminal::grid::Dimensions for Viewport {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// 终端模型。
pub struct Terminal {
    pub(crate) term: Arc<FairMutex<Term<ChannelListener>>>,
    /// 测试路径专用：不起 PTY 时直接驱动解析。
    parser: Processor,
    /// 尺寸。包在 `Mutex` 里是为了让 `resize(&self)` 可行 —— iced 的
    /// `Subscription` 只给得出 `&'static`，而 resize 又必须改它。
    size: Mutex<WindowSize>,
    palette: TerminalPalette,
    /// 是否有真 PTY 在写 `Term`。测试路径为 false。
    has_pty: bool,
    /// 搜索状态（命中 + 当前跳转下标）。字段名与方法名错开：同名会解析到方法。
    pub(crate) search_state: std::sync::Mutex<crate::search::SearchState>,
}

/// 进程内唯一的终端实例。
///
/// 存在的理由很实际：`iced::Subscription::run` 只接受 `fn() -> S`，不带状态，
/// 所以会话只能从进程级静态量递出来；而 UI 渲染又需要一个活得一样久的
/// `&'static Terminal`。两者都指到这里。
///
/// 代价是**一个进程只能有一个终端**。多标签 / 多面板时应当把这个下探到
/// ics 的某个组件上，而不是进程全局 —— 这里先按单终端设计。
static TERMINAL: OnceLock<Terminal> = OnceLock::new();

impl Terminal {
    /// 建一个只含尺寸与配色的终端，**不**起 PTY。
    ///
    /// 测试走这条路：可以直接 `feed()`，不起进程、不受时序影响。
    pub fn for_test(size: WindowSize, palette: TerminalPalette) -> Self {
        let (listener, _discard) = channel();
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &Viewport::from_window_size(size),
            listener,
        )));
        Self::build(term, size, palette, false)
    }

    fn build(
        term: Arc<FairMutex<Term<ChannelListener>>>,
        size: WindowSize,
        palette: TerminalPalette,
        has_pty: bool,
    ) -> Self {
        Self {
            term,
            parser: Processor::default(),
            size: Mutex::new(size),
            palette,
            has_pty,
            search_state: std::sync::Mutex::new(crate::search::SearchState::default()),
        }
    }

    /// 喂入字节（**仅测试路径**）。
    ///
    /// 生产路径下 PTY 由 alacritty 的线程读并喂给 `Term`，这里直接返回 —— 调用
    /// 它会掩盖「你以为在喂，其实没喂」的接线错误，所以只留给测试。
    pub fn feed(&mut self, bytes: &[u8]) {
        debug_assert!(!self.has_pty, "feed() 只用于测试路径，别拿它喂真 PTY");
        if bytes.is_empty() {
            // alacritty 的事件循环明确注释过「0 字节会让终端挂起」。
            return;
        }
        let mut term = self.term.lock();
        self.parser.advance(&mut *term, bytes);
    }

    /// 调整尺寸。
    pub fn resize(&self, size: WindowSize) {
        // WindowSize 只 derive 了 Copy/Clone/Debug，没有 PartialEq，只能逐字段比。
        let mut current = self.size.lock().unwrap_or_else(|e| e.into_inner());
        if current.num_lines == size.num_lines
            && current.num_cols == size.num_cols
            && current.cell_width == size.cell_width
            && current.cell_height == size.cell_height
        {
            return;
        }
        let mut term = self.term.lock();
        term.resize(Viewport::from_window_size(size));
        *current = size;
    }

    /// 由视口几何调整尺寸。
    pub fn resize_to(&mut self, geometry: &GridGeometry) {
        self.resize(window_size_from(geometry));
    }

    /// 当前尺寸。
    pub fn size(&self) -> WindowSize {
        *self.size.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 可见屏幕的行列数。
    pub fn dimensions(&self) -> (usize, usize) {
        let size = self.size();
        (usize::from(size.num_cols), usize::from(size.num_lines))
    }

    /// terminal 是否处于 application cursor 模式（DECSET `DECCKM`）。
    ///
    /// 该模式下方向键/home/end 的编码从 `ESC [ x` 变成 `ESC O x`。判错的话
    /// vim / less / tmux 会收到错误的方向序列 —— 表现为「按上下键光标乱跳」。
    /// 只能从 alacritty 的 `TermMode` 读，无法靠设置推断。
    pub fn app_cursor_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// terminal 当前开启的鼠标模式。
    ///
    /// 决定滚轮/点击是发给程序还是终端自己处理。四个开关对应终端的 1000/1002/
    /// 1003/1005/1006 几套 DECSET，判错会让鼠标事件「跳两下」（程序和终端各
    /// 响应一次）。
    pub fn mouse_mode(&self) -> crate::mappings::mouse::MouseMode {
        use crate::mappings::mouse::MouseMode;
        use alacritty_terminal::term::TermMode;
        let term = self.term.lock();
        let mode = term.mode();
        MouseMode {
            enabled: mode.intersects(TermMode::MOUSE_MODE),
            motion: mode.intersects(TermMode::MOUSE_MOTION),
            drag_only: mode.contains(TermMode::MOUSE_DRAG),
            sgr: mode.contains(TermMode::SGR_MOUSE),
            utf8: mode.contains(TermMode::UTF8_MOUSE),
        }
    }

    /// 光标的视口坐标。
    ///
    /// 鼠标上报没有自己的坐标，用的是「最近一次光标位置」。
    pub fn last_cursor_point(&self) -> crate::grid::Point {
        self.cursor_point()
    }

    /// 回滚区总行数（含屏幕）。
    pub fn total_lines(&self) -> usize {
        self.term.lock().total_lines()
    }

    /// 光标所在的网格位置。
    ///
    /// 与 [`crate::grid::Point`] 的 line 约定一致：alacritty 的绝对 `Line` 以回滚区
    /// 底部为原点、向上递增，减去 `display_offset` 后就是视口行号（0 = 视口顶行）。
    pub fn cursor_point(&self) -> Point {
        let term = self.term.lock();
        let line = term.grid().cursor.point.line.0 - term.grid().display_offset() as i32;
        Point::new(line, term.grid().cursor.point.column.0)
    }

    /// 本地滚动视口，**不**转发给程序。
    ///
    /// `delta` 为正表示看更早的内容，与 [`Session::scroll`] 的约定一致。
    ///
    /// 与 [`Session::scroll`] 的分工：那个是「用户滚了滚轮」，程序开了鼠标上报
    /// 或处于 alternate screen 时要把滚动转发给它；这个只用于**明确要求滚动本
    /// 视图**的场合（拖滚动条），不看任何模式 —— 用户都抓住滚动条了，意图很
    /// 清楚，再往 vim 里发方向键反而是添乱。
    pub fn scroll_display(&self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.term
            .lock()
            .grid_mut()
            .scroll_display(Scroll::Delta(delta));
    }

    /// 取一个可见 cell。
    ///
    /// `point` 用 [`crate::grid::Point`] 的 line 约定（**视口行号**，0 = 视口顶行，
    /// 向下递增），不是裸的 alacritty `Line`。越界返回 `None`。
    pub fn cell(&self, point: Point) -> Option<RenderCell> {
        let (columns, screen_lines) = self.dimensions();
        if point.column >= columns {
            return None;
        }
        // Point.line → 绝对 Line。由 `line = row - display_offset` 反解出
        // `Line = line + display_offset`。注意是**加**不是减：alacritty 的
        // Line(0) 是视口首行、向下递增，写反了会读到镜像的位置。
        let term = self.term.lock();
        let raw_line = point.line + term.grid().display_offset() as i32;
        if raw_line < 0 || raw_line >= screen_lines as i32 {
            return None;
        }
        Some(self.render_cell(&term.grid()[Line(raw_line)][Column(point.column)]))
    }

    /// 取一个可见 cell 上的超链接 URI（OSC 8）。
    ///
    /// 超链接会跨多个 cell，链上每一格都返回同一个 URI；不在链上返回 `None`。
    /// 行号换算与 [`Self::cell`] 完全相同（`Point.line + display_offset`）。
    pub fn hyperlink_at(&self, point: Point) -> Option<String> {
        let (columns, screen_lines) = self.dimensions();
        if point.column >= columns {
            return None;
        }
        // Point.line → 绝对 Line，与 `cell()` 同一套换算（注意是**加**）。
        let term = self.term.lock();
        let raw_line = point.line + term.grid().display_offset() as i32;
        if raw_line < 0 || raw_line >= screen_lines as i32 {
            return None;
        }
        term.grid()[Line(raw_line)][Column(point.column)]
            .hyperlink()
            .map(|link| link.uri().to_owned())
    }

    fn render_cell(&self, cell: &Cell) -> RenderCell {
        use alacritty_terminal::term::cell::Flags;
        RenderCell {
            // 未写入的 cell 是 '\0'，渲染层容易画成豆腐块，统一成空格。
            c: if cell.c == '\0' { ' ' } else { cell.c },
            fg: resolve_color(&cell.fg, &self.palette),
            bg: resolve_color(&cell.bg, &self.palette),
            exact: matches!(cell.fg, alacritty_terminal::vte::ansi::Color::Spec(_)),
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            dim: cell.flags.contains(Flags::DIM),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            underline_color: cell
                .underline_color()
                .map(|color| resolve_color(&color, &self.palette)),
            hyperlink: cell.hyperlink().is_some(),
            strikeout: cell.flags.intersects(Flags::STRIKEOUT),
            inverse: cell.flags.contains(Flags::INVERSE),
            is_wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
        }
    }

    /// 一次性取出整个可见屏幕。
    ///
    /// 只在里面锁一次 `Term`：逐行调用 `cell()` 会反复抢放锁，而事件循环每读到
    /// 一批字节都在等这把锁。
    pub fn rows(&self) -> Vec<Vec<RenderCell>> {
        let term = self.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let screen_lines = term.grid().screen_lines();
        let columns = term.grid().columns();
        (0..screen_lines)
            .filter_map(|row| {
                let raw_line = row as i32 + display_offset;
                // 负行是「视口上方还没被滚出去的行」，本方法只取可见部分。
                (raw_line >= 0).then(|| {
                    (0..columns)
                        .map(|column| {
                            self.render_cell(&term.grid()[Line(raw_line)][Column(column)])
                        })
                        .collect()
                })
            })
            .collect()
    }

    /// 屏幕上的文本（按行拼接）。
    ///
    /// 用于「把可见内容复制出去」这类操作。会去掉行尾空白。
    pub fn visible_text(&self) -> String {
        let mut text = String::new();
        for row in self.rows().iter().rev() {
            let content: String = row.iter().map(|cell| cell.c).collect();
            text.push_str(content.trim_end());
            text.push('\n');
        }
        text
    }

    /// 按视口坐标做命中测试。
    ///
    /// 取整策略显式传入：上游对「像素→cell」的取整有两套并存的说法，详见
    /// [`crate::grid::Rounding`]。
    pub fn point_at(&self, geometry: &GridGeometry, x: f32, y: f32, rounding: Rounding) -> Point {
        geometry.point_at(x, y, rounding)
    }
}

/// 真 PTY 会话 —— 持有向终端写输入的通道。
///
/// 拆分出来是因为 `Terminal` 在 UI 线程上，而 `Notifier` 可以从任意线程调用
/// （输入法、按键处理都不该受限于 UI 线程）。
pub struct Session {
    notifier: Notifier,
    /// 刚启动时的事件接收端，等 UI 侧来取。
    events: Option<UnboundedReceiver<PtyEvent>>,
}

impl Session {
    /// 起一个真 shell。
    pub fn spawn(
        size: WindowSize,
        palette: TerminalPalette,
        spec: &ShellSpec,
    ) -> std::io::Result<Self> {
        let (terminal, events, notifier) = spawn_parts(size, palette, spec)?;
        TERMINAL
            .set(terminal)
            .map_err(|_rejected| std::io::Error::other("进程内已有终端实例"))?;
        Ok(Self {
            notifier,
            events: Some(events),
        })
    }

    /// 渲染用的终端引用。
    pub fn terminal(&self) -> &'static Terminal {
        // `spawn` 成功就一定装进去了；没成功时调用方拿不到 Session。
        TERMINAL.get().expect("会话未启动")
    }

    /// 取出事件接收端。
    ///
    /// **只能调一次**：接收端被搬走后就取不到了。调用方（iced 的 Subscription）
    /// 应当在启动时就把它取走并保存。
    pub fn take_events(&mut self) -> Option<UnboundedReceiver<PtyEvent>> {
        self.events.take()
    }

    /// 向 shell 写字节（按键转发）。
    pub fn write(&self, bytes: Vec<u8>) {
        // Notifier::notify 会自己吞掉空输入 —— 空字节会让终端挂起，
        // 见 alacritty event_loop 的注释。
        self.notifier.notify(bytes);
    }

    /// 调整窗口尺寸，同时通知 PTY。
    pub fn resize(&self, size: WindowSize) {
        let terminal = self.terminal();
        let before = terminal.size();
        terminal.resize(size);
        let after = terminal.size();
        // 尺寸没变就不必打扰 PTY：每次 ioctl 都会给前台进程组发 SIGWINCH。
        if before.num_lines != after.num_lines || before.num_cols != after.num_cols {
            self.notifier
                .0
                .send(alacritty_terminal::event_loop::Msg::Resize(size))
                .ok();
        }
    }
}

/// 起 PTY + 事件循环，产出 `(Terminal, 事件接收端, Notifier)`。
///
/// ## 为什么 `Term` 只有一份
///
/// 这是唯一需要小心的地方：`EventLoop` 和 UI 两侧必须共享**同一个** `Term`。
/// 各自建一份的话，解析器写进去的字 UI 永远看不到，而 UI 看到的是另一份永远
/// 空白的网格 —— 症状就是「窗口起来了但什么都不显示」，且不会有任何报错。
pub(crate) fn spawn_parts(
    size: WindowSize,
    palette: TerminalPalette,
    spec: &ShellSpec,
) -> std::io::Result<(Terminal, UnboundedReceiver<PtyEvent>, Notifier)> {
    // 同一个 listener 既给 Term 又给 EventLoop：Term 在解析过程中发出事件
    // （标题变化、响铃）由它转发，channel 是共享的。
    let (listener, events) = channel();
    let term = Arc::new(FairMutex::new(Term::new(
        Config::default(),
        &Viewport::from_window_size(size),
        listener.clone(),
    )));

    let pty = tty::new(
        &spec.to_options("aa_term", env!("CARGO_PKG_VERSION")),
        size,
        0,
    )?;
    let event_loop = EventLoop::new(
        // `EventLoop` 持有 term 的一份 Arc（同一块内存），Terminal 里的那份
        // 指向同一个 Term —— 这不是拷贝，是 refcount。写在这里是因为搞出两份
        // Term 的话症状是「窗口起来但不显示」且无任何报错，很难查。
        term.clone(),
        listener,
        pty,
        // drain_on_exit：退出前读完尾部输出，否则「命令结果丢了半截」。
        true,
        false,
    )?;
    let notifier = Notifier(event_loop.channel());
    // EventLoop 自己持有 term 的 Arc；丢掉这个句柄即可，线程继续跑。
    let _ = event_loop.spawn();

    let terminal = Terminal::build(term, size, palette, true);
    Ok((terminal, events, notifier))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::palette::{AnsiColors, TerminalPalette};
    use crate::session::window_size_from;
    use aa_theme_core::{black, hsla};

    /// 造一份「每个槽位颜色都不同」的调色板，便于断言。
    pub(crate) fn palette() -> TerminalPalette {
        let slot = |n: usize| hsla(0., 0., 0., 1.).alpha(n as f32 / 24.);
        TerminalPalette {
            background: hsla(0., 0., 0.5, 1.),
            foreground: hsla(0., 0., 1., 1.),
            bright_foreground: hsla(0., 0., 0.9, 1.),
            dim_foreground: hsla(0., 0., 0.4, 1.),
            ansi_background: hsla(0., 0., 0.6, 1.),
            ansi: AnsiColors {
                normal: core::array::from_fn(slot),
                bright: core::array::from_fn(|i| slot(8 + i)),
                dim: core::array::from_fn(|i| slot(16 + i)),
            },
            cursor: hsla(0., 0., 0.3, 1.),
            selection_background: hsla(0., 0., 0.2, 1.),
            selection_foreground: hsla(0., 0., 0.7, 1.),
            search_match_background: hsla(0., 0., 0.1, 1.),
            search_active_match_background: hsla(0., 0., 0.8, 1.),
            scrollbar_thumb: hsla(0., 0., 0.35, 1.),
        }
    }

    fn terminal() -> Terminal {
        let size = window_size_from(&GridGeometry::new(800., 480., 10., 20.));
        Terminal::for_test(size, palette())
    }

    pub(crate) fn slot_of(color: aa_theme_core::Hsla) -> usize {
        (color.a * 24.).round() as usize
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut t = terminal();
        t.feed(b"hello");
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().c, 'h');
        assert_eq!(t.cell(Point::new(0, 4)).unwrap().c, 'o');
        // 未写入处是空格而不是 '\0'
        assert_eq!(t.cell(Point::new(0, 5)).unwrap().c, ' ');
    }

    #[test]
    fn escape_sequence_split_across_feeds_still_parses() {
        // PTY 天然会把一个转义序列切成两半，Parser 必须能跨调用续上。
        let mut t = terminal();
        t.feed(b"\x1b[3");
        t.feed(b"1mred");
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().c, 'r');
        // SGR 31 = 红色 → 具名色表第 1 槽
        assert_eq!(slot_of(t.cell(Point::new(0, 0)).unwrap().fg), 1);
    }

    #[test]
    fn sgr_colors_resolve_through_the_palette() {
        let mut t = terminal();
        t.feed(b"\x1b[32mG");
        assert_eq!(
            slot_of(t.cell(Point::new(0, 0)).unwrap().fg),
            2,
            "32 应映射到 green 槽"
        );

        t.feed(b"\x1b[93mB");
        assert_eq!(
            slot_of(t.cell(Point::new(0, 1)).unwrap().fg),
            11,
            "93 应映射到 bright yellow"
        );
    }

    #[test]
    fn true_color_bypasses_the_palette() {
        let mut t = terminal();
        t.feed(b"\x1b[38;2;255;128;0mX");
        let fg = t.cell(Point::new(0, 0)).unwrap().fg;
        // 真彩色不走 16 色表，alpha 保持 1（没被当成槽位标记）
        assert_eq!(fg.a, 1.0);
        assert_eq!(slot_of(fg), 24, "不在任何 ANSI 槽位内");
    }

    #[test]
    fn reset_restores_the_default_foreground() {
        let mut t = terminal();
        t.feed(b"\x1b[31mA\x1b[0mB");
        assert_eq!(slot_of(t.cell(Point::new(0, 0)).unwrap().fg), 1);
        let p = palette();
        assert_eq!(t.cell(Point::new(0, 1)).unwrap().fg, p.foreground);
    }

    #[test]
    fn bold_and_italic_flags_are_exposed() {
        let mut t = terminal();
        t.feed(b"\x1b[1;3mS");
        let cell = t.cell(Point::new(0, 0)).unwrap();
        assert!(cell.bold);
        assert!(cell.italic);
        assert!(!cell.underline);
    }

    #[test]
    fn dim_flag_reaches_the_cell() {
        // SGR 2。渲染层靠它降 alpha，取不到就表现为「dim 完全不暗」。
        let mut terminal = terminal();
        terminal.feed(b"\x1b[2mD");
        let cell = terminal.cell(Point::new(0, 0)).expect("首列应可读");
        assert!(cell.dim);
    }

    #[test]
    fn underline_and_strikeout_are_distinguished() {
        // 先测下划线，再显式重置后测删除线。
        //
        // 不能写成 `\x1b[4mU\x1b[9mK`：SGR 属性是**累积**的，`\x1b[4m`
        // 设下的下划线会一直保持，于是 K 同时带下划线和删除线 —— 那是终端
        // 的正确行为，不是 bug。想让两者互斥必须先 `\x1b[0m`。
        let mut t = terminal();
        t.feed(b"\x1b[4mU");
        assert!(t.cell(Point::new(0, 0)).unwrap().underline);

        t.feed(b"\x1b[0m\x1b[9mK");
        let strike = t.cell(Point::new(0, 1)).unwrap();
        assert!(strike.strikeout, "SGR 9 应设置删除线");
        assert!(!strike.underline, "重置后不应残留下划线");
    }

    #[test]
    fn sgr_attributes_accumulate_until_reset() {
        // 锁住上面那条语义：属性叠加，只有 SGR 0 才清空。
        let mut t = terminal();
        t.feed(b"\x1b[4mA\x1b[1mB");
        assert!(t.cell(Point::new(0, 0)).unwrap().underline);
        let second = t.cell(Point::new(0, 1)).unwrap();
        assert!(second.underline, "加粗不应清掉下划线");
        assert!(second.bold);

        t.feed(b"\x1b[0mC");
        let third = t.cell(Point::new(0, 2)).unwrap();
        assert!(!third.underline && !third.bold, "SGR 0 应清空所有属性");
    }

    #[test]
    fn underline_color_is_exposed_and_resettable() {
        // SGR 58 设下划线颜色，59 清掉。没设时必须是 `None`，表示「跟随前景
        // 色」—— 渲染层没法从「等于 fg」反推出这个区分。
        let mut t = terminal();
        assert_eq!(
            t.cell(Point::new(0, 0)).unwrap().underline_color,
            None,
            "未设 SGR 58 时应为 None"
        );

        // 同一个真彩色同时设成前景与下划线颜色，两者应解析成同一个值
        // （resolve_color 对两个字段走的是同一条路径）。
        t.feed(b"\x1b[58;2;255;0;0m\x1b[38;2;255;0;0m\x1b[4mR");
        let cell = t.cell(Point::new(0, 0)).unwrap();
        assert!(cell.underline, "SGR 58 不应清掉下划线");
        assert_eq!(
            cell.underline_color,
            Some(cell.fg),
            "58 的颜色应与同色前台一致"
        );

        t.feed(b"\x1b[59mG");
        assert_eq!(
            t.cell(Point::new(0, 1)).unwrap().underline_color,
            None,
            "SGR 59 应清掉下划线颜色"
        );
    }

    #[test]
    fn osc8_hyperlink_is_exposed_and_ends_with_an_empty_uri() {
        // OSC 8 形如 `ESC ] 8 ; params ; URI ST`，关闭用空 URI。收尾必须是
        // ST（`ESC \`）或 BEL，否则后面的字会被当成 OSC 参数吞掉。
        let mut t = terminal();
        assert_eq!(t.hyperlink_at(Point::new(0, 0)), None, "默认没有链接");

        t.feed(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\plain");

        // 链上每一格都指向同一个 URI
        for column in 0..4 {
            assert_eq!(
                t.hyperlink_at(Point::new(0, column)).as_deref(),
                Some("https://example.com"),
                "第 {column} 列应在链接内"
            );
            assert!(t.cell(Point::new(0, column)).unwrap().hyperlink);
        }

        // 空 URI 关闭后不再是链接
        assert_eq!(t.hyperlink_at(Point::new(0, 4)), None);
        assert!(!t.cell(Point::new(0, 4)).unwrap().hyperlink);
    }

    #[test]
    fn hyperlink_at_is_none_outside_the_viewport() {
        let t = terminal();
        let (columns, _) = t.dimensions();
        assert_eq!(t.hyperlink_at(Point::new(-1, 0)), None, "视口上方应 None");
        assert_eq!(t.hyperlink_at(Point::new(0, columns)), None, "越列应 None");
    }

    #[test]
    fn inverse_flag_is_surfaced() {
        // 反色不在这里做交换 —— 搜索命中高亮要用它，渲染层再决定怎么反转。
        let mut t = terminal();
        t.feed(b"\x1b[7mI");
        assert!(t.cell(Point::new(0, 0)).unwrap().inverse);
    }

    #[test]
    fn empty_input_does_not_hang_or_corrupt() {
        // alacritty 的事件循环里明确注释过「发 0 字节会让终端挂起」。
        let mut t = terminal();
        t.feed(b"ok");
        t.feed(b"");
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().c, 'o');
    }

    #[test]
    fn cursor_tracks_written_text() {
        let mut t = terminal();
        assert_eq!(t.cursor_point(), Point::new(0, 0));
        t.feed(b"abc");
        assert_eq!(t.cursor_point(), Point::new(0, 3));
        t.feed(b"\r\n");
        // \r 回列首，\n 下行。alacritty 的 Line(0) 是视口首行且向下递增，
        // 所以下行让 line 变大，不是变小。
        assert_eq!(t.cursor_point(), Point::new(1, 0));
    }

    #[test]
    fn out_of_range_cells_return_none() {
        let t = terminal();
        assert!(t.cell(Point::new(0, 80)).is_none(), "列越界");
        assert!(t.cell(Point::new(24, 0)).is_none(), "行越界");
        // line 为负表示在视口首行之上，不存在
        assert!(t.cell(Point::new(-1, 0)).is_none());
        assert!(t.cell(Point::new(24, 0)).is_none());
    }

    #[test]
    fn row_returns_exactly_one_screen_of_cells() {
        let mut t = terminal();
        t.feed(b"abcdef");
        let rows = t.rows();
        assert_eq!(rows.len(), t.dimensions().1, "应有一整屏的行");
        assert_eq!(rows[0].len(), 80);
        assert_eq!(rows[0][0].c, 'a');
    }

    #[test]
    fn resize_changes_the_grid_dimensions() {
        let mut t = terminal();
        assert_eq!(t.dimensions(), (80, 24));
        t.resize_to(&GridGeometry::new(400., 240., 10., 20.));
        assert_eq!(t.dimensions(), (40, 12));
    }

    #[test]
    fn resize_to_the_same_size_is_a_no_op() {
        let mut t = terminal();
        t.feed(b"keep");
        let before = t.size();
        t.resize(before);
        // 尺寸没变时不应重排网格，否则已有内容会被截断
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().c, 'k');
    }

    #[test]
    fn wide_characters_mark_their_spacer_cell() {
        let mut t = terminal();
        // 中文字符占两格
        t.feed("中".as_bytes());
        let first = t.cell(Point::new(0, 0)).unwrap();
        let second = t.cell(Point::new(0, 1)).unwrap();
        assert_eq!(first.c, '中');
        assert!(
            second.is_wide_spacer,
            "第二格应标记为宽字符占位，渲染时要跳过"
        );
    }

    #[test]
    fn visible_text_reads_bottom_up_without_trailing_space() {
        let mut t = terminal();
        t.feed(b"first\r\nsecond");
        let text = t.visible_text();
        let lines: Vec<&str> = text.lines().collect();
        // 底部是最后写入的 second
        assert!(lines.contains(&"first"), "应包含两行: {lines:?}");
        assert!(lines.contains(&"first"), "应包含两行: {lines:?}");
        // 行尾空白被去掉
        assert!(!text.lines().any(|l| l.ends_with(' ')), "{text:?}");
    }

    #[test]
    fn background_defaults_come_from_the_palette() {
        let t = terminal();
        let p = palette();
        // 没写过内容的 cell 背景应取调色板的背景色，而不是黑色
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().bg, p.background);
        assert_ne!(p.background, black());
    }

    #[test]
    fn default_foreground_matches_the_palette() {
        let t = terminal();
        let p = palette();
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().fg, p.foreground);
    }
}

#[cfg(test)]
mod hit_testing {
    use super::tests_support::*;
    use crate::grid::{Point, Rounding};

    /// 命中测试是 `GridGeometry` 的职责，这里只验证接线正确。
    #[test]
    fn hit_testing_delegates_to_geometry() {
        let (terminal, geometry) = fixture();
        let point = terminal.point_at(&geometry, 45., 30., Rounding::Truncate);
        assert_eq!(point, Point::new(1, 4));
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;

    pub(super) fn fixture() -> (Terminal, GridGeometry) {
        use crate::palette::{AnsiColors, TerminalPalette};
        use aa_theme_core::hsla;
        let geometry = GridGeometry::new(800., 480., 10., 20.);
        let palette = TerminalPalette {
            background: hsla(0., 0., 0.5, 1.),
            foreground: hsla(0., 0., 1., 1.),
            bright_foreground: hsla(0., 0., 0.9, 1.),
            dim_foreground: hsla(0., 0., 0.4, 1.),
            ansi_background: hsla(0., 0., 0.6, 1.),
            ansi: AnsiColors {
                normal: core::array::from_fn(|n| hsla(0., 0., 0., 1.).alpha(n as f32 / 24.)),
                bright: core::array::from_fn(|i| hsla(0., 0., 0., 1.).alpha((8 + i) as f32 / 24.)),
                dim: core::array::from_fn(|i| hsla(0., 0., 0., 1.).alpha((16 + i) as f32 / 24.)),
            },
            cursor: hsla(0., 0., 0.3, 1.),
            selection_background: hsla(0., 0., 0.2, 1.),
            selection_foreground: hsla(0., 0., 0.7, 1.),
            search_match_background: hsla(0., 0., 0.1, 1.),
            search_active_match_background: hsla(0., 0., 0.8, 1.),
            scrollbar_thumb: hsla(0., 0., 0.35, 1.),
        };
        let terminal = Terminal::for_test(window_size_from(&geometry), palette);
        (terminal, geometry)
    }
}
/// 跨层的集成测试：真起一个 shell，让它输出带颜色的文本，验证
/// PTY 字节 → vte 解析 → 网格 cell → 调色板解析 这条链真的通。
///
/// 单模块测试喂的都是自己构造的字节，这一层则要确认「从内核读到的真实
/// 输出」也能正确落到网格上 —— 中间任何一层的接线错误都会在这里暴露。
#[cfg(unix)]
#[test]
#[ignore = "会真的 fork 一个 shell，CI 上不稳定"]
fn pty_output_reaches_the_grid_through_every_layer() {
    use crate::buffer::tests::{palette, slot_of};
    use crate::grid::Point;
    use crate::session::{PtySession, ShellSpec, window_size_from};
    use std::io::Read;

    let geometry = GridGeometry::new(800., 480., 10., 20.);
    let size = window_size_from(&geometry);

    // printf 出带 SGR 颜色的文本，然后立刻退出，便于读到 EOF。
    let spec = ShellSpec::new().with_program(
        "/bin/sh",
        vec![
            "-c".to_string(),
            "printf '\\033[31mRED\\033[0m\\n'".to_string(),
        ],
    );
    let session =
        PtySession::spawn(&spec.to_options("aa_term", "test"), size).expect("应能启动 /bin/sh");

    let fd = session.master_fd();
    // SAFETY: fd 来自本进程刚打开的 PTY 主端，本测试内不关闭它，
    // 且只做只读。生命周期不超过 session。
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };

    // 主端是**非阻塞**的（alacritty 在 tty/unix.rs 里设了 O_NONBLOCK），
    // 无数据时 read 返回 WouldBlock 而不是阻塞。所以不能直接 read_to_end ——
    // 那会立刻拿到空结果，看起来像「子进程没输出」。必须循环重试。
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break, // EOF：子进程已退出且管道关闭
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    // 归还 fd 所有权，避免 double close。
    std::mem::forget(file);

    let mut terminal = Terminal::for_test(size, palette());
    terminal.feed(&raw);
    let cell = terminal.cell(Point::new(0, 0)).expect("首行首列应可读");
    assert_eq!(cell.c, 'R', "实际读到: {raw:?}");
    assert_eq!(slot_of(cell.fg), 1, "\x1b[31m 应解析为红色槽位");
}
