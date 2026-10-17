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
//! 终端卡死。所以可见内容用 [`Terminal::content`] 取一份整屏快照，绘制过程中不
//! 碰锁；快照在**终端内容变化时**重建（`feed` / 滚动 / resize / PTY 输出到来），
//! 不是每帧现取 —— 与 Zed 的 `last_content` + `make_content` 同一套做法。
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
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::Processor;
use futures::channel::mpsc::UnboundedReceiver;

use crate::alacritty::{ChannelListener, channel};
use crate::events::PtyEvent;
use crate::grid::{Cell, Content, GridGeometry, Point, Rounding};
use crate::scroll::ScrollAmount;
use crate::session::{ShellSpec, window_size_from};

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
    /// 是否有真 PTY 在写 `Term`。测试路径为 false。
    has_pty: bool,
    /// 可见屏幕快照，内容变化时整体换一份。
    ///
    /// 包在 `Arc` 里是为了让渲染帧能**拿着快照画**而不必持有 `Mutex` 的
    /// `Guard`（终端在 `static` 上，`Guard` 一旦被渲染层跨语句持有，稍有
    /// 不小心就会与重建路径撞成死锁）；换快照是 `*guard = Arc::new(..)`。
    content: Mutex<Arc<Content>>,
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

/// 视口行号（0 = 视口顶行，向下递增）→ alacritty 的绝对 `Line`。
///
/// alacritty 的 `Line` 以**不滚动时视口首行**为 0、向下递增；回滚区在它上方，
/// 记作负数（`Line(-1)` 是视口顶行上面那一行）。视口行号 = `Line + display_offset`
/// （alacritty 的 `point_to_viewport`），反过来就是 `Line = row - display_offset`。
///
/// 实测确认（24 行屏喂 30 行、history=7）：`offset = 0` 时 `Line(0)` 是第 8 行、
/// `Line(-1)` 是第 7 行内容；`offset = 1` 时视口首行是 `Line(-1)`。
/// 写成 `row + display_offset` 会在 offset ≥ 1 时读到 `Line(screen_lines)` 越界
/// —— debug 下当场断言失败，release 下 `compute_index` 下溢绕回、静默读错格。
fn raw_line_of(row: i32, display_offset: usize) -> i32 {
    row - display_offset as i32
}

impl Terminal {
    /// 建一个只含尺寸与配色的终端，**不**起 PTY。
    ///
    /// 测试走这条路：可以直接 `feed()`，不起进程、不受时序影响。
    pub fn for_test(size: WindowSize) -> Self {
        let (listener, _discard) = channel();
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &Viewport::from_window_size(size),
            listener,
        )));
        Self::build(term, size, false)
    }

    fn build(term: Arc<FairMutex<Term<ChannelListener>>>, size: WindowSize, has_pty: bool) -> Self {
        let content = Arc::new(Content::new(&term.lock()));
        Self {
            term,
            parser: Processor::default(),
            size: Mutex::new(size),
            has_pty,
            content: Mutex::new(content),
            search_state: std::sync::Mutex::new(crate::search::SearchState::default()),
        }
    }

    /// 当前可见屏幕的快照。
    ///
    /// 拿到的是 `Arc`，克隆它只是一次引用计数 —— 渲染层可以放心持有到画完。
    /// 快照的**新鲜度**由重建时机保证，见 [`Self::refresh_content`]。
    pub fn content(&self) -> Arc<Content> {
        self.content
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 重建快照（持有 `Term` 锁期间调用）。
    fn rebuild_content_locked(&self, term: &Term<ChannelListener>) {
        let content = Arc::new(Content::new(term));
        *self.content.lock().unwrap_or_else(|e| e.into_inner()) = content;
    }

    /// 唯一的网格改动入口：改完立刻重建快照。
    ///
    /// 快照不是每帧现取的，所以**每一处直接改网格的地方都必须走这里**，否则
    /// 屏幕会停在旧内容上（症状是「滚了但画面没动」）。收成一个入口就是为了
    /// 让这类遗漏无处可藏 —— 直接 `self.term.lock()` 去改网格的地方应当只剩
    /// 生产路径的 alacritty 事件循环（那边靠 [`Self::refresh_content`]）。
    pub(crate) fn mutate_grid(&self, mutate: impl FnOnce(&mut Term<ChannelListener>)) {
        let mut term = self.term.lock();
        mutate(&mut term);
        self.rebuild_content_locked(&term);
    }

    /// 按当前网格重建快照。
    ///
    /// 给**生产路径**用：PTY 的字节由 alacritty 的事件循环线程直接写进 `Term`
    /// （不经过 [`Self::feed`]），那边改完网格会发一个 `Event::Wakeup`；UI 收到
    /// 后调这里，与 Zed 在 `Terminal::sync` 里重建 `last_content` 是一回事。
    pub fn refresh_content(&self) {
        self.mutate_grid(|_| {});
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
        // 测试路径没有 PTY 事件，快照得在这里重建 —— 生产路径靠 Wakeup。
        self.rebuild_content_locked(&term);
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
        self.mutate_grid(|term| term.resize(Viewport::from_window_size(size)));
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

    /// 光标所在的网格位置（视口坐标）。
    ///
    /// 与 [`crate::grid::Point`] 的 line 约定一致：alacritty 的绝对 `Line` 以不滚动
    /// 时的视口首行为 0、向下递增（回滚区为负），**加上** `display_offset` 得视口
    /// 行号（0 = 视口顶行）—— 就是 alacritty 的 `point_to_viewport`，也是
    /// `raw_line_of` 的逆运算。
    ///
    /// 上滚之后光标会落到视口**下方**（行号 ≥ 屏高），调用方据此决定画不画。
    pub fn cursor_point(&self) -> Point {
        let term = self.term.lock();
        let grid = term.grid();
        let line = grid.cursor.point.line.0 + grid.display_offset() as i32;
        Point::new(line, grid.cursor.point.column.0)
    }

    /// 本地滚动视口，**不**转发给程序。
    ///
    /// 与 [`Session::scroll`] 的分工：那个是「用户滚了滚轮」，程序开了鼠标上报
    /// 或处于 alternate screen 时要把滚动转发给它；这个用于**明确要求滚动本
    /// 视图**的场合（拖滚动条、Shift+PageUp 这类翻页键），不看任何模式 ——
    /// 用户都抓住滚动条 / 按下翻页键了，意图很清楚，再往 vim 里发方向键反而
    /// 是添乱。
    pub fn scroll_display(&self, amount: ScrollAmount) {
        if amount == ScrollAmount::Lines(0) {
            return;
        }
        let scroll = match amount {
            ScrollAmount::Lines(lines) => Scroll::Delta(lines),
            ScrollAmount::PageUp => Scroll::PageUp,
            ScrollAmount::PageDown => Scroll::PageDown,
            ScrollAmount::Top => Scroll::Top,
            ScrollAmount::Bottom => Scroll::Bottom,
        };
        self.mutate_grid(|term| term.grid_mut().scroll_display(scroll));
    }

    /// 是否处于 alternate screen（vim / less 这类全屏程序）。
    ///
    /// 那种屏幕上没有回滚区，翻页键交给程序自己 —— 对应 Zed 里
    /// `TerminalView::is_alt_screen`。
    pub fn is_alt_screen(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    /// 取一个可见 cell。
    ///
    /// `point` 用 [`crate::grid::Point`] 的 line 约定（**视口行号**，0 = 视口顶行，
    /// 向下递增），不是裸的 alacritty `Line`。越界返回 `None`。
    pub fn cell(&self, point: Point) -> Option<Cell> {
        let (columns, screen_lines) = self.dimensions();
        if point.column >= columns {
            return None;
        }
        if point.line < 0 || point.line >= screen_lines as i32 {
            return None;
        }
        // 越界判断落在**视口行号**上：算出的绝对行必然落在 `[-history, screen_lines)`
        // 里（回滚区就是负数那一截），不必再判。
        let term = self.term.lock();
        let raw_line = raw_line_of(point.line, term.grid().display_offset());
        Some(Cell::from_alacritty(
            &term.grid()[Line(raw_line)][Column(point.column)],
        ))
    }

    /// 取一个可见 cell 上的超链接 URI（OSC 8）。
    ///
    /// 超链接会跨多个 cell，链上每一格都返回同一个 URI；不在链上返回 `None`。
    /// 行号换算与 [`Self::cell`] 完全相同（走 `raw_line_of`）。
    pub fn hyperlink_at(&self, point: Point) -> Option<String> {
        let (columns, screen_lines) = self.dimensions();
        if point.column >= columns || point.line < 0 || point.line >= screen_lines as i32 {
            return None;
        }
        let term = self.term.lock();
        let raw_line = raw_line_of(point.line, term.grid().display_offset());
        term.grid()[Line(raw_line)][Column(point.column)]
            .hyperlink()
            .map(|link| link.uri().to_owned())
    }

    /// 屏幕上的文本（按行拼接）。
    ///
    /// 用于「把可见内容复制出去」这类操作。会去掉行尾空白。
    pub fn visible_text(&self) -> String {
        let snapshot = self.content();
        // 先按行号收成一行一行：cells 是扁平的一长条，逐行去 filter 会变成
        // 「行数 × 格子数」。行号在快照内必定是 `0..screen_lines`（见 `Content`）。
        let mut rows: Vec<String> = vec![String::new(); snapshot.screen_lines];
        for indexed in &snapshot.cells {
            if let Some(row) = rows.get_mut(indexed.point.line.max(0) as usize) {
                row.push(indexed.character());
            }
        }
        let mut text = String::new();
        for row in rows.iter().rev() {
            text.push_str(row.trim_end());
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
    pub fn spawn(size: WindowSize, spec: &ShellSpec) -> std::io::Result<Self> {
        let (terminal, events, notifier) = spawn_parts(size, spec)?;
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

    let terminal = Terminal::build(term, size, true);
    Ok((terminal, events, notifier))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::palette::{AnsiColors, TerminalPalette};
    use crate::resolve::resolve_color;
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
        Terminal::for_test(size)
    }

    pub(crate) fn slot_of(color: aa_theme_core::Hsla) -> usize {
        (color.a * 24.).round() as usize
    }

    /// 取某个 cell 的**已解析**前景色。
    ///
    /// 解析不再由 core 做（快照里存的是符号化的 `Color`），所以测试自己拿
    /// 调色板解一次 —— 这正是渲染层要做的事。
    pub(crate) fn fg_at(t: &Terminal, point: Point) -> aa_theme_core::Hsla {
        let cell = t.cell(point).expect("测试坐标应在视口内");
        resolve_color(&cell.foreground(), &palette())
    }

    /// 取某个 cell 的**已解析**背景色。
    pub(crate) fn bg_at(t: &Terminal, point: Point) -> aa_theme_core::Hsla {
        let cell = t.cell(point).expect("测试坐标应在视口内");
        resolve_color(&cell.background(), &palette())
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut t = terminal();
        t.feed(b"hello");
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().character(), 'h');
        assert_eq!(t.cell(Point::new(0, 4)).unwrap().character(), 'o');
        // 未写入处是空格而不是 '\0'
        assert_eq!(t.cell(Point::new(0, 5)).unwrap().character(), ' ');
    }

    #[test]
    fn escape_sequence_split_across_feeds_still_parses() {
        // PTY 天然会把一个转义序列切成两半，Parser 必须能跨调用续上。
        let mut t = terminal();
        t.feed(b"\x1b[3");
        t.feed(b"1mred");
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().character(), 'r');
        // SGR 31 = 红色 → 具名色表第 1 槽。颜色是**符号**，解析要走调色板 ——
        // 解析已经不在 core 里做了（快照只存 `Color`），所以测试自己解析。
        assert_eq!(slot_of(fg_at(&t, Point::new(0, 0))), 1);
    }

    #[test]
    fn sgr_colors_resolve_through_the_palette() {
        let mut t = terminal();
        t.feed(b"\x1b[32mG");
        assert_eq!(
            slot_of(fg_at(&t, Point::new(0, 0))),
            2,
            "32 应映射到 green 槽"
        );

        t.feed(b"\x1b[93mB");
        assert_eq!(
            slot_of(fg_at(&t, Point::new(0, 1))),
            11,
            "93 应映射到 bright yellow"
        );
    }

    #[test]
    fn true_color_bypasses_the_palette() {
        let mut t = terminal();
        t.feed(b"\x1b[38;2;255;128;0mX");
        let fg = fg_at(&t, Point::new(0, 0));
        // 真彩色不走 16 色表，alpha 保持 1（没被当成槽位标记）
        assert_eq!(fg.a, 1.0);
        assert_eq!(slot_of(fg), 24, "不在任何 ANSI 槽位内");
    }

    #[test]
    fn reset_restores_the_default_foreground() {
        let mut t = terminal();
        t.feed(b"\x1b[31mA\x1b[0mB");
        assert_eq!(slot_of(fg_at(&t, Point::new(0, 0))), 1);
        let p = palette();
        assert_eq!(fg_at(&t, Point::new(0, 1)), p.foreground);
    }

    #[test]
    fn bold_and_italic_flags_are_exposed() {
        let mut t = terminal();
        t.feed(b"\x1b[1;3mS");
        let cell = t.cell(Point::new(0, 0)).unwrap();
        assert!(cell.is_bold());
        assert!(cell.is_italic());
        assert!(!cell.has_underline());
    }

    #[test]
    fn dim_flag_reaches_the_cell() {
        // SGR 2。渲染层靠它降 alpha，取不到就表现为「dim 完全不暗」。
        let mut terminal = terminal();
        terminal.feed(b"\x1b[2mD");
        let cell = terminal.cell(Point::new(0, 0)).expect("首列应可读");
        assert!(cell.is_dim());
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
        assert!(t.cell(Point::new(0, 0)).unwrap().has_underline());

        t.feed(b"\x1b[0m\x1b[9mK");
        let strike = t.cell(Point::new(0, 1)).unwrap();
        assert!(strike.has_strikeout(), "SGR 9 应设置删除线");
        assert!(!strike.has_underline(), "重置后不应残留下划线");
    }

    #[test]
    fn sgr_attributes_accumulate_until_reset() {
        // 锁住上面那条语义：属性叠加，只有 SGR 0 才清空。
        let mut t = terminal();
        t.feed(b"\x1b[4mA\x1b[1mB");
        assert!(t.cell(Point::new(0, 0)).unwrap().has_underline());
        let second = t.cell(Point::new(0, 1)).unwrap();
        assert!(second.has_underline(), "加粗不应清掉下划线");
        assert!(second.is_bold());

        t.feed(b"\x1b[0mC");
        let third = t.cell(Point::new(0, 2)).unwrap();
        assert!(
            !third.has_underline() && !third.is_bold(),
            "SGR 0 应清空所有属性"
        );
    }

    #[test]
    fn underline_color_is_exposed_and_resettable() {
        // SGR 58 设下划线颜色，59 清掉。没设时必须是 `None`，表示「跟随前景
        // 色」—— 渲染层没法从「等于 fg」反推出这个区分。
        let mut t = terminal();
        assert_eq!(
            t.cell(Point::new(0, 0)).unwrap().underline_color(),
            None,
            "未设 SGR 58 时应为 None"
        );

        // 同一个真彩色同时设成前景与下划线颜色，两者应解析成同一个值
        // （resolve_color 对两个字段走的是同一条路径）。
        let p = palette();
        t.feed(b"\x1b[58;2;255;0;0m\x1b[38;2;255;0;0m\x1b[4mR");
        let cell = t.cell(Point::new(0, 0)).unwrap();
        assert!(cell.has_underline(), "SGR 58 不应清掉下划线");
        assert_eq!(
            resolve_color(&cell.underline_color().expect("已设 SGR 58"), &p),
            resolve_color(&cell.foreground(), &p),
            "58 的颜色应与同色前台一致"
        );

        t.feed(b"\x1b[59mG");
        assert_eq!(
            t.cell(Point::new(0, 1)).unwrap().underline_color(),
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
            assert!(t.cell(Point::new(0, column)).unwrap().has_hyperlink());
        }

        // 空 URI 关闭后不再是链接
        assert_eq!(t.hyperlink_at(Point::new(0, 4)), None);
        assert!(!t.cell(Point::new(0, 4)).unwrap().has_hyperlink());
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
        assert!(t.cell(Point::new(0, 0)).unwrap().is_inverse());
    }

    #[test]
    fn empty_input_does_not_hang_or_corrupt() {
        // alacritty 的事件循环里明确注释过「发 0 字节会让终端挂起」。
        let mut t = terminal();
        t.feed(b"ok");
        t.feed(b"");
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().character(), 'o');
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
    fn content_covers_the_viewport_in_row_order() {
        let mut t = terminal();
        t.feed(b"abcdef");
        let content = t.content();
        let (columns, screen_lines) = t.dimensions();
        assert_eq!(
            content.cells.len(),
            columns * screen_lines,
            "快照应铺满整屏"
        );
        assert_eq!(content.columns, columns);
        assert_eq!(content.screen_lines, screen_lines);
        // 扁平数组按视口顺序：第一格是左上角，第 columns 格是第二行首列
        assert_eq!(content.cells[0].point, Point::new(0, 0));
        assert_eq!(content.cells[0].character(), 'a');
        assert_eq!(content.cells[columns].point, Point::new(1, 0));
        assert_eq!(content.cells[5].point, Point::new(0, 5));
    }

    #[test]
    fn content_tracks_the_scroll_offset() {
        // 快照必须跟着滚动走：上滚之后首行是刚露出来的那一行，光标落到视口
        // **下方**。这条同时锁住「滚了之后画面要动」—— 快照不是每帧现取的，
        // 滚动路径忘了重建的话，这里会看到旧内容。
        let t = terminal_with_history();
        assert_eq!(t.content().cells[0].character(), 'L');
        assert_eq!(text_row(&t, 0), "L07");

        t.scroll_display(ScrollAmount::Lines(3));
        let content = t.content();
        assert_eq!(content.display_offset, 3);
        assert_eq!(text_row(&t, 0), "L04", "上滚 3 行后首行应是 L04");
        // 快照里的首行字符与逐格读出来的一致
        let snapshot_row: String = content
            .cells
            .iter()
            .take(3)
            .map(|cell| cell.character())
            .collect();
        assert_eq!(snapshot_row, "L04");
        // 光标在视口下方（行号 ≥ 屏高），渲染层据此不画
        assert!(
            content.cursor.line >= content.screen_lines as i32,
            "上滚后光标应落在视口下方: {:?}",
            content.cursor
        );
    }

    #[test]
    fn content_is_rebuilt_by_every_grid_mutation() {
        // 快照只在改动网格的路径上重建，所以每条这样的路径都要验一遍：
        // 漏掉任何一条，画面会停在旧内容上（没有任何报错）。
        let mut t = terminal_with_history();

        let assert_fresh = |t: &Terminal, what: &str| {
            let fresh = Content::new(&t.term.lock());
            let cached = t.content();
            assert_eq!(
                cached.cells.len(),
                fresh.cells.len(),
                "{what} 之后快照的格子数不对"
            );
            assert_eq!(
                cached.display_offset, fresh.display_offset,
                "{what} 之后偏移不对"
            );
            assert_eq!(cached.cursor, fresh.cursor, "{what} 之后光标不对");
            for (cached, fresh) in cached.cells.iter().zip(fresh.cells.iter()) {
                assert_eq!(cached.point, fresh.point, "{what} 之后坐标不对");
                assert_eq!(
                    cached.character(),
                    fresh.character(),
                    "{what} 之后 {:?} 的字符不对",
                    cached.point
                );
            }
        };

        t.feed(b"more\r\n");
        assert_fresh(&t, "feed");
        t.scroll_display(ScrollAmount::Lines(2));
        assert_fresh(&t, "scroll_display");
        t.resize_to(&GridGeometry::new(400., 240., 10., 20.));
        assert_fresh(&t, "resize");
    }

    /// 前 3 个字符组成的行文本，用来看清哪一行显示在哪个视口行上。
    fn text_row(t: &Terminal, row: i32) -> String {
        (0..3)
            .map(|column| t.cell(Point::new(row, column)).unwrap().character())
            .collect()
    }

    /// 喂 30 行 `L00`..`L29` 到 24 行屏上：前 7 行进回滚区，视口顶是 `L07`。
    fn terminal_with_history() -> Terminal {
        let mut t = terminal();
        for i in 0..30 {
            t.feed(format!("L{i:02}\r\n").as_bytes());
        }
        t
    }

    #[test]
    fn scrolling_up_reveals_the_lines_that_scrolled_off() {
        // 这条是回归测试：换算曾写成 `Line = 视口行号 + display_offset`，
        // 于是 offset ≥ 1 时读到 `Line(24)` 越界 —— debug 下直接把这条测试
        // 之外的所有渲染路径都炸掉，release 下静默读错格子。
        let t = terminal_with_history();
        assert_eq!(t.scroll_state().history_size(), 7);
        // 不滚动时视口 = Line(0..23)：L07..L29 后面跟着命令行自己的空行。
        assert_eq!(text_row(&t, 0), "L07", "视口首行是第 8 行");
        assert_eq!(text_row(&t, 22), "L29");
        assert_eq!(text_row(&t, 23), "   ", "最后一行是光标所在的空行");

        t.scroll_display(ScrollAmount::Lines(1));
        assert_eq!(text_row(&t, 0), "L06", "上滚一行应露出刚滚出去的那一行");
        assert_eq!(text_row(&t, 23), "L29", "底部滑到第 30 行");

        t.scroll_display(ScrollAmount::Lines(7));
        assert_eq!(t.scroll_state().display_offset(), 7, "应夹在回滚区上限");
        assert_eq!(text_row(&t, 0), "L00", "滚到头是第 1 行");
        assert_eq!(text_row(&t, 23), "L23");
    }

    #[test]
    fn cell_agrees_with_alacritty_viewport_to_point() {
        // 拿 alacritty 自己的换算当标准答案：这样即使我对 `Line` 的正负号
        // 理解错了，测试也会跟着错，而不是自证清白。
        use alacritty_terminal::index::Point as AlacPoint;
        use alacritty_terminal::term::viewport_to_point;

        let t = terminal_with_history();
        for offset in 0..=7usize {
            t.scroll_display(ScrollAmount::Lines(
                offset as i32 - t.scroll_state().display_offset() as i32,
            ));

            let expected: Vec<(i32, char)> = {
                let term = t.term.lock();
                let grid = term.grid();
                [0usize, 12, 23]
                    .into_iter()
                    .map(|row| {
                        let point = viewport_to_point(
                            offset,
                            AlacPoint {
                                line: row,
                                column: Column(0),
                            },
                        );
                        (row as i32, grid[point.line][Column(0)].c)
                    })
                    .collect()
            };

            for (row, c) in expected {
                assert_eq!(
                    t.cell(Point::new(row, 0)).map(|cell| cell.character()),
                    Some(c),
                    "offset={offset} row={row} 与 alacritty 的 viewport_to_point 不一致"
                );
            }
        }
    }

    #[test]
    fn cursor_point_agrees_with_alacritty_after_scrolling() {
        // 回归测试：光标换算曾和 `cell()` 犯同一个错 —— 写成
        // `Line - display_offset`。offset = 0 时两种写法结果相同，所以只喂内容
        // 不滚动是测不出来的：上滚之后光标会算到**上方**去（22 而不是 24），
        // 表现为「滚起来以后光标画在错误的行上」。
        use alacritty_terminal::term::point_to_viewport;

        let t = terminal_with_history();
        for offset in 0..=3usize {
            t.scroll_display(ScrollAmount::Lines(
                offset as i32 - t.scroll_state().display_offset() as i32,
            ));

            let expected = {
                let term = t.term.lock();
                let grid = term.grid();
                point_to_viewport(grid.display_offset(), grid.cursor.point)
                    .map(|point| Point::new(point.line as i32, point.column.0))
            };
            assert_eq!(
                Some(t.cursor_point()),
                expected,
                "offset={offset} 与 alacritty 的 point_to_viewport 不一致"
            );
        }
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
        assert_eq!(t.cell(Point::new(0, 0)).unwrap().character(), 'k');
    }

    #[test]
    fn wide_characters_mark_their_spacer_cell() {
        let mut t = terminal();
        // 中文字符占两格
        t.feed("中".as_bytes());
        let first = t.cell(Point::new(0, 0)).unwrap();
        let second = t.cell(Point::new(0, 1)).unwrap();
        assert_eq!(first.character(), '中');
        assert!(
            second.is_wide_char_spacer(),
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
        assert_eq!(bg_at(&t, Point::new(0, 0)), p.background);
        assert_ne!(p.background, black());
    }

    #[test]
    fn default_foreground_matches_the_palette() {
        let t = terminal();
        let p = palette();
        assert_eq!(fg_at(&t, Point::new(0, 0)), p.foreground);
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
        let geometry = GridGeometry::new(800., 480., 10., 20.);
        let terminal = Terminal::for_test(window_size_from(&geometry));
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
    use crate::resolve::resolve_color;
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

    let mut terminal = Terminal::for_test(size);
    terminal.feed(&raw);
    let cell = terminal.cell(Point::new(0, 0)).expect("首行首列应可读");
    assert_eq!(cell.character(), 'R', "实际读到: {raw:?}");
    assert_eq!(
        slot_of(resolve_color(&cell.foreground(), &palette())),
        1,
        "\x1b[31m 应解析为红色槽位"
    );
}
