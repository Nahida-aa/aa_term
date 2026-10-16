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
use aa_terminal_core::{AnsiColors, Session, Terminal, TerminalPalette};
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
    /// 实测出来的 cell 尺寸，渲染与几何换算共用一份。
    metrics: aa_terminal_iced::CellMetrics,
    /// shell 是否已退出。
    exited: bool,
    /// 光标当前是否处于可见相位。
    blink: bool,
    /// 最近一次光标位置。iced 的 `ButtonPressed` 不带坐标，只能自己记。
    last_cursor: Option<iced::Point>,
    /// 左键是否按住（决定移动是拖选还是单纯移动）。
    dragging: bool,
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
    /// 光标该翻转相位了。
    Blink,
    /// 光标移动（既更新缓存也可能在拖选）。
    CursorMoved(iced::Point),
    /// 左键按下。
    MouseDown,
    /// 左键松开。
    MouseUp,
    /// 滚轮滚动。
    Wheel(iced::mouse::ScrollDelta),
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
        let booted = Session::spawn(size, palette, &ShellSpec::new());
        let terminal = match booted {
            Ok(session) => match SESSION.set(Mutex::new(session)) {
                Ok(()) => SESSION
                    .get()
                    .map(|session| session.lock().unwrap_or_else(|e| e.into_inner()).terminal())
                    .unwrap_or_else(|| fallback_terminal(size, palette)),
                Err(rejected) => {
                    // 已经有一个会话了（理论上不会发生：boot 只跑一次）。
                    eprintln!("aa_term: 已有会话，丢弃新会话");
                    let _ = rejected.into_inner().unwrap_or_else(|p| p.into_inner());
                    fallback_terminal(size, palette)
                }
            },
            Err(err) => {
                eprintln!("aa_term: 启动 shell 失败: {err}");
                fallback_terminal(size, palette)
            }
        };

        (
            Self {
                terminal,
                palette,
                geometry,
                metrics,
                exited: false,
                blink: true,
                last_cursor: None,
                dragging: false,
                search: None,
                modifiers: iced::keyboard::Modifiers::default(),
                pressed_link: None,
                opened_links: Vec::new(),
                opener: spawn_opener,
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Event(event) => match event {
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
            Message::Blink => {
                self.blink = !self.blink;
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
                if self.dragging {
                    self.terminal
                        .update_selection(position.x, position.y, &self.geometry);
                }
            }
            Message::MouseDown => {
                let Some(position) = self.last_cursor else {
                    self.dragging = true;
                    return Task::none();
                };
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
                self.dragging = false;
                let Some(position) = self.last_cursor else {
                    return Task::none();
                };
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

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            pty_events(),
            iced::event::listen_with(on_window_event),
            iced::event::listen_with(on_key_event),
            iced::event::listen_with(on_mouse_event),
            // 光标闪烁：终端起不来也照样闪，否则用户看不出焦点在哪儿。
            iced::time::every(CURSOR_BLINK).map(|_| Message::Blink),
        ])
    }

    fn view(&self) -> iced::Element<'_, Message> {
        let terminal =
            aa_terminal_iced::view(self.terminal, &self.palette, self.blink, self.metrics);

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

/// 起不来 PTY 时的兜底终端。
fn fallback_terminal(size: WindowSize, palette: TerminalPalette) -> &'static Terminal {
    FALLBACK.get_or_init(|| Terminal::for_test(size, palette))
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
        let mut terminal = Terminal::for_test(window_size_from(&geometry), palette());
        terminal.feed(screen);
        App {
            terminal: Box::leak(Box::new(terminal)),
            palette: palette(),
            geometry,
            metrics: metrics(),
            exited: false,
            blink: true,
            last_cursor: None,
            dragging: false,
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
