//! 应用入口 —— 把 `aa_terminal_core` 接到 iced 的消息系统上。
//!
//! ## 数据流
//!
//! ```text
//! PTY → alacritty 事件循环线程 → Term (FairMutex 共享)
//!          └→ ChannelListener → channel → Subscription → Message::Event → 重绘
//! UI → Notifier::notify        → PTY（按键转发）
//! UI → Msg::Resize             → PTY（尺寸同步）
//! ```
//!
//! PTY 的读取与解析全在 alacritty 的线程里完成，**这个进程里没有第二个读循环**。
//! 早先那版自己起线程手动 `read()`，漏了三件事：主端是非阻塞的、子进程退出要走
//! SIGCHLD、resize 与读取要保持同一顺序。alacritty 的 `EventLoop` 全管了。
//!
//! UI 侧只在绘制时短暂锁 `Term`（`Terminal::rows` 内锁一次）。长时间持锁会把
//! 事件循环卡住 —— 那是「整屏不刷新」的直接原因。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aa_terminal_core::grid::{GridGeometry, Rounding};
use aa_terminal_core::session::{ShellSpec, window_size_from};
use aa_terminal_core::{
    AnsiColors, BlinkEpoch, BlinkState, RESUME_AFTER_PAUSING, ScrollAmount, Session, Terminal,
    TerminalPalette,
};
use aa_terminal_core::{PtyEvent, TerminalEvent as Event, WindowSize};
use iced::keyboard::Key;
use iced::{Subscription, Task, event, window};

/// 初始视口（像素）。
const INITIAL_WIDTH: f32 = 900.;
const INITIAL_HEIGHT: f32 = 600.;

/// 会话。
///
/// `Subscription::run` 的参数是 `fn() -> S`，不带状态，所以会话只能通过进程级
/// 静态量递出来。
///
/// 外面还要一层 `Mutex`：`Session::resize` / `take_events` 都要 `&mut self`，
/// 而 `OnceLock::get()` 只给 `&`。`iced` 的 update 与订阅都在同一线程上，
/// 这把锁实际没有竞争，只是为了让类型过得去。
static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

struct App {
    /// 与 `SESSION` 里同一个 `Terminal`（共享同一个 `Term`）。
    terminal: &'static Terminal,
    palette: TerminalPalette,
    geometry: GridGeometry,
    /// 视口像素尺寸。canvas 铺满窗口，它同时就是画布 bounds —— 滚动条要拿它
    /// 算轨道。**不能**用 `geometry.width` 顶替：窗口过窄时 `geometry_for` 会把
    /// 宽度垫到 2 个 cell，那时两者不一样。
    viewport: iced::Size,
    /// 实测出来的 cell 尺寸，渲染与几何换算共用一份。
    metrics: aa_terminal_iced::CellMetrics,
    /// shell 是否已退出。
    exited: bool,
    /// 光标闪烁状态机：相位、暂停，以及防过期定时器用的 epoch。
    blink: BlinkState,
    /// 最近一次光标位置。iced 的 `ButtonPressed` 不带坐标，只能自己记。
    last_cursor: Option<iced::Point>,
    /// 左键是否按住（决定移动是拖选还是单纯移动）。
    dragging: bool,
    /// 左键是否按住**滚动条**（决定移动是拖滚动条还是拖选）。
    scrollbar_drag: bool,
    /// 拖滚动条时指针距拇指**顶端**的像素距离（按下时记下）。
    ///
    /// 同 Zed：抓在拇指上就保持抓点不变，拇指不会在按下那一下跳走。
    scrollbar_grab: f32,
    /// 搜索框：`None` 为关闭。打开时保存查询词与命中数。
    search: Option<SearchState>,
    /// 当前修饰键状态。
    ///
    /// iced 只把 `modifiers` 挂在键盘事件上，鼠标事件不带 —— Ctrl+click 判定
    /// 只能靠这里缓存。
    modifiers: iced::keyboard::Modifiers,
    /// Ctrl+按下时命中的链接 URI。松手还落在同一链接上才打开。
    ///
    /// 与 Zed 的 `mouse_down_hyperlink` 同一语义：按下与松开要是**同一目标**，
    /// 中途拖开就作罢。
    pressed_link: Option<String>,
    /// 经由 `opener` 打开过的 URL。
    ///
    /// 生产环境里没人读它（量很小），它存在的意义是让测试能断言「打开的是哪个
    /// 链接」，而不用真的弹出一个浏览器。
    opened_links: Vec<String>,
    /// 真正去打开 URL 的动作。默认 spawn 系统 opener，测试注入空实现。
    opener: fn(&str),
}

/// 搜索框状态。
struct SearchState {
    /// 当前查询词。
    query: String,
    /// 命中数，由 `Terminal::set_search` 返回。
    matches: usize,
    /// 当前跳转到的命中下标；还没跳转时为 `None`。
    current: Option<usize>,
}

/// 光标闪烁间隔。
const CURSOR_BLINK: Duration = Duration::from_millis(550);

/// 应用消息。
#[derive(Debug, Clone)]
enum Message {
    /// alacritty 的事件（标题变化、响铃、退出……）。
    Event(Event),
    /// 窗口尺寸变化。
    Resized(iced::Size),
    /// 一次按键。
    Key(iced::keyboard::Event),
    /// 剪贴板读回来的内容（从 Message::Paste 得到）。
    Pasted(String),
    /// 打开搜索框（Ctrl+Shift+F）。
    OpenSearch,
    /// 搜索词变了（输入框每次编辑都会发）。
    SearchInput(String),
    /// 跳到下一处命中（回车）。
    SearchNext,
    /// 跳到上一处命中（Shift+回车）。
    SearchPrev,
    /// 关闭搜索框并清掉高亮。
    CloseSearch,
    /// 光标该翻转相位了（由 `CURSOR_BLINK` 周期订阅发出）。
    BlinkTick,
    /// 暂停期结束，可以重新闪了；`epoch` 用来丢弃连打时排下的过期定时器。
    BlinkResume(BlinkEpoch),
    /// 光标移动（既更新缓存也可能在拖选）。
    CursorMoved(iced::Point),
    /// 左键按下。
    MouseDown,
    /// 左键松开。
    MouseUp,
    /// 滚轮滚动。
    Wheel(iced::mouse::ScrollDelta),
    /// 翻页键要求滚动本视图（Shift+↑/↓/PageUp/PageDown/Home/End）。
    ScrollView(ScrollAmount),
    /// 把当前选区写进剪贴板。
    Copy,
    /// 读剪贴板并发给 shell。
    Paste,
}

fn main() -> iced::Result {
    let mut app = iced::application(App::boot, App::update, App::view)
        .title("aa_term")
        .window(window::Settings {
            size: iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT),
            ..Default::default()
        })
        .subscription(App::subscription)
        // `Font` 只是 family/weight/style 的**描述**，字体文件要另行注册。
        .default_font(iced::Font::MONOSPACE);

    // 等宽字体注册：iced 自带比例字体，不换会导致列对不齐。
    //
    // **必须注册整套字族**而不是只注册 Regular：bold/italic 是靠换字形文件
    // 实现的，只给 Regular 的话 cosmic-text 会合成一个假粗体（advance 与
    // 原体完全相同，视觉上 flag 像没生效）。aa_terminal_iced 的
    // bold_and_italic_map_to_distinct_fonts 测的就是这件事。
    for bytes in load_mono_family() {
        app = app.font(bytes);
    }

    app.run()
}

/// 加载等宽字体的四个变体。
///
/// 找不到就返回空：iced 会回退到系统等宽字体，网格仍对得齐，只是没有
/// bold/italic 变体。
fn load_mono_family() -> Vec<Vec<u8>> {
    const BASE: [(&str, [&str; 4]); 3] = [
        (
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont",
            ["-Regular", "-Bold", "-Italic", "-BoldItalic"],
        ),
        (
            "/usr/share/fonts/TTF/DejaVuSansMono",
            ["", "-Bold", "-Oblique", "-BoldOblique"],
        ),
        (
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFontMono",
            ["-Regular", "-Bold", "-Italic", "-BoldItalic"],
        ),
    ];
    for (stem, suffixes) in BASE {
        let family: Vec<Vec<u8>> = suffixes
            .iter()
            .filter_map(|suffix| std::fs::read(format!("{stem}{suffix}.ttf")).ok())
            .collect();
        if !family.is_empty() {
            return family;
        }
    }
    Vec::new()
}

/// 默认调色板。
///
/// 临时用固定十六进制配色，等 `aa_theme_iced` 建好后由主题层注入。
fn palette() -> TerminalPalette {
    use aa_terminal_core::resolve::rgb;
    use aa_theme_core::black;

    TerminalPalette {
        background: rgb(0x1e, 0x1e, 0x2e),
        foreground: rgb(0xcd, 0xd6, 0xf4),
        bright_foreground: rgb(0xf5, 0xe0, 0xdc),
        dim_foreground: rgb(0xa6, 0xad, 0xc4),
        ansi_background: rgb(0x1e, 0x1e, 0x2e),
        ansi: AnsiColors {
            normal: [
                rgb(0x45, 0x47, 0x5a),
                rgb(0xf3, 0x8b, 0xa8),
                rgb(0xa6, 0xe3, 0xa1),
                rgb(0xf9, 0xe2, 0xaf),
                rgb(0x89, 0xb4, 0xfa),
                rgb(0xf5, 0xc2, 0xe7),
                rgb(0x94, 0xe2, 0xd5),
                rgb(0xb6, 0xbf, 0xfb),
            ],
            bright: [
                rgb(0x58, 0x5b, 0x70),
                rgb(0xf3, 0x8b, 0xa8),
                rgb(0xa6, 0xe3, 0xa1),
                rgb(0xf9, 0xe2, 0xaf),
                rgb(0x89, 0xb4, 0xfa),
                rgb(0xf5, 0xc2, 0xe7),
                rgb(0x94, 0xe2, 0xd5),
                rgb(0xcd, 0xd6, 0xf4),
            ],
            dim: [black(); 8],
        },
        cursor: rgb(0xf5, 0xe0, 0xdc),
        selection_background: rgb(0x45, 0x47, 0x5a),
        selection_foreground: rgb(0xcd, 0xd6, 0xf4),
        search_match_background: rgb(0x45, 0x47, 0x5a),
        search_active_match_background: rgb(0xf9, 0xe2, 0xaf),
        scrollbar_thumb: rgb(0x58, 0x5b, 0x70),
    }
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let palette = palette();
        // 必须先实测再算尺寸：cell 宽不是比例常数，猜的值会让整屏列对不齐。
        // Zed 用 text_system.advance(font, size, 'm')，iced 上用
        // Paragraph::with_text("m").min_bounds()，同一个思想。
        let metrics = aa_terminal_iced::CellMetrics::measure(
            iced::Font::MONOSPACE,
            aa_terminal_iced::BASE_FONT_SIZE,
        );
        let geometry =
            aa_terminal_iced::geometry_for(iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT), metrics);
        let size = window_size_from(&geometry);

        // 终端必须是 `'static`：`Subscription::run` 只吃 `fn() -> S`，会话只能
        // 从静态量里取，渲染用的 Terminal 因此也得活得一样久。
        // 终端实例装进 core 的进程静态量；失败时退回一个不起 PTY 的终端，
        // 至少窗口不是全白且 stderr 有线索。
        let booted = Session::spawn(size, &ShellSpec::new());
        let terminal = match booted {
            Ok(session) => match SESSION.set(Mutex::new(session)) {
                Ok(()) => SESSION
                    .get()
                    .map(|session| session.lock().unwrap_or_else(|e| e.into_inner()).terminal())
                    .unwrap_or_else(|| fallback_terminal(size)),
                Err(rejected) => {
                    // 已经有一个会话了（理论上不会发生：boot 只跑一次）。
                    eprintln!("aa_term: 已有会话，丢弃新会话");
                    let _ = rejected.into_inner().unwrap_or_else(|p| p.into_inner());
                    fallback_terminal(size)
                }
            },
            Err(err) => {
                eprintln!("aa_term: 启动 shell 失败: {err}");
                fallback_terminal(size)
            }
        };

        (
            Self {
                terminal,
                palette,
                geometry,
                viewport: iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT),
                metrics,
                exited: false,
                blink: blinking(),
                last_cursor: None,
                dragging: false,
                scrollbar_drag: false,
                scrollbar_grab: 0.0,
                search: None,
                modifiers: iced::keyboard::Modifiers::default(),
                pressed_link: None,
                opened_links: Vec::new(),
                opener: spawn_opener,
            },
            Task::none(),
        )
    }

    /// 有输入就把光标稳住、暂停闪烁，并排一个「恢复」定时器。
    ///
    /// 判定放在这里而不是各按键分支里 —— 按键分支有若干提前返回的路径
    /// （搜索框开着、快捷键命中），逐处补丁必漏。
    fn update(&mut self, message: Message) -> Task<Message> {
        let paused = match &message {
            Message::Key(iced::keyboard::Event::KeyPressed { .. }) | Message::Pasted(_) => {
                Some(self.blink.pause())
            }
            _ => None,
        };

        let task = self.handle(message);

        match paused {
            Some(epoch) => Task::batch([task, resume_blink_after(epoch)]),
            None => task,
        }
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Event(event) => match event {
                // PTY 有新输出：alacritty 的事件循环**已经**把字节写进网格了
                // （它在读完一批字节后才发 Wakeup），这里重建一份可见内容快照。
                //
                // 快照不是每帧现取的，所以这一条是生产路径上「屏幕会更新」的
                // 唯一来源 —— 对应 Zed 在 `Terminal::sync` 里重建 `last_content`。
                Event::Wakeup => self.terminal.refresh_content(),
                Event::Exit => self.exited = true,
                // 窗口标题；这一版先不接线。
                Event::Title(title) => _ = title,
                // shell 自己要求回写（如 OSC 查询的应答）
                // OSC 查询的应答等：alacritty 把它以 String 交给 UI，
                // 由我们转发回 PTY（终端应答是 ASCII，into_bytes 无损）。
                Event::PtyWrite(bytes) => {
                    if let Some(session) = SESSION.get() {
                        session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write(bytes.into_bytes());
                    }
                }
                _ => {}
            },
            Message::Key(event) => {
                // 持续记录修饰键状态：鼠标事件不带 modifiers，Ctrl+click 只能
                // 靠键盘事件缓存下来。KeyReleased 也要更新 —— 松开 Ctrl 本身
                // 会走这里。
                match &event {
                    iced::keyboard::Event::KeyPressed { modifiers, .. }
                    | iced::keyboard::Event::KeyReleased { modifiers, .. } => {
                        self.modifiers = *modifiers;
                    }
                    iced::keyboard::Event::ModifiersChanged(modifiers) => {
                        self.modifiers = *modifiers;
                    }
                }
                if let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = &event {
                    // 搜索框打开时按键归输入框。这里只留 Esc（关搜索），其余一律
                    // 不发往 shell —— 否则用户敲的搜索词会被当成终端输入执行。
                    // 输入框自身通过 on_input 拿到字符，不经过这里。
                    if self.search.is_some() {
                        if key.as_ref() == Key::Named(iced::keyboard::key::Named::Escape) {
                            return self.update(Message::CloseSearch);
                        }
                        // 回车在搜索框里是「跳到下一处」，这是搜索最常用的操作。
                        // 在这里拦而不是用 `text_input` 的 `on_submit`：后者拿不到
                        // Shift，区分不出上一处。
                        if key.as_ref() == Key::Named(iced::keyboard::key::Named::Enter) {
                            return self.update(if modifiers.shift() {
                                Message::SearchPrev
                            } else {
                                Message::SearchNext
                            });
                        }
                        return Task::none();
                    }
                    // app_cursor 必须从 Term 现读：shell 随时可能切换它
                    // （vim 进入正常模式就会），缓存会发错方向序列。
                    let app_cursor = self.terminal.app_cursor_mode();
                    // 应用级快捷键必须先于按键编码拦掉 —— 否则会被当普通键发进
                    // shell（Ctrl+Shift+C 想复制，结果发了 \x03 中断）。判定逻辑
                    // 在 core 里且带测试，别在这里重写。
                    if let Some(shortcut) =
                        aa_terminal_core::mappings::keys::app_shortcut(key, *modifiers)
                    {
                        use aa_terminal_core::mappings::keys::AppShortcut;
                        return match shortcut {
                            AppShortcut::Copy => self.update(Message::Copy),
                            AppShortcut::Paste => self.update(Message::Paste),
                            AppShortcut::OpenSearch => self.update(Message::OpenSearch),
                            AppShortcut::ScrollLineUp => {
                                self.update(Message::ScrollView(ScrollAmount::Lines(1)))
                            }
                            AppShortcut::ScrollLineDown => {
                                self.update(Message::ScrollView(ScrollAmount::Lines(-1)))
                            }
                            AppShortcut::ScrollPageUp => {
                                self.update(Message::ScrollView(ScrollAmount::PageUp))
                            }
                            AppShortcut::ScrollPageDown => {
                                self.update(Message::ScrollView(ScrollAmount::PageDown))
                            }
                            AppShortcut::ScrollToTop => {
                                self.update(Message::ScrollView(ScrollAmount::Top))
                            }
                            AppShortcut::ScrollToBottom => {
                                self.update(Message::ScrollView(ScrollAmount::Bottom))
                            }
                        };
                    }
                    if let (Some(bytes), Some(session)) = (
                        aa_terminal_core::mappings::keys::encode(key, modifiers, app_cursor, true),
                        SESSION.get(),
                    ) {
                        session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write(bytes.into_owned().into_bytes());
                    }
                }
            }
            Message::BlinkTick => {
                // 周期订阅不会送来过期节拍，拿当前 epoch 恒成立；暂停或关闭时
                // `tick` 返回 None，光标就停在当前相位上不再翻。
                let _ = self.blink.tick(self.blink.epoch());
            }
            Message::BlinkResume(epoch) => {
                // 连打时排下的多个定时器，早到的那个 epoch 已经过期，被 core 丢弃。
                let _ = self.blink.resume(epoch);
            }
            Message::Copy => {
                // 只有真有选中内容时才碰剪贴板：空写会清掉用户已有的内容。
                if let Some(text) = self.terminal.selected_text().filter(|t| !t.is_empty()) {
                    return iced::clipboard::write(text);
                }
            }
            Message::Paste => {
                // 读剪贴板要在 Task 里做：iced 的读是异步的，拿不到值就只能靠
                // 回一条消息。剪贴板为空时不发任何字节。
                return iced::clipboard::read()
                    .map(|text| Message::Pasted(text.unwrap_or_default()));
            }
            Message::OpenSearch => {
                // 保留上次的词，重新打开时不用重打。
                let query = self
                    .search
                    .as_ref()
                    .map(|state| state.query.clone())
                    .unwrap_or_default();
                let matches = self.terminal.set_search(&query);
                self.search = Some(SearchState {
                    query,
                    matches,
                    current: None,
                });
                // 焦点必须主动移过去，否则刚打开的输入框收不到按键。
                return iced::widget::operation::focus(iced::widget::Id::new(
                    aa_terminal_iced::SEARCH_INPUT_ID,
                ));
            }
            Message::SearchInput(query) => {
                let matches = self.terminal.set_search(&query);
                self.search = Some(SearchState {
                    query,
                    matches,
                    current: None,
                });
            }
            Message::SearchNext => {
                self.terminal.search_next();
                self.sync_search_index();
            }
            Message::SearchPrev => {
                self.terminal.search_prev();
                self.sync_search_index();
            }
            Message::CloseSearch => {
                self.search = None;
                // 空查询即清空高亮，与 core 的 set_search("") 约定一致。
                self.terminal.set_search("");
            }
            Message::Pasted(text) => {
                // 只发文字本身：换行按 shell 习惯用 CR，否则多行粘贴会执行前一行。
                if let Some(session) = SESSION.get() {
                    session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .write(text.into_bytes());
                }
            }
            Message::CursorMoved(position) => {
                // 先缓存，ButtonPressed 要用 —— iced 的按键事件不带坐标。
                self.last_cursor = Some(position);
                if self.scrollbar_drag {
                    self.scroll_to_scrollbar(position.y);
                } else if self.dragging {
                    self.terminal
                        .update_selection(position.x, position.y, &self.geometry);
                }
            }
            Message::MouseDown => {
                let Some(position) = self.last_cursor else {
                    self.dragging = true;
                    return Task::none();
                };
                // 滚动条压在内容上方，先判它：落在轨道上就拖滚动条，既不选区也
                // 不开链接。按下即跳（点哪跳哪），不用等拖动。
                if let Some((track, thumb)) = self.scrollbar()
                    && track.contains(position)
                {
                    self.scrollbar_grab = aa_terminal_iced::scrollbar_grab_offset(thumb, position);
                    self.scrollbar_drag = true;
                    self.dragging = false;
                    self.scroll_to_scrollbar(position.y);
                    return Task::none();
                }
                // Ctrl+左键落在超链接上 → 记下来等松手打开（同 Zed 的
                // `mouse_down_hyperlink`）。这一下不开始选区，否则松手会同时
                // 打开链接又留下一片选中。
                if self.modifiers.control()
                    && let Some(uri) = self.hyperlink_at(position)
                {
                    self.pressed_link = Some(uri);
                    self.dragging = false;
                    return Task::none();
                }
                self.dragging = true;
                // 先问程序要不要这个按键。它接管了鼠标（vim 可视模式之类）时
                // 终端自己再选一份就重复了。
                let reported = SESSION.get().is_some_and(|session| {
                    session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .report_mouse_button(
                            iced::mouse::Button::Left,
                            true,
                            iced::keyboard::Modifiers::default(),
                            position.x,
                            position.y,
                            &self.geometry,
                        )
                });
                if reported {
                    self.dragging = false;
                    return Task::none();
                }

                self.terminal.start_selection(
                    aa_terminal_core::SelectionType::Simple,
                    position.x,
                    position.y,
                    &self.geometry,
                );
            }
            Message::MouseUp => {
                let was_scrollbar_drag = self.scrollbar_drag;
                self.scrollbar_drag = false;
                self.dragging = false;
                let Some(position) = self.last_cursor else {
                    return Task::none();
                };
                // 拖滚动条不产生「释放」语义：程序没收到 press，就不该收到 release。
                if was_scrollbar_drag {
                    return Task::none();
                }
                // 松手：只有仍落在按下时的同一链接上才打开。
                if let Some(pressed) = self.pressed_link.take() {
                    if self.hyperlink_at(position).as_deref() == Some(pressed.as_str()) {
                        (self.opener)(&pressed);
                        self.opened_links.push(pressed);
                    }
                    return Task::none();
                }
                // 松键同样先问程序。按住了鼠标 dragging 时它已经接管过一次，
                // 这里补上对应的 release。
                let _ = SESSION.get().is_some_and(|session| {
                    session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .report_mouse_button(
                            iced::mouse::Button::Left,
                            false,
                            iced::keyboard::Modifiers::default(),
                            position.x,
                            position.y,
                            &self.geometry,
                        )
                });
            }
            Message::ScrollView(amount) => {
                // alternate screen（vim / less）上没有回滚区，那是程序的地盘：
                // 与 Zed 一样既不自己滚、也不把翻页键转发过去。
                if !self.terminal.is_alt_screen() {
                    self.terminal.scroll_display(amount);
                }
            }
            Message::Wheel(delta) => {
                // 像素增量按一个 cell 高折算成行数；行增量直接用。
                let (_, line_height) =
                    aa_terminal_iced::cell_size(aa_terminal_iced::BASE_FONT_SIZE);
                let lines = match delta {
                    iced::mouse::ScrollDelta::Lines { y, .. } => -y,
                    iced::mouse::ScrollDelta::Pixels { y, .. } => -y / line_height,
                };
                if let Some(session) = SESSION.get() {
                    session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .scroll(lines.round() as i32);
                }
            }
            Message::Resized(size) => {
                // 先记下窗口尺寸：下面可能因退化而提前返回，视口也要保持同步。
                self.viewport = size;
                let geometry = aa_terminal_iced::geometry_for(size, self.metrics);
                if geometry.num_columns() == 0 || geometry.num_lines() == 0 {
                    // 窗口最小化时尺寸归零。让终端也 resize 到 0 列会让内核
                    // 接受但内容全部错乱，恢复时还要再排一次，所以忽略。
                    return Task::none();
                }
                self.geometry = geometry;
                let window_size = window_size_from(&geometry);

                // PTY 侧走 alacritty 的 channel 而非直接 ioctl：与读取同序。
                if let Some(session) = SESSION.get() {
                    session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .resize(window_size);
                }
            }
        }
        Task::none()
    }

    /// 跳转后把 UI 侧记录的下标同步回 core 的结果。
    ///
    /// 下标由 core 拥有（只有它知道跳到了哪一处），这里只是把结果抄回
    /// `SearchState` 以便显示「i/N」—— 别在 UI 侧自己推算下标，环绕逻辑一改
    /// 就对不上。
    fn sync_search_index(&mut self) {
        if let Some(state) = self.search.as_mut() {
            state.current = self.terminal.search_current_index();
        }
    }

    /// 视口内一个像素位置命中的超链接 URI。
    ///
    /// 换算走 [`GridGeometry::point_at`] 的 `Truncate`，与 core 的选区命中
    /// （`selection.rs`）同一条路径 —— 两处取整方式不一致会变成「明明点在链接
    /// 上却打不开」。
    fn hyperlink_at(&self, position: iced::Point) -> Option<String> {
        let point = self
            .geometry
            .point_at(position.x, position.y, Rounding::Truncate);
        self.terminal.hyperlink_at(point)
    }

    /// 终端画布的矩形。canvas 铺满窗口、原点在左上角，所以就是 `(0,0,viewport)`。
    fn terminal_bounds(&self) -> iced::Rectangle {
        iced::Rectangle::new(iced::Point::ORIGIN, self.viewport)
    }

    /// 当前滚动条的 `(轨道, 拇指)` 矩形；没有回滚区时为 `None`。
    fn scrollbar(&self) -> Option<(iced::Rectangle, iced::Rectangle)> {
        aa_terminal_iced::scrollbar_rects(self.terminal_bounds(), self.terminal.scroll_state())
    }

    /// 把指针的 y 换算成目标偏移并滚过去。
    ///
    /// 只滚**差值**：`scroll_display` 是相对的，而 alacritty 自己会把偏移夹进
    /// 合法范围，这里不必重复实现夹取。
    fn scroll_to_scrollbar(&self, y: f32) {
        let state = self.terminal.scroll_state();
        let Some(target) = aa_terminal_iced::scrollbar_offset_at(
            self.terminal_bounds(),
            state,
            y,
            self.scrollbar_grab,
        ) else {
            return;
        };
        self.terminal.scroll_display(ScrollAmount::Lines(
            target as i32 - state.display_offset() as i32,
        ));
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            pty_events(),
            iced::event::listen_with(on_window_event),
            iced::event::listen_with(on_key_event),
            iced::event::listen_with(on_mouse_event),
            // 光标闪烁：终端起不来也照样闪，否则用户看不出焦点在哪儿。
            iced::time::every(CURSOR_BLINK).map(|_| Message::BlinkTick),
        ])
    }

    fn view(&self) -> iced::Element<'_, Message> {
        let terminal = aa_terminal_iced::view(
            self.terminal,
            &self.palette,
            self.blink.is_visible(),
            self.metrics,
        );

        // 没开搜索就直接给终端，避免白白套一层 stack。
        let Some(search) = &self.search else {
            return terminal;
        };

        // stack 把搜索框叠在终端之上；外层 container 负责把它推到右上角。
        iced::widget::stack![
            terminal,
            iced::widget::container(aa_terminal_iced::search_bar(
                &search.query,
                search.matches,
                search.current,
                Message::SearchInput,
            ))
            .width(iced::Fill)
            .height(iced::Shrink)
            .padding(12)
            .align_x(iced::Right)
        ]
        .into()
    }
}

/// 光标闪烁：目前没有设置项，一律开启。
fn blinking() -> BlinkState {
    let mut blink = BlinkState::new();
    blink.set_enabled(true);
    blink
}

/// `RESUME_AFTER_PAUSING` 之后带着 `epoch` 回来。
///
/// `iced::time` 只公开了周期订阅 `every`，没有一次性定时器，所以直接用树里
/// 那份 tokio —— iced 的 tokio 后端本来就在跑它。
///
/// 注意 `tokio::time::sleep` 一构造就要注册 reactor，放在 `async` 块里包一层
/// 才不会在没跑起运行时的场合（测试）当场 panic；测试里这个 Task 从不被 poll。
fn resume_blink_after(epoch: BlinkEpoch) -> Task<Message> {
    Task::perform(
        async move { tokio::time::sleep(RESUME_AFTER_PAUSING).await },
        move |()| Message::BlinkResume(epoch),
    )
}

/// 起不来 PTY 时的兜底终端。
fn fallback_terminal(size: WindowSize) -> &'static Terminal {
    FALLBACK.get_or_init(|| Terminal::for_test(size))
}

/// 系统打开 URL 用的命令。
#[cfg(target_os = "macos")]
const OPEN_COMMAND: &str = "open";
#[cfg(all(unix, not(target_os = "macos")))]
const OPEN_COMMAND: &str = "xdg-open";
#[cfg(target_os = "windows")]
const OPEN_COMMAND: &str = "cmd";

/// 交给系统默认程序打开一个 URL。
///
/// 与 Zed 在 Linux 上的做法一致（`gpui_linux` 就是 spawn `xdg-open`）：放后台
/// 线程别阻塞 UI，失败只记一行日志 —— 链接打不开不该让终端卡住或崩掉。
fn spawn_opener(uri: &str) {
    let uri = uri.to_owned();
    std::thread::spawn(move || {
        let mut command = std::process::Command::new(OPEN_COMMAND);
        // Windows 的 `cmd` 要靠 `start` 转交给默认程序，空串是窗口标题占位；
        // 另外两个平台直接把 URL 当参数。
        #[cfg(target_os = "windows")]
        command.args(["/C", "start", ""]);
        if let Err(error) = command.arg(&uri).status() {
            eprintln!("aa_term: 打开链接失败: {error}");
        }
    });
}

/// 兜底终端。
static FALLBACK: OnceLock<Terminal> = OnceLock::new();

/// 当前生效的终端代数。Zed 用同一个机制丢弃被替换终端的残留事件。
static CURRENT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// PTY 事件订阅。
fn pty_events() -> Subscription<Message> {
    use iced::futures::StreamExt;
    Subscription::run(|| {
        match SESSION.get().and_then(|session| {
            session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take_events()
        }) {
            Some(receiver) => receiver
                .filter_map(|event| async move {
                    // 只处理当前这一代终端的事件 —— 被替换掉的旧 event loop
                    // 还可能短暂活着并往同一个 channel 灌事件，照收会污染显示。
                    // 与 Zed 的 process_pty_event 同一个判断。
                    match event {
                        PtyEvent::Event(event, generation)
                            if generation
                                == CURRENT_GENERATION
                                    .load(std::sync::atomic::Ordering::Relaxed) =>
                        {
                            Some(Message::Event(event))
                        }
                        _ => None,
                    }
                })
                .boxed(),
            // 会话还没起来（或已取走过一次）→ 挂起而不是 panic。
            // `pending::<Infallible>()` 永不产出项，`map` 使其类型对齐，
            // 而这个 map 永远不会被调用。写 `match never {}` 比 cast 安全。
            None => iced::futures::stream::pending::<std::convert::Infallible>()
                .map(|never| match never {})
                .boxed(),
        }
    })
}

/// 键盘事件 → 消息。
///
/// 只在**按下**时转发。这一版还没做焦点判定 —— 单窗口终端可以全盘接收，
/// 将来加多面板时必须在这里加 `is_focused` 判断，否则两个终端的按键会互相串。
fn on_key_event(
    event: iced::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Keyboard(keyboard) => Some(Message::Key(keyboard)),
        _ => None,
    }
}

/// 鼠标事件 → 消息。
///
/// 注意 iced 的 `ButtonPressed` **不带坐标**，只能靠 `CursorMoved` 提前缓存，
/// 所以光标移动事件必须无条件先记住位置，再由按键处理去取。
fn on_mouse_event(
    event: iced::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    use iced::mouse::Button;
    match event {
        iced::Event::Mouse(mouse) => match mouse {
            iced::mouse::Event::CursorMoved { position } => Some(Message::CursorMoved(position)),
            iced::mouse::Event::ButtonPressed(Button::Left) => Some(Message::MouseDown),
            iced::mouse::Event::ButtonReleased(Button::Left) => Some(Message::MouseUp),
            iced::mouse::Event::WheelScrolled { delta, .. } => Some(Message::Wheel(delta)),
            _ => None,
        },
        _ => None,
    }
}

/// 窗口事件 → 消息。
fn on_window_event(
    event: iced::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Window(window::Event::Resized(size)) => Some(Message::Resized(size)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::Physical;
    use iced::keyboard::{Key, Modifiers, key::Named};

    fn metrics() -> aa_terminal_iced::CellMetrics {
        aa_terminal_iced::CellMetrics::measure(
            iced::Font::MONOSPACE,
            aa_terminal_iced::BASE_FONT_SIZE,
        )
    }

    fn geometry() -> GridGeometry {
        aa_terminal_iced::geometry_for(iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT), metrics())
    }

    /// 每个测试独占一个终端：`App.terminal` 要 `'static`，只能泄漏。
    /// 共用一份的话测试之间会互相污染网格内容。
    fn test_app_with(screen: &[u8]) -> App {
        let geometry = geometry();
        // feed 要 &mut，只能在泄漏成 'static 之前喂。
        let mut terminal = Terminal::for_test(window_size_from(&geometry));
        terminal.feed(screen);
        App {
            terminal: Box::leak(Box::new(terminal)),
            palette: palette(),
            geometry,
            viewport: iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT),
            metrics: metrics(),
            exited: false,
            blink: blinking(),
            last_cursor: None,
            dragging: false,
            scrollbar_drag: false,
            scrollbar_grab: 0.0,
            search: None,
            modifiers: Modifiers::default(),
            pressed_link: None,
            opened_links: Vec::new(),
            // 测试里绝不真的去 spawn xdg-open（会弹出浏览器），一律空实现。
            opener: |_| {},
        }
    }

    fn test_app() -> App {
        test_app_with(b"")
    }

    #[test]
    fn a_key_press_pauses_blinking() {
        let mut app = test_app();
        assert!(!app.blink.is_paused());
        let _ = app.update(pressed(Key::Character("x".into()), Modifiers::default()));
        assert!(app.blink.is_paused(), "有输入就暂停闪烁");
        assert!(app.blink.is_visible(), "暂停期间光标必须可见");
    }

    #[test]
    fn only_the_latest_epoch_resumes_blinking() {
        let mut app = test_app();
        let stale = app.blink.pause();
        let fresh = app.blink.pause();
        assert_ne!(stale, fresh, "每次暂停都换一个新 epoch");

        let _ = app.update(Message::BlinkResume(stale));
        assert!(app.blink.is_paused(), "过期 epoch 不得解除暂停");

        let _ = app.update(Message::BlinkResume(fresh));
        assert!(!app.blink.is_paused(), "最新 epoch 才解除暂停");
    }

    #[test]
    fn ticking_while_paused_keeps_the_cursor_lit() {
        let mut app = test_app();
        assert!(app.blink.is_visible());

        let _ = app.update(Message::BlinkTick);
        assert!(!app.blink.is_visible(), "没人打字时随节拍熄灭");
        let _ = app.update(Message::BlinkTick);
        assert!(app.blink.is_visible(), "再一个节拍又亮回来");

        let _ = app.update(pressed(Key::Character("x".into()), Modifiers::default()));
        let _ = app.update(Message::BlinkTick);
        assert!(app.blink.is_visible(), "暂停期间节拍不得熄灭光标");
    }

    fn pressed(key: Key, modifiers: Modifiers) -> Message {
        Message::Key(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: iced::keyboard::Location::Standard,
            modifiers,
            repeat: false,
            text: None,
        })
    }

    fn ctrl_shift() -> Modifiers {
        let mut modifiers = Modifiers::default();
        modifiers.insert(Modifiers::CTRL);
        modifiers.insert(Modifiers::SHIFT);
        modifiers
    }

    #[test]
    fn ctrl_shift_f_opens_the_search_box() {
        let mut app = test_app();
        let _ = app.update(pressed(Key::Character("F".into()), ctrl_shift()));
        assert!(app.search.is_some(), "Ctrl+Shift+F 应打开搜索框");
    }

    #[test]
    fn escape_closes_the_search_box_and_clears_the_query() {
        let mut app = test_app();
        let _ = app.update(pressed(Key::Character("F".into()), ctrl_shift()));
        assert!(app.search.is_some());

        let _ = app.update(pressed(Key::Named(Named::Escape), Modifiers::default()));
        assert!(app.search.is_none(), "Esc 应关闭搜索框");
    }

    #[test]
    fn search_box_is_rendered_over_the_terminal_with_the_match_count() {
        let mut app = test_app_with(b"needle and needle\r\n");
        // 直接设状态，绕过打开流程：这里只关心渲染。
        let _ = app.update(Message::SearchInput("needle".into()));

        let mut ui = iced_test::simulator(app.view());
        assert!(ui.find("2 处").is_ok(), "两个 needle 应显示 2 处");
        assert!(ui.find("needle").is_ok(), "输入框应显示查询词");
    }

    fn shift() -> Modifiers {
        let mut modifiers = Modifiers::default();
        modifiers.insert(Modifiers::SHIFT);
        modifiers
    }

    /// 已输入 `needle` 的搜索态 app（数据里正好 3 处）。
    fn search_app() -> App {
        let mut app = test_app_with(b"needle and needle and needle\r\n");
        let _ = app.update(Message::SearchInput("needle".into()));
        app
    }

    #[test]
    fn enter_jumps_to_the_first_match_then_the_next() {
        let mut app = search_app();
        assert_eq!(app.search.as_ref().unwrap().matches, 3);
        assert_eq!(
            app.search.as_ref().unwrap().current,
            None,
            "还没回车时不该有当前项"
        );

        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        assert_eq!(
            app.search.as_ref().unwrap().current,
            Some(0),
            "第一次回车应落在第一处"
        );
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        assert_eq!(app.search.as_ref().unwrap().current, Some(1));
    }

    #[test]
    fn shift_enter_jumps_backwards() {
        let mut app = search_app();
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        assert_eq!(app.search.as_ref().unwrap().current, Some(1));

        let _ = app.update(pressed(Key::Named(Named::Enter), shift()));
        assert_eq!(
            app.search.as_ref().unwrap().current,
            Some(0),
            "Shift+回车应回上一处"
        );
    }

    #[test]
    fn the_current_index_is_rendered_after_jumping() {
        let mut app = search_app();
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));

        let mut ui = iced_test::simulator(app.view());
        assert!(ui.find("1/3").is_ok(), "跳转后应显示当前序号");
    }

    #[test]
    fn the_closed_state_shows_no_search_box() {
        let app = test_app();
        let mut ui = iced_test::simulator(app.view());
        assert!(ui.find("处").is_err(), "未开搜索时不该出现命中数");
    }

    fn ctrl() -> Modifiers {
        let mut modifiers = Modifiers::default();
        modifiers.insert(Modifiers::CTRL);
        modifiers
    }

    /// 第 `line` 行第 `column` 列那个 cell 正中间的像素位置。
    fn center_of(line: usize, column: usize) -> iced::Point {
        let metrics = metrics();
        iced::Point::new(
            metrics.cell_width * (column as f32 + 0.5),
            metrics.line_height * (line as f32 + 0.5),
        )
    }

    /// 一段里 `link` 是超链接、紧跟其后的 `plain` 不是。
    fn link_app() -> App {
        test_app_with(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\plain")
    }

    #[test]
    fn key_events_update_the_cached_modifiers() {
        let mut app = test_app();
        assert!(!app.modifiers.control());
        let _ = app.update(pressed(Key::Character("x".into()), ctrl()));
        assert!(app.modifiers.control(), "键盘事件应刷新缓存的修饰键");
    }

    #[test]
    fn ctrl_click_on_a_hyperlink_opens_it() {
        let mut app = link_app();
        // 先来一个带 Ctrl 的键盘事件，把修饰键喂进缓存。
        let _ = app.update(pressed(Key::Character("x".into()), ctrl()));
        let _ = app.update(Message::CursorMoved(center_of(0, 1)));
        let _ = app.update(Message::MouseDown);
        let _ = app.update(Message::MouseUp);

        assert_eq!(
            app.opened_links,
            vec!["https://example.com".to_owned()],
            "Ctrl+click 应打开链接"
        );
    }

    #[test]
    fn a_plain_click_on_a_hyperlink_does_not_open_it() {
        // 没按 Ctrl 时那一下只是普通点击，不该打开链接。
        let mut app = link_app();
        let _ = app.update(Message::CursorMoved(center_of(0, 1)));
        let _ = app.update(Message::MouseDown);
        let _ = app.update(Message::MouseUp);
        assert!(app.opened_links.is_empty(), "裸点击不该打开链接");
    }

    /// 喂 60 行造出一段回滚区。
    fn scrolled_app() -> App {
        let mut content = Vec::new();
        for line in 0..60 {
            content.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        test_app_with(&content)
    }

    /// 回滚区远多于一屏 —— 翻页测试要能翻满一整屏。
    ///
    /// 不能复用 `scrolled_app()`：那个只喂 60 行，而这里的窗口高、字号小，
    /// 一屏就有三四十行，回滚区反而不够一屏。
    fn tall_app() -> App {
        let mut content = Vec::new();
        for line in 0..200 {
            content.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        test_app_with(&content)
    }

    #[test]
    fn shift_pageup_scrolls_back_a_screen() {
        let mut app = tall_app();
        let screen = app.terminal.scroll_state().screen_lines();
        assert!(
            app.terminal.scroll_state().history_size() > screen,
            "要多于一屏才看得出翻页"
        );

        let _ = app.update(pressed(Key::Named(Named::PageUp), shift()));
        assert_eq!(
            app.terminal.scroll_state().display_offset(),
            screen,
            "翻一屏 = 一个屏高"
        );

        let _ = app.update(pressed(Key::Named(Named::PageDown), shift()));
        assert_eq!(app.terminal.scroll_state().display_offset(), 0);
    }

    #[test]
    fn shift_arrows_scroll_one_line_at_a_time() {
        let mut app = tall_app();
        let _ = app.update(pressed(Key::Named(Named::ArrowUp), shift()));
        assert_eq!(app.terminal.scroll_state().display_offset(), 1);
        let _ = app.update(pressed(Key::Named(Named::ArrowDown), shift()));
        assert_eq!(app.terminal.scroll_state().display_offset(), 0);
    }

    #[test]
    fn shift_home_and_end_jump_to_the_two_ends() {
        let mut app = tall_app();
        let history = app.terminal.scroll_state().history_size();

        let _ = app.update(pressed(Key::Named(Named::Home), shift()));
        assert_eq!(app.terminal.scroll_state().display_offset(), history);

        let _ = app.update(pressed(Key::Named(Named::End), shift()));
        assert_eq!(app.terminal.scroll_state().display_offset(), 0);
    }

    #[test]
    fn bare_paging_keys_do_not_scroll_locally() {
        // 裸 PageUp / ↑ / Home 是终端自己的：要编码成序列发给程序
        //（less、vim 自己会翻页），不能本地消费掉。
        let mut app = tall_app();
        for key in [Named::PageUp, Named::ArrowUp, Named::Home] {
            let _ = app.update(pressed(Key::Named(key), Modifiers::default()));
        }
        assert_eq!(app.terminal.scroll_state().display_offset(), 0);
    }

    #[test]
    fn paging_does_not_scroll_on_the_alternate_screen() {
        // 全屏程序（vim / less）没有回滚区，Zed 在这里也不滚。
        let mut app = test_app_with(b"\x1b[?1049h");
        assert!(app.terminal.is_alt_screen());
        let _ = app.update(pressed(Key::Named(Named::PageUp), shift()));
        assert_eq!(app.terminal.scroll_state().display_offset(), 0);
    }

    #[test]
    fn dragging_the_scrollbar_scrolls_the_view() {
        let mut app = scrolled_app();
        let history = app.terminal.scroll_state().history_size();
        assert!(history > 0, "喂了 60 行就该有回滚区");
        assert_eq!(app.terminal.scroll_state().display_offset(), 0);

        let (track, _) = app.scrollbar().expect("有回滚区就该有滚动条");
        let x = track.x + track.width / 2.0;

        // 按在轨道顶端 = 翻到最早
        let _ = app.update(Message::CursorMoved(iced::Point::new(x, track.y)));
        let _ = app.update(Message::MouseDown);
        assert!(app.scrollbar_drag, "按在滚动条上应进入拖拽");
        assert!(!app.dragging, "拖滚动条不该同时开始选区");
        let top = app.terminal.scroll_state().display_offset();
        assert!(top > 0, "按在顶端应翻到更早的内容, 实际 offset={top}");

        // 一路拖回底端 = 回到最新
        let _ = app.update(Message::CursorMoved(iced::Point::new(
            x,
            track.y + track.height,
        )));
        assert_eq!(
            app.terminal.scroll_state().display_offset(),
            0,
            "拖到底端应回到最新输出"
        );

        let _ = app.update(Message::MouseUp);
        assert!(!app.scrollbar_drag, "松手应结束拖拽");
    }

    #[test]
    fn grabbing_the_thumb_does_not_jump_the_view() {
        let mut app = scrolled_app();
        let (track, _) = app.scrollbar().expect("有滚动条");
        let x = track.x + track.width / 2.0;

        // 回滚区不长时拇指能占掉半条轨道，所以要点**拇指上方**的空白处
        // （贴最新输出时拇指就在底端，轨道顶端必然是空白）。
        let _ = app.update(Message::CursorMoved(iced::Point::new(x, track.y + 1.0)));
        let _ = app.update(Message::MouseDown);
        let _ = app.update(Message::MouseUp);
        let top = app.terminal.scroll_state().display_offset();
        assert!(top > 0, "点轨道顶端应翻到更早的内容, 实际 offset={top}");

        // 再抓住拇指**上沿**按下：抓点被记住，偏移不该变
        let (_, thumb) = app.scrollbar().expect("有滚动条");
        let on_edge = iced::Point::new(thumb.x + 1.0, thumb.y + 1.0);
        let _ = app.update(Message::CursorMoved(on_edge));
        let _ = app.update(Message::MouseDown);
        assert_eq!(
            app.terminal.scroll_state().display_offset(),
            top,
            "抓住拇指按下不该把视口顶走"
        );
    }

    #[test]
    fn a_click_on_the_content_still_starts_a_selection() {
        // 反向对照：不在滚动条上的按下要照旧走选区，别把整个鼠标都吃掉。
        let mut app = scrolled_app();
        let _ = app.update(Message::CursorMoved(center_of(2, 2)));
        let _ = app.update(Message::MouseDown);
        assert!(app.dragging, "按在内容上应开始拖选");
        assert!(!app.scrollbar_drag, "内容上的按下不该被当成拖滚动条");
    }

    #[test]
    fn releasing_off_the_link_does_not_open_it() {
        // 同 Zed：按下与松开要落在同一链接上。松手挪到非链接格就作罢。
        let mut app = link_app();
        let _ = app.update(pressed(Key::Character("x".into()), ctrl()));
        let _ = app.update(Message::CursorMoved(center_of(0, 1)));
        let _ = app.update(Message::MouseDown);
        // 第 4 列是 "plain" 的第一格，已经不是链接
        let _ = app.update(Message::CursorMoved(center_of(0, 4)));
        let _ = app.update(Message::MouseUp);
        assert!(app.opened_links.is_empty(), "松手离开链接后不该打开");
    }
}
