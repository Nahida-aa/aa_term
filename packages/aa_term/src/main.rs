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
//! UI 侧只在**终端内容变化时**重建一次可见内容快照（`Terminal::refresh_content`），
//! 绘制帧只是拿一份 `Arc` 克隆来画 —— 不再每帧锁 `Term`。长时间持锁会把事件
//! 循环卡住，那是「整屏不刷新」的直接原因。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aa_terminal_core::grid::{GridGeometry, Rounding};
use aa_terminal_core::mappings::keymap::Keymap;
use aa_terminal_core::mappings::keys::AppShortcut;
use aa_terminal_core::session::{ShellSpec, window_size_from};
use aa_terminal_core::{
    AnsiColors, BlinkEpoch, BlinkState, RESUME_AFTER_PAUSING, ScrollAmount, Session, Terminal,
    TerminalPalette,
};
use aa_terminal_core::{PtyEvent, TerminalEvent as Event, WindowSize};
use aa_terminal_iced::TerminalSearch;
use aa_ui_iced::{ContextMenu, ContextMenuEntry, WindowControl};
use chrono::Utc;
use iced::keyboard::Key;
use iced::{Subscription, Task, event, window};
use search::{SearchBarState, SearchMessage, SearchOptions, SearchStatus, SearchableItem};

/// 初始视口（像素）。
const INITIAL_WIDTH: f32 = 900.;
const INITIAL_HEIGHT: f32 = 600.;

/// 自绘窗口（CSD）边缘缩放热区的厚度。
///
/// 上游把缩放留给窗口管理器，所以没有可抄的数字；5px 是各家自绘标题栏的常见值。
///
/// 代价：热区是**叠**在内容上的浮层（iced 没有「窗口边框」这层留白），所以最左
/// 5px 会盖住第一列文字的一部分、最下 5px 盖住最后一行 —— 想点第一列开头得往
/// 右偏一点。真要有窗口边框留白，得先有个 1px 的 CSD 边框 + 透明窗口，那是另一
/// 摊子（见 `aa_ui_iced::TitleBar` 里记的取舍）。
const RESIZE_EDGE: f32 = 5.0;

/// 右边缘缩放热区的厚度。
///
/// 比别的边窄：终端滚动条就贴着右边缘（`aa_terminal_iced::SCROLLBAR_MARGIN` 是
/// 2px），热区再往里挤就会把滚动条也变成缩放区。正好取这段留白。
const RIGHT_EDGE_WIDTH: f32 = aa_terminal_iced::SCROLLBAR_MARGIN;

/// 标题栏字号。
///
/// iced 没有 gpui 的 `rem`，标题栏高度是按字号算出来的（`platform_title_bar_height`），
/// 所以这里钉一个明确的字号，渲染与测试才知道标题栏占多高。
const TITLE_BAR_TEXT_SIZE: f32 = 16.0;

/// 默认键位表。
///
/// 照 Zed 的做法把 `assets/keymaps/*.json` 编进二进制（zed 的
/// `assets/keymaps/default-linux.json` 也是这样进的产物），运行时不需要在旁边
/// 放一个 assets 目录。各平台的差异将来靠多份文件 + `cfg!` 选，先只有 Linux 这份。
const KEYMAP: &str = include_str!("../assets/keymaps/default-linux.json");

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
    /// 查找栏：`None` 为关闭。
    ///
    /// 只存栏自己的状态（查询词 + 选项开关）—— 命中数与当前下标归终端，栏渲染
    /// 时经 [`TerminalSearch`] 现取（见 `view`）。
    search: Option<SearchBarState>,
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
    /// 线程库（侧边栏列表的数据源）。
    ///
    /// Zed 里它是 `agent_ui` 上的一个 gpui `Global`，因为有个后台要持久化、到处都
    /// 读得到；我们只有终端这一个宿主，先由 app 持有。`Sidebar` 只读它来重建列表。
    threads: agent_ui::ThreadMetadataStore,
    /// 工作区外壳：侧边栏开着没、多宽、停在哪一侧。
    ///
    /// 侧边栏本体（`sidebar::Sidebar`）注册在里面，布局也由它摆 —— app 只给
    /// 「主区」与两条拖拽消息。
    multi_workspace: workspace::MultiWorkspace<sidebar::Sidebar>,
    /// 主题：所有界面颜色都从这里取（终端调色板是另一份，见 `palette()`）。
    ///
    /// iced 没有 gpui 那种「环境主题」，所以由 app 持有、渲染时往下传。
    theme: aa_theme_iced::Theme,
    /// 打开着的右键菜单（`None` = 没有）。
    ///
    /// 菜单内容在 app 这一层：条目里的动作要动 store 与侧边栏状态，侧边栏只报
    /// 「哪一行被右键了」。
    menu: Option<ContextMenu<Message>>,
    /// 标题栏**内容**：菜单栏（File/Edit/…）+ 项目名 + 分支。
    ///
    /// 窗口框架（拖拽区、最小化/最大化/关闭）是共享组件 `aa_ui_iced::TitleBar`；
    /// 这里只存内容那半的状态，与上游两个 crate 的分工一致。
    title_bar: title_bar::TitleBar,
    /// 窗口有没有焦点 —— 标题栏失焦要变暗（上游在 linux/freebsd 上这么分）。
    focused: bool,
    /// 键位表：按键 → 动作。
    ///
    /// 数据来自 [`KEYMAP`]，解析与查询在 `aa_terminal_core::mappings::keymap`。
    /// 放实例上而不是每次按键现解析 —— 解析要过 JSON。
    keymap: Keymap,
    /// 是否正在拖侧边栏的宽度手柄。
    ///
    /// 与 `scrollbar_drag` 同一套路：手柄只有 6px 命中区，指针一快就出了范围收不到
    /// 事件，所以按下之后由全局指针事件驱动。
    sidebar_resize: bool,
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
    /// 打开查找栏（Ctrl+Shift+F）。
    OpenSearch,
    /// 查找栏发来的消息（查询词、选项开关、上/下一处、关闭）。
    ///
    /// 整条搬进来而不是拆成几个变体：栏的消息集合将来会变（加替换、加选项），
    /// 拆开就得在 app 里跟着改一遍 `match`。
    Search(SearchMessage),
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
    /// 开关线程侧边栏（Ctrl+Alt+J）。
    ToggleWorkspaceSidebar,
    /// 菜单条目：重命名某条线程。
    RenameThread(agent_ui::ThreadId),
    /// 菜单条目：归档某条线程。
    ArchiveThread(agent_ui::ThreadId),
    /// 关掉右键菜单（点空白处、按 Esc、选中条目之后）。
    CloseContextMenu,
    /// 标题栏内容发来的消息（点了菜单栏/项目名/分支）。
    TitleBar(title_bar::Message),
    /// 标题栏菜单里选中的条目。
    TitleBarAction(title_bar::MenuAction),
    /// 按在标题栏空白处：拖窗口。
    DragWindow,
    /// 按了窗口按钮（最小化/最大化/关闭）。
    WindowControl(WindowControl),
    /// 按在窗口边缘：缩放窗口。
    ResizeWindow(iced::window::Direction),
    /// 窗口焦点变了（标题栏底色跟着变）。
    FocusChanged(bool),
    /// 侧边栏发来的消息（新建线程、打开某条）。
    ///
    /// 与 `Search` 同样整条搬进来：栏/侧边栏的消息集合会变，拆成几个变体就得在
    /// app 的 `match` 里跟着改两遍。
    Sidebar(sidebar::SidebarMessage),
    /// 按下了侧边栏的宽度手柄，开始拖拽。
    SidebarResizeStart,
    /// 双击手柄：宽度回到默认值。
    SidebarResetWidth,
}

fn main() -> iced::Result {
    init_tracing();

    let mut app = iced::application(App::boot, App::update, App::view)
        .title("aa_term")
        .window(window_settings())
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

/// 装日志订阅器。
///
/// **必须显式装**：`tracing` 只是一套宏 + 门面，没有订阅器时所有事件都被丢掉
/// （iced 自己不装，wgpu 也不装）。这就是「跑起来一行日志都看不见」的原因。
///
/// 默认只放开本 crate 的 debug，其余按 warn：iced/wgpu 的 info 级日志一屏一屏地
/// 刷，会把真正想看的东西冲掉。`RUST_LOG` 可以覆盖，例如
/// `RUST_LOG=aa_term=trace,aa_terminal_iced=debug`。
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // 注意 `aa_term` 会同时匹配 `aa_term` 与 `aa_terminal_*`（前缀匹配），
        // 正是想要的：终端模型层的日志一起看。
        tracing_subscriber::EnvFilter::new("aa_term=debug,warn")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
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
                    tracing::error!("已有会话，丢弃新会话");
                    let _ = rejected.into_inner().unwrap_or_else(|p| p.into_inner());
                    fallback_terminal(size)
                }
            },
            Err(err) => {
                tracing::error!(%err, "启动 shell 失败，退回兜底终端");
                fallback_terminal(size)
            }
        };

        tracing::info!(
            columns = geometry.num_columns(),
            lines = geometry.num_lines(),
            width = geometry.width,
            height = geometry.height,
            "启动：终端 {} 列 × {} 行",
            geometry.num_columns(),
            geometry.num_lines(),
        );

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
                threads: agent_ui::ThreadMetadataStore::new(),
                multi_workspace: sidebar_shell(),
                sidebar_resize: false,
                theme: aa_theme_iced::Theme::dark(),
                menu: None,
                title_bar: title_bar_at_cwd(),
                focused: true,
                keymap: default_keymap(),
            },
            Task::none(),
        )
    }

    /// 有输入就把光标稳住、暂停闪烁，并排一个「恢复」定时器。
    ///
    /// 判定放在这里而不是各按键分支里 —— 按键分支有若干提前返回的路径
    /// （搜索框开着、快捷键命中），逐处补丁必漏。
    fn update(&mut self, message: Message) -> Task<Message> {
        // 「按了没反应」类的排查全靠这一行：能看出事件有没有进来、进的是哪条。
        // 高频消息降到 trace —— `Wakeup` / 光标移动 / 闪烁节拍一秒钟好几百条，
        // 放 debug 会把真正想看的那几条冲走（实测跑一次满屏都是 Wakeup）。
        match &message {
            Message::Event(Event::Wakeup) | Message::CursorMoved(_) | Message::BlinkTick => {
                tracing::trace!(?message, "→ 更新");
            }
            _ => tracing::debug!(?message, "→ 更新"),
        }

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
                Event::Wakeup => {
                    tracing::trace!("pty 有新输出，重建内容快照");
                    self.terminal.refresh_content();
                }
                Event::Exit => {
                    tracing::info!("shell 退出");
                    self.exited = true;
                }
                // 窗口标题；这一版先不接线。
                Event::Title(title) => tracing::debug!(%title, "标题变化（暂不接线）"),
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
                    // 菜单开着时 Esc 先关菜单 —— 与 zed 一致：一层层往上关
                    // （`ContextMenu::cancel` 先吃 Esc，再轮到别的）。
                    if self.menu.is_some()
                        && key.as_ref() == Key::Named(iced::keyboard::key::Named::Escape)
                    {
                        tracing::debug!("Esc 关掉右键菜单");
                        return self.update(Message::CloseContextMenu);
                    }
                    // 重命名输入框开着时按键归输入框：Esc 取消、回车提交，其余
                    // 一律不发往 shell —— 否则用户敲的标题会被当成终端输入执行
                    // （与下面查找栏同一个道理）。
                    if self
                        .multi_workspace
                        .sidebar()
                        .is_some_and(|sidebar| sidebar.renaming_thread_id().is_some())
                    {
                        if key.as_ref() == Key::Named(iced::keyboard::key::Named::Escape) {
                            return self
                                .update(Message::Sidebar(sidebar::SidebarMessage::RenameCancel));
                        }
                        if key.as_ref() == Key::Named(iced::keyboard::key::Named::Enter) {
                            return self
                                .update(Message::Sidebar(sidebar::SidebarMessage::RenameCommit));
                        }
                        tracing::debug!(?key, "重命名输入框开着，按键归输入框");
                        return Task::none();
                    }
                    // 搜索框打开时按键归输入框。这里只留 Esc（关搜索），其余一律
                    // 不发往 shell —— 否则用户敲的搜索词会被当成终端输入执行。
                    // 输入框自身通过 on_input 拿到字符，不经过这里。
                    if self.search.is_some() {
                        if key.as_ref() == Key::Named(iced::keyboard::key::Named::Escape) {
                            return self.update(Message::Search(SearchMessage::Close));
                        }
                        // 回车在查找栏里是「跳到下一处」，这是查找最常用的操作。
                        // 在这里拦而不是用 `text_input` 的 `on_submit`：后者拿不到
                        // Shift，区分不出上一处。
                        if key.as_ref() == Key::Named(iced::keyboard::key::Named::Enter) {
                            return self.update(Message::Search(if modifiers.shift() {
                                SearchMessage::Prev
                            } else {
                                SearchMessage::Next
                            }));
                        }
                        tracing::debug!(?key, "查找栏开着，按键归输入框");
                        return Task::none();
                    }
                    // app_cursor 必须从 Term 现读：shell 随时可能切换它
                    // （vim 进入正常模式就会），缓存会发错方向序列。
                    let app_cursor = self.terminal.app_cursor_mode();
                    // 应用级快捷键必须先于按键编码拦掉 —— 否则会被当普通键发进
                    // shell（Ctrl+Shift+C 想复制，结果发了 \x03 中断）。
                    // 哪个键绑哪个动作由键位表（`assets/keymaps/`）决定，别在这里
                    // 再写一份 if 链 —— 那样文件里的绑定就成了自欺欺人。
                    if let Some(shortcut) = self.keymap.action_for(key, *modifiers) {
                        tracing::debug!(?shortcut, "命中应用级快捷键，不再发往 shell");
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
                            AppShortcut::ToggleWorkspaceSidebar => {
                                self.update(Message::ToggleWorkspaceSidebar)
                            }
                        };
                    }
                    let encoded =
                        aa_terminal_core::mappings::keys::encode(key, modifiers, app_cursor, true);
                    tracing::debug!(
                        ?key,
                        ?modifiers,
                        app_cursor,
                        bytes = encoded.as_ref().map(|b| b.len()),
                        "按键 → shell"
                    );
                    if let (Some(bytes), Some(session)) = (encoded, SESSION.get()) {
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
                    .map(|state| state.query().to_string())
                    .unwrap_or_default();
                let supported = TerminalSearch::new(self.terminal).supported_options();
                // 选项默认全开（视图支持什么就开什么）—— 终端支持正则，于是
                // 「正则」开关一开始就是亮的，与 Zed 的终端一致。
                let state = SearchBarState::new(query, supported);
                TerminalSearch::new(self.terminal).set_query(state.query(), state.options());
                self.search = Some(state);
                // 焦点必须主动移过去，否则刚打开的输入框收不到按键。
                return iced::widget::operation::focus(iced::widget::Id::new(
                    search::SEARCH_INPUT_ID,
                ));
            }
            Message::Search(SearchMessage::Close) => {
                // 单独一条分支：关掉栏之后 `self.search` 就该是 `None`，不能让
                // 上面那段的借用还活着。
                TerminalSearch::new(self.terminal).set_query("", SearchOptions::NONE);
                self.search = None;
            }
            Message::Search(message) => {
                // 借的是 `self` 的两个不同字段（`terminal` / `search`），借用
                // 检查器分得清 —— 所以这里能一边拿 adapter 一边改栏状态。
                let Some(state) = self.search.as_mut() else {
                    return Task::none();
                };
                let mut item = TerminalSearch::new(self.terminal);
                match message {
                    SearchMessage::Query(query) => {
                        state.set_query(query);
                        // 每敲一个字都重跑一次：终端这边是收集式搜索（一次扫全
                        // 部命中），比 alacritty 每次从 origin 重扫便宜。
                        item.set_query(state.query(), state.options());
                    }
                    SearchMessage::Toggle(option) => {
                        state.toggle(option);
                        // 选项变了要按新语义重搜（例如关掉正则后 `.` 变成小数点），
                        // 否则栏上的高亮与实际行为不一致。
                        item.set_query(state.query(), state.options());
                    }
                    SearchMessage::Next => item.next_match(),
                    SearchMessage::Prev => item.prev_match(),
                    SearchMessage::Close => unreachable!("上面单独处理过"),
                }
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
            Message::TitleBar(title_bar::Message::ShowMenu(kind)) => {
                // 菜单贴在指针处：iced 拿不到菜单按钮的屏幕坐标（上游是贴在按钮
                // 下沿的）。
                let position = self.last_cursor.unwrap_or_default();
                tracing::debug!(?kind, ?position, "弹出菜单栏里的菜单");
                self.menu = Some(title_bar::menu(kind, position, Message::TitleBarAction));
            }
            Message::TitleBar(title_bar::Message::ShowProjectMenu) => {
                let position = self.last_cursor.unwrap_or_default();
                self.menu = Some(title_bar::project_menu(position, Message::TitleBarAction));
            }
            Message::TitleBar(title_bar::Message::ShowBranchMenu) => {
                let position = self.last_cursor.unwrap_or_default();
                self.menu = Some(title_bar::branch_menu(position, Message::TitleBarAction));
            }
            Message::TitleBarAction(action) => {
                // 选了条目菜单就收起来（上游 `ContextMenu` 也是点完即收）。
                self.menu = None;
                return self.title_bar_action(action);
            }
            Message::DragWindow => {
                // 与上游 `window_control_area(Drag)` 同一个动作：`_NET_WM_MOVERESIZE`
                // 交给窗口管理器，按下之后不用我们跟。
                tracing::debug!("拖动窗口");
                return window_task(iced::window::drag);
            }
            Message::WindowControl(control) => {
                // 窗口按钮与 Window 菜单是同一批动作（上游两处都在）。
                let action = match control {
                    WindowControl::Minimize => title_bar::MenuAction::Minimize,
                    WindowControl::Maximize => title_bar::MenuAction::Maximize,
                    WindowControl::Close => title_bar::MenuAction::CloseWindow,
                };
                return self.title_bar_action(action);
            }
            Message::ResizeWindow(direction) => {
                tracing::debug!(?direction, "缩放窗口");
                return window_task(move |id| iced::window::drag_resize(id, direction));
            }
            Message::FocusChanged(focused) => {
                tracing::debug!(focused, "窗口焦点变化");
                self.focused = focused;
            }
            Message::ToggleWorkspaceSidebar => {
                // 菜单是按窗口坐标摆的，侧边栏一开一关布局就变了；先收掉最省事。
                self.menu = None;
                self.multi_workspace.toggle_sidebar();
                tracing::info!(
                    open = self.multi_workspace.sidebar_open(),
                    width = self.multi_workspace.sidebar_width(),
                    side = ?self.multi_workspace.sidebar_side(),
                    "开关侧边栏"
                );
                // 侧边栏一开一关，终端区域宽度就变了：网格列数与 PTY 尺寸都得跟着
                // 走，否则 shell 会按旧的宽度折行。
                self.sync_geometry();
            }
            Message::Sidebar(message) => {
                // 借的两个不同字段（`multi_workspace` / `threads`），借用检查器
                // 分得清。
                let Some(sidebar) = self.multi_workspace.sidebar_mut() else {
                    return Task::none();
                };
                tracing::debug!(?message, "侧边栏消息");
                match message {
                    // 「新建线程」目前只做数据侧：写一条草稿元数据、进列表、选中它。
                    // 会话本身要等 agent 后端接进来。
                    sidebar::SidebarMessage::CreateNewThread => {
                        sidebar.create_new_thread(&mut self.threads, Utc::now());
                    }
                    sidebar::SidebarMessage::ActivateEntry(thread_id) => {
                        sidebar.activate_entry(&thread_id);
                    }
                    // 右键：菜单内容归宿主，位置用缓存的指针位置。
                    sidebar::SidebarMessage::ShowContextMenu(thread_id) => {
                        let position = self.last_cursor.unwrap_or_default();
                        tracing::debug!(?thread_id, ?position, "弹出线程行的右键菜单");
                        self.menu = Some(thread_context_menu(position, &thread_id));
                    }
                    sidebar::SidebarMessage::RenameInput(text) => {
                        sidebar.set_renaming_text(text);
                    }
                    sidebar::SidebarMessage::RenameCommit => {
                        // 空标题当取消（上游的空标题同样不写）。
                        let target = sidebar
                            .renaming_thread_id()
                            .cloned()
                            .zip(sidebar.renaming_text().map(str::to_owned));
                        sidebar.stop_renaming();
                        if let Some((thread_id, title)) = target
                            && !title.trim().is_empty()
                        {
                            tracing::info!(?thread_id, %title, "重命名线程");
                            self.threads.set_title_override(&thread_id, title);
                            sidebar.rebuild_contents(&self.threads);
                        }
                    }
                    sidebar::SidebarMessage::RenameCancel => {
                        sidebar.stop_renaming();
                    }
                }
            }
            Message::RenameThread(thread_id) => {
                self.menu = None;
                if let Some(sidebar) = self.multi_workspace.sidebar_mut() {
                    // 上游 `start_renaming_entry` 同样只把编辑器打开，落库在提交时。
                    sidebar.start_renaming(&thread_id);
                }
            }
            Message::ArchiveThread(thread_id) => {
                self.menu = None;
                tracing::info!(?thread_id, "归档线程");
                self.threads.archive(&thread_id);
                if let Some(sidebar) = self.multi_workspace.sidebar_mut() {
                    // 归档的那条要从列表里消失；若它正是打开着的那条，选中也一起清掉。
                    sidebar.rebuild_contents(&self.threads);
                }
            }
            Message::CloseContextMenu => {
                self.menu = None;
            }
            Message::SidebarResizeStart => {
                tracing::debug!("开始拖侧边栏宽度手柄");
                self.sidebar_resize = true;
                // 拖宽期间不再当成拖选：手柄与终端内容挨着，起点落在哪一边很容易
                // 判错（滚动条那套也是这么先声明的）。
                self.dragging = false;
            }
            Message::SidebarResetWidth => {
                tracing::debug!("双击手柄：宽度复位");
                self.multi_workspace.set_sidebar_width(None);
                self.sidebar_resize = false;
                self.sync_geometry();
            }
            Message::CursorMoved(position) => {
                // 先缓存，ButtonPressed 要用 —— iced 的按键事件不带坐标。
                self.last_cursor = Some(position);
                if self.sidebar_resize {
                    self.resize_sidebar_to(position.x);
                } else if self.scrollbar_drag {
                    self.scroll_to_scrollbar(position.y);
                } else if self.dragging
                    && let Some(local) = self.to_terminal(position)
                {
                    self.terminal
                        .update_selection(local.x, local.y, &self.geometry);
                }
            }
            Message::MouseDown => {
                let Some(position) = self.last_cursor else {
                    self.dragging = true;
                    return Task::none();
                };
                // 落在侧边栏上的按下不归终端：那一行自己会处理这次点击，终端这边
                // 既不该开始选区，也不该去开链接（否则点侧边栏会顺带选中终端第一列）。
                let Some(local) = self.to_terminal(position) else {
                    tracing::debug!(
                        x = position.x,
                        y = position.y,
                        inset = self.sidebar_inset(),
                        "按下落在终端区域外（侧边栏/外部），不归终端"
                    );
                    self.dragging = false;
                    self.pressed_link = None;
                    return Task::none();
                };
                tracing::debug!(
                    x = local.x,
                    y = local.y,
                    window_x = position.x,
                    "→ 终端内按下"
                );
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
                    && let Some(uri) = self.hyperlink_at(local)
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
                            local.x,
                            local.y,
                            &self.geometry,
                        )
                });
                if reported {
                    self.dragging = false;
                    return Task::none();
                }

                self.terminal.start_selection(
                    aa_terminal_core::SelectionType::Simple,
                    local.x,
                    local.y,
                    &self.geometry,
                );
            }
            Message::MouseUp => {
                let was_scrollbar_drag = self.scrollbar_drag;
                let was_sidebar_resize = self.sidebar_resize;
                self.scrollbar_drag = false;
                self.sidebar_resize = false;
                self.dragging = false;
                // 拖手柄也一样：程序没收到 press，就不该收到 release，也不必再去
                // 判链接。
                if was_sidebar_resize {
                    return Task::none();
                }
                let Some(position) = self.last_cursor else {
                    return Task::none();
                };
                // 滚轮条拖拽与手柄同理：都不产生「释放」语义。
                if was_scrollbar_drag {
                    return Task::none();
                }
                // 指针这会儿可能停在侧边栏上（松手前挪过去了）—— 那就没有链接可谈。
                let local = self.to_terminal(position);
                // 松手：只有仍落在按下时的同一链接上才打开。
                if let Some(pressed) = self.pressed_link.take() {
                    if local.and_then(|local| self.hyperlink_at(local)).as_deref()
                        == Some(pressed.as_str())
                    {
                        (self.opener)(&pressed);
                        self.opened_links.push(pressed);
                    }
                    return Task::none();
                }
                let Some(local) = local else {
                    return Task::none();
                };
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
                            local.x,
                            local.y,
                            &self.geometry,
                        )
                });
            }
            Message::ScrollView(amount) => {
                tracing::debug!(?amount, "视口滚动（应用级）");
                // alternate screen（vim / less）上没有回滚区，那是程序的地盘：
                // 与 Zed 一样既不自己滚、也不把翻页键转发过去。
                if !self.terminal.is_alt_screen() {
                    self.terminal.scroll_display(amount);
                }
            }
            Message::Wheel(delta) => {
                tracing::trace!(?delta, "滚轮 → shell");
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
                tracing::debug!(width = size.width, height = size.height, "窗口尺寸变化");
                // 先记下窗口尺寸：后面可能因退化而提前返回，视口也要保持同步。
                self.viewport = size;
                // 窗口最小化时尺寸归零。让终端也 resize 到 0 列会让内核接受但内容
                // 全部错乱，恢复时还要再排一次 —— 这道判断在 `sync_geometry` 里。
                self.sync_geometry();
            }
        }
        Task::none()
    }

    /// 终端区域内的一个像素位置命中的超链接 URI。
    ///
    /// 换算走 [`GridGeometry::point_at`] 的 `Truncate`，与 core 的选区命中
    /// （`selection.rs`）同一条路径 —— 两处取整方式不一致会变成「明明点在链接
    /// 上却打不开」。
    fn hyperlink_at(&self, local: iced::Point) -> Option<String> {
        let point = self.geometry.point_at(local.x, local.y, Rounding::Truncate);
        self.terminal.hyperlink_at(point)
    }

    /// 终端画布在**窗口坐标**里的矩形。
    ///
    /// canvas 自己铺满它被分到的那一块，而侧边栏会从窗口里切走一条 —— 所以这里
    /// 不能再用 `(0,0,viewport)`：终端区域的原点会被推到侧边栏右侧（停靠在右时
    /// 仍是 0）。滚动条矩形、命中判定都按这个矩形算，于是指针坐标只需在入口处
    /// 减一次偏移。
    fn terminal_bounds(&self) -> iced::Rectangle {
        iced::Rectangle::new(
            iced::Point::new(self.sidebar_inset(), title_bar_height()),
            self.terminal_size(),
        )
    }

    /// 侧边栏从窗口**左侧**吃掉的像素。
    ///
    /// 停靠在右边时是 0 —— 那时终端仍在窗口左侧，只是右侧少了一块。
    ///
    /// 手柄那 6px 也算进去：它夹在两者之间，不归任何一方画。
    fn sidebar_inset(&self) -> f32 {
        let sidebar = self.multi_workspace.sidebar_render_state();
        if sidebar.open && !sidebar.side.is_right() {
            self.multi_workspace.sidebar_width() + workspace::SIDEBAR_RESIZE_HANDLE_SIZE
        } else {
            0.0
        }
    }

    /// 终端可用区域的尺寸（窗口减去侧边栏与标题栏）。
    fn terminal_size(&self) -> iced::Size {
        iced::Size::new(
            (self.viewport.width - self.sidebar_inset()).max(0.),
            (self.viewport.height - title_bar_height()).max(0.),
        )
    }

    /// 窗口坐标 → 终端区域内的坐标；落在终端区域外（例如侧边栏上）时是 `None`。
    fn to_terminal(&self, position: iced::Point) -> Option<iced::Point> {
        let bounds = self.terminal_bounds();
        bounds
            .contains(position)
            .then(|| iced::Point::new(position.x - bounds.x, position.y - bounds.y))
    }

    /// 按当前终端区域重算网格，并把新尺寸同步给 PTY。
    ///
    /// 三处都会改变终端区域，必须都走这里：窗口缩放、侧边栏开关、拖宽。
    /// 网格列数与 PTY 尺寸一旦不同步，shell 就会按错误的宽度折行（折在屏幕外）。
    fn sync_geometry(&mut self) {
        let geometry = aa_terminal_iced::geometry_for(self.terminal_size(), self.metrics);
        if geometry.num_columns() == 0 || geometry.num_lines() == 0 {
            // 窗口最小化，或侧边栏宽到把终端挤没了。保留旧几何，等下一次尺寸变化。
            return;
        }
        tracing::debug!(
            columns = geometry.num_columns(),
            lines = geometry.num_lines(),
            width = geometry.width,
            "重算终端几何（窗口/侧边栏变化）"
        );
        self.geometry = geometry;
        // PTY 侧走 alacritty 的 channel 而非直接 ioctl：与读取同序。
        if let Some(session) = SESSION.get() {
            session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .resize(window_size_from(&geometry));
        }
    }

    /// 拖拽中把宽度跟到指针位置。
    ///
    /// 停靠在右时宽度是从窗口右边量的 —— 指针位置不能直接用。
    fn resize_sidebar_to(&mut self, x: f32) {
        let width = if self.multi_workspace.sidebar_side().is_right() {
            self.viewport.width - x
        } else {
            x
        };
        tracing::trace!(x, width, "拖动侧边栏宽度");
        self.multi_workspace.set_sidebar_width(Some(width));
        self.sync_geometry();
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
        // 侧边栏由 `MultiWorkspace` 摆位（`[侧边栏, 手柄, 主区]` 或镜像），它自己
        // 渲染 `sidebar::Sidebar`、自己把侧边栏消息映射成 `Message`，所以这里只给
        // 主区和两条拖拽消息。收起时它原样返回主区。
        let body = self.multi_workspace.layout(
            &self.theme,
            self.main_view(),
            Message::SidebarResizeStart,
            Message::SidebarResetWidth,
            Message::Sidebar,
        );

        // 标题栏在最上面，占满整个窗口宽度（侧边栏在它下面 —— 上游也是这么摆的）：
        // 窗口框架在 `aa_ui_iced::TitleBar`，内容（菜单栏、项目名、分支）在
        // `title_bar` crate。窗口按钮只有自绘装饰（CSD）时才该画，我们正是 CSD。
        let title_bar = aa_ui_iced::TitleBar::new(&self.theme)
            .focused(self.focused)
            .text_size(TITLE_BAR_TEXT_SIZE)
            .left(title_bar::view(
                &self.title_bar,
                &self.theme,
                Message::TitleBar,
            ))
            .window_controls(true)
            .on_drag(Message::DragWindow)
            .on_window_control(Message::WindowControl)
            .into_element();

        let window = iced::widget::column![title_bar, body];

        // 三层浮层，自下而上：缩放热区（贴在窗口边缘）→ 打开的菜单。
        let mut layers = vec![window.into()];
        layers.extend(self.resize_edges());
        if let Some(menu) = &self.menu {
            // 菜单是按窗口坐标摆的，所以叠在整棵树最上面，而不是塞进某一层里
            // （塞进去会被那一层的宽度裁掉）。
            layers.push(menu.overlay(&self.theme, Message::CloseContextMenu));
        }
        iced::widget::stack(layers).into()
    }

    /// 窗口边缘的 8 条缩放热区。
    ///
    /// 上游在 X11 上**不做**这件事（`platform_title_bar` 只实现拖动与窗口按钮，
    /// 缩放留给窗口管理器）；iced 的自绘窗口把装饰关掉之后，没有窗口管理器帮忙就
    /// 彻底没法缩放，所以这里自己铺。做法是 8 个「占满整窗的容器 + 用 padding
    /// 把热区推到边上」的浮层（iced 没有绝对定位，`ContextMenu` 也是这么摆的）。
    ///
    /// 尺寸：上下左右 5px，四角 5×5 且叠在边之上（先命中角）。右边那条只用
    /// [`RIGHT_EDGE_WIDTH`]（终端滚动条就贴在右边缘，热区不能再往里挤）。
    fn resize_edges(&self) -> Vec<iced::Element<'_, Message>> {
        use iced::window::Direction;

        let size = self.viewport;
        let right = RIGHT_EDGE_WIDTH;
        let edge = RESIZE_EDGE;

        // (方向, 距左上角的偏移, 热区尺寸, 指针形状)
        let strips: [(
            Direction,
            iced::Padding,
            (f32, f32),
            iced::mouse::Interaction,
        ); 8] = [
            (
                Direction::North,
                iced::Padding::ZERO,
                (size.width, edge),
                iced::mouse::Interaction::ResizingVertically,
            ),
            (
                Direction::South,
                iced::Padding {
                    top: (size.height - edge).max(0.0),
                    ..iced::Padding::ZERO
                },
                (size.width, edge),
                iced::mouse::Interaction::ResizingVertically,
            ),
            (
                Direction::West,
                iced::Padding::ZERO,
                (edge, size.height),
                iced::mouse::Interaction::ResizingHorizontally,
            ),
            (
                Direction::East,
                iced::Padding {
                    left: (size.width - right).max(0.0),
                    ..iced::Padding::ZERO
                },
                (right, size.height),
                iced::mouse::Interaction::ResizingHorizontally,
            ),
            (
                Direction::NorthWest,
                iced::Padding::ZERO,
                (edge, edge),
                iced::mouse::Interaction::ResizingDiagonallyUp,
            ),
            (
                Direction::NorthEast,
                iced::Padding {
                    left: (size.width - right).max(0.0),
                    ..iced::Padding::ZERO
                },
                (right, edge),
                iced::mouse::Interaction::ResizingDiagonallyDown,
            ),
            (
                Direction::SouthWest,
                iced::Padding {
                    top: (size.height - edge).max(0.0),
                    ..iced::Padding::ZERO
                },
                (edge, edge),
                iced::mouse::Interaction::ResizingDiagonallyUp,
            ),
            (
                Direction::SouthEast,
                iced::Padding {
                    top: (size.height - edge).max(0.0),
                    left: (size.width - right).max(0.0),
                    ..iced::Padding::ZERO
                },
                (right, edge),
                iced::mouse::Interaction::ResizingDiagonallyDown,
            ),
        ];

        strips
            .into_iter()
            .map(|(direction, offset, (width, height), interaction)| {
                let strip = iced::widget::mouse_area(
                    iced::widget::container(iced::widget::space::horizontal())
                        .width(iced::Length::Fixed(width))
                        .height(iced::Length::Fixed(height)),
                )
                .interaction(interaction)
                .on_press(Message::ResizeWindow(direction));

                // 外层占满整窗，用 padding 把热区推到边上（左上角为原点）。
                iced::widget::container(strip)
                    .padding(offset)
                    .width(iced::Fill)
                    .height(iced::Fill)
                    .align_x(iced::Alignment::Start)
                    .align_y(iced::Alignment::Start)
                    .into()
            })
            .collect()
    }

    /// 菜单/窗口按钮选中的条目 → 实际动作。
    fn title_bar_action(&mut self, action: title_bar::MenuAction) -> Task<Message> {
        tracing::info!(?action, "标题栏动作");
        match action {
            title_bar::MenuAction::NewThread => {
                self.update(Message::Sidebar(sidebar::SidebarMessage::CreateNewThread))
            }
            title_bar::MenuAction::Copy => self.update(Message::Copy),
            title_bar::MenuAction::Paste => self.update(Message::Paste),
            title_bar::MenuAction::ToggleSidebar => self.update(Message::ToggleWorkspaceSidebar),
            title_bar::MenuAction::Minimize => window_task(|id| iced::window::minimize(id, true)),
            title_bar::MenuAction::Maximize => window_task(iced::window::toggle_maximize),
            // 单窗口应用：关掉窗口就是退出（上游 `CloseWindow` 与 `Quit` 是两个动作）。
            title_bar::MenuAction::CloseWindow => window_task(iced::window::close),
            title_bar::MenuAction::CopyProjectPath => match self.title_bar.project_path() {
                Some(path) => iced::clipboard::write(path.display().to_string()),
                None => Task::none(),
            },
            title_bar::MenuAction::CopyBranchName => match self.title_bar.branch() {
                Some(branch) => iced::clipboard::write(branch.to_owned()),
                None => Task::none(),
            },
            // 与终端里的链接同一个 opener（`xdg-open`）：目录会开文件管理器。
            title_bar::MenuAction::RevealProject => {
                if let Some(path) = self.title_bar.project_path() {
                    (self.opener)(&path.display().to_string());
                }
                Task::none()
            }
        }
    }

    /// 主区：终端，外加开着时的查找栏浮层。
    fn main_view(&self) -> iced::Element<'_, Message> {
        let terminal = aa_terminal_iced::view(
            self.terminal,
            &self.palette,
            self.blink.is_visible(),
            self.metrics,
        );

        // 没开查找栏就直接给终端，避免白白套一层 stack。
        let Some(state) = &self.search else {
            return terminal;
        };

        // 命中数与当前下标每次都从终端现取（栏自己只存查询词与选项）——
        // 抄一份进栏里就会有两处真相。
        let item = TerminalSearch::new(self.terminal);
        let status = SearchStatus {
            matches: item.match_count(),
            current: item.active_match_index(),
        };

        // stack 把查找栏叠在终端之上；外层 container 负责把它推到右上角。
        iced::widget::stack![
            terminal,
            iced::widget::container(search::view(
                state,
                status,
                item.supported_options(),
                Message::Search,
            ))
            .width(iced::Fill)
            .height(iced::Shrink)
            .padding(12)
            .align_x(iced::Right)
        ]
        .into()
    }
}

/// 装好侧边栏的工作区外壳。
///
/// **默认收起**（`register_sidebar` 只管注册，展开是 `toggle_sidebar` 的事）——
/// 现在的侧边栏里只有「还没有线程」：agent 后端还没接进来，一上来就吃掉 300px
/// 终端宽度不值当。Zed 的对应开关是 `agent.threads_sidebar.auto_open`（默认开），
/// 等设置层建好再照搬；在那之前 Ctrl+Alt+J 开关。
fn sidebar_shell() -> workspace::MultiWorkspace<sidebar::Sidebar> {
    let mut multi_workspace = workspace::MultiWorkspace::new();
    multi_workspace.register_sidebar(sidebar::Sidebar::new(workspace::SidebarSide::Left));
    multi_workspace
}

/// 线程行的右键菜单。
///
/// 条目照 zed 的 `render_thread` 里那份（`Rename Title` / `Regenerate Thread
/// Title` / `Open Thread as Markdown` / `Archive Thread`）。后两条我们还没有依赖
/// （前者要 agent 后端重命名，后者要 markdown 面板）—— 照上游的样子**置灰显示**
/// 而不是省略，这样菜单形状与 zed 一致，缺口也留在界面上看得见。
fn thread_context_menu(
    position: iced::Point,
    thread_id: &agent_ui::ThreadId,
) -> ContextMenu<Message> {
    ContextMenu::new(position)
        .entry(
            ContextMenuEntry::new("Rename Title")
                .on_press(Message::RenameThread(thread_id.clone())),
        )
        .entry(ContextMenuEntry::new("Regenerate Thread Title").disabled())
        .entry(ContextMenuEntry::new("Open Thread as Markdown").disabled())
        .separator()
        .entry(
            ContextMenuEntry::new("Archive Thread")
                .on_press(Message::ArchiveThread(thread_id.clone())),
        )
}

/// 解析 [`KEYMAP`]。
///
/// 解析不了就直接 `expect` 崩掉：键位表是编进二进制的内置数据，坏了属于开发期
/// 错误（`parses_the_bundled_keymap` 那条测试钉着），**不能**悄悄退回一张空表 ——
/// 那样表现是「快捷键全都失灵」，比启动即崩难查得多。
fn default_keymap() -> Keymap {
    Keymap::from_json(KEYMAP).expect("内置键位表应当能解析（有测试钉住）")
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
            tracing::warn!(%error, "打开链接失败");
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
    status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    use iced::mouse::Button;

    // 光标位置无条件记：`ButtonPressed` 不带坐标，而控件拖拽（侧边栏宽度手柄、
    // 滚动条）全靠它。
    if let iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) = event {
        return Some(Message::CursorMoved(position));
    }

    // 控件已经把这一下吃掉了（`button`/`mouse_area`/`text_input`/缩放热区都在按
    // 下时 `capture_event`），终端就不该再顺手开始选区、或者跟着滚动。
    if status == event::Status::Captured {
        return None;
    }

    match event {
        iced::Event::Mouse(mouse) => match mouse {
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
        iced::Event::Window(window::Event::Focused) => Some(Message::FocusChanged(true)),
        iced::Event::Window(window::Event::Unfocused) => Some(Message::FocusChanged(false)),
        _ => None,
    }
}

/// 窗口设置。
///
/// `decorations: false` = 自绘装饰（CSD）：标题栏由我们自己画（见 [`App::view`]），
/// 上游在 X11 上默认走服务端装饰、只在合成器支持时切 CSD，我们没那条路，直接自绘。
fn window_settings() -> window::Settings {
    window::Settings {
        size: iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT),
        decorations: false,
        ..window::Settings::default()
    }
}

/// 「先拿窗口 id，再发窗口动作」。
///
/// iced 里窗口动作都要 id，而 id 要异步问（`window::latest`）—— 单窗口应用也躲不掉
/// 这一步（iced 没给「主窗口」常量）。用 `then` 串上：`Task<Option<Id>>` → `Task<T>`。
fn window_task<F, T>(action: F) -> Task<T>
where
    F: Fn(window::Id) -> Task<T> + Send + 'static,
    T: Send + 'static,
{
    iced::window::latest().then(move |id| match id {
        Some(id) => action(id),
        None => Task::none(),
    })
}

/// 标题栏占掉的高度。
///
/// 渲染与坐标换算共用一份：终端区域从标题栏下面开始，命中判定（[`App::to_terminal`]）
/// 又必须与画布真实位置一致，差一像素就会「点在链接上却打不开」。
fn title_bar_height() -> f32 {
    aa_ui_iced::platform_title_bar_height(TITLE_BAR_TEXT_SIZE)
}

/// 标题栏的项目信息：当前工作目录所在仓库（没有就是目录名本身、无分支）。
///
/// 上游的项目来自「打开的 worktree」；我们只有终端会话，先用自己的 cwd。shell 里
/// `cd` 出去之后这里不会跟着变（拿不到 shell 的 cwd）。
fn title_bar_at_cwd() -> title_bar::TitleBar {
    let mut title_bar = title_bar::TitleBar::new();
    if let Ok(dir) = std::env::current_dir() {
        title_bar.set_project_at(&dir);
    }
    title_bar
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::Physical;
    use iced::keyboard::{Key, Modifiers, key::Named};
    // 只有测试里直接点名选项：生产路径把 [SearchMessage] 原样转给 adapter，
    // 自己不需要知道「正则」是哪个开关。
    use search::SearchOption;

    fn metrics() -> aa_terminal_iced::CellMetrics {
        aa_terminal_iced::CellMetrics::measure(
            iced::Font::MONOSPACE,
            aa_terminal_iced::BASE_FONT_SIZE,
        )
    }

    /// 终端主体的尺寸（窗口去掉标题栏）—— 与 [`App::terminal_size`] 同一份算法。
    fn body_size() -> iced::Size {
        iced::Size::new(INITIAL_WIDTH, INITIAL_HEIGHT - title_bar_height())
    }

    fn geometry() -> GridGeometry {
        aa_terminal_iced::geometry_for(body_size(), metrics())
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
            threads: agent_ui::ThreadMetadataStore::new(),
            // 与 `boot` 一样：侧边栏注册好了但收起。坐标类测试都以「终端铺满整个
            // 窗口」为前提（`center_of` 给的是画布局部坐标），收起时左侧偏移为 0；
            // 开着的情形由下面那几条专门测。
            multi_workspace: sidebar_shell(),
            sidebar_resize: false,
            theme: aa_theme_iced::Theme::dark(),
            menu: None,
            // 空标题栏：不读 cwd，测试才可复现（要用项目名的测试自己造）。
            title_bar: title_bar::TitleBar::new(),
            focused: true,
            keymap: default_keymap(),
        }
    }

    /// 给 app 的线程库塞几条线程，并让侧边栏重建列表。
    fn seed_threads(app: &mut App, threads: &[(&str, i64)]) {
        for (title, interacted_at) in threads {
            let mut metadata = agent_ui::ThreadMetadata::new(
                agent_ui::ThreadId::new(*title),
                chrono::DateTime::from_timestamp(*interacted_at, 0).expect("合法时间戳"),
            );
            metadata.title = Some((*title).to_owned());
            metadata.interacted_at =
                Some(chrono::DateTime::from_timestamp(*interacted_at, 0).expect("合法时间戳"));
            app.threads.save(metadata);
        }
        let store = &app.threads;
        if let Some(sidebar) = app.multi_workspace.sidebar_mut() {
            sidebar.rebuild_contents(store);
        }
    }

    /// 一个开着侧边栏的 app。
    fn sidebar_app() -> App {
        let mut app = test_app();
        let _ = app.update(Message::ToggleWorkspaceSidebar);
        app
    }

    /// 跑完 `view` 里这一轮点击产生的消息（模拟器里的点击要手动喂回去）。
    fn click(app: &mut App, target: &str) {
        let messages: Vec<Message> = {
            let mut ui = iced_test::simulator(app.view());
            assert!(ui.click(target).is_ok(), "应能点到 {target}");
            ui.into_messages().collect()
        };
        for message in messages {
            let _ = app.update(message);
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

    /// 按位拼修饰键。参数顺序与 core 那几条测试一致（shift, alt, ctrl）。
    fn mods(shift: bool, alt: bool, ctrl: bool) -> Modifiers {
        let mut modifiers = Modifiers::default();
        modifiers.set(Modifiers::SHIFT, shift);
        modifiers.set(Modifiers::ALT, alt);
        modifiers.set(Modifiers::CTRL, ctrl);
        modifiers
    }

    fn ctrl_shift() -> Modifiers {
        mods(true, false, true)
    }

    /// 键位表的语义测试都对着**真文件**测（`assets/keymaps/default-linux.json`）：
    /// 表从代码搬进 JSON 之后，光测解析器不够 —— 还得钉住「这份表到底绑了什么」。
    /// 原来这些是 `aa_terminal_core::mappings::keys` 里对着硬编码 if 链测的。
    fn keymap() -> Keymap {
        default_keymap()
    }

    /// 文件里每一条绑定都得查得回来，且没有多余条目 —— 数量写死是为了让「顺手
    /// 多绑一个键」这种改动必须在这里留痕（也与 zed 的做法一致：键位是显式的）。
    #[test]
    fn the_bundled_keymap_binds_every_action() {
        use aa_terminal_core::mappings::keymap::parse_keystroke;

        let keymap = keymap();
        let expected = [
            ("ctrl-shift-c", AppShortcut::Copy),
            ("ctrl-shift-v", AppShortcut::Paste),
            ("ctrl-shift-f", AppShortcut::OpenSearch),
            ("shift-up", AppShortcut::ScrollLineUp),
            ("shift-down", AppShortcut::ScrollLineDown),
            ("shift-pageup", AppShortcut::ScrollPageUp),
            ("shift-pagedown", AppShortcut::ScrollPageDown),
            ("shift-home", AppShortcut::ScrollToTop),
            ("shift-end", AppShortcut::ScrollToBottom),
            ("ctrl-alt-j", AppShortcut::ToggleWorkspaceSidebar),
        ];
        assert_eq!(keymap.bindings().len(), expected.len(), "绑定条数变了");
        for (keystroke, action) in expected {
            let (modifiers, key) = parse_keystroke(keystroke).expect("应能解析");
            assert_eq!(
                keymap.action_for(&key, modifiers),
                Some(action),
                "{keystroke} 应当绑到 {action:?}"
            );
        }
    }

    /// Ctrl+Shift+C/V/F 是应用动作，裸 Ctrl+C/V 必须留给 shell。
    #[test]
    fn app_shortcuts_require_shift() {
        let keymap = keymap();
        assert_eq!(
            keymap.action_for(&Key::Character("C".into()), ctrl_shift()),
            Some(AppShortcut::Copy)
        );
        assert_eq!(
            keymap.action_for(&Key::Character("V".into()), ctrl_shift()),
            Some(AppShortcut::Paste)
        );
        assert_eq!(
            keymap.action_for(&Key::Character("F".into()), ctrl_shift()),
            Some(AppShortcut::OpenSearch)
        );
        // 裸 Ctrl：得进 shell（Ctrl+C 是中断信号）
        for character in ["c", "v", "f"] {
            assert_eq!(
                keymap.action_for(&Key::Character(character.into()), ctrl()),
                None,
                "裸 Ctrl+{character} 不得被应用抢走"
            );
        }
    }

    /// 侧边栏开关是 Ctrl+Alt+J，多按一个 Shift 就不算（否则会和别的绑定额外撞车）。
    #[test]
    fn the_sidebar_shortcut_is_ctrl_alt_j_without_shift() {
        let keymap = keymap();
        assert_eq!(
            keymap.action_for(&Key::Character("j".into()), mods(false, true, true)),
            Some(AppShortcut::ToggleWorkspaceSidebar)
        );
        assert_eq!(
            keymap.action_for(&Key::Character("J".into()), mods(true, true, true)),
            None,
            "带了 Shift 就不该命中"
        );
        assert_eq!(keymap.action_for(&Key::Character("j".into()), ctrl()), None);
        assert_eq!(
            keymap.action_for(&Key::Character("j".into()), mods(false, true, false)),
            None
        );
    }

    /// 滚动快捷键是 Shift+方向/翻页键；不带修饰键的那些要留给程序（shell、
    /// vim、less 都要用）。
    #[test]
    fn shifted_named_keys_are_scroll_shortcuts() {
        let keymap = keymap();
        let shift = mods(true, false, false);
        for (key, action) in [
            (Named::PageUp, AppShortcut::ScrollPageUp),
            (Named::PageDown, AppShortcut::ScrollPageDown),
            (Named::ArrowUp, AppShortcut::ScrollLineUp),
            (Named::ArrowDown, AppShortcut::ScrollLineDown),
            (Named::Home, AppShortcut::ScrollToTop),
            (Named::End, AppShortcut::ScrollToBottom),
        ] {
            assert_eq!(
                keymap.action_for(&Key::Named(key), shift),
                Some(action),
                "Shift+{key:?} 应当绑到 {action:?}"
            );
            assert_eq!(
                keymap.action_for(&Key::Named(key), Modifiers::default()),
                None,
                "裸 {key:?} 应当留给程序"
            );
        }
    }

    /// 滚动快捷键不能顺手抢走带 Control 的组合（less 之类大量用 Ctrl+PageUp）。
    #[test]
    fn scroll_shortcuts_do_not_steal_control_combinations() {
        let keymap = keymap();
        assert_eq!(
            keymap.action_for(&Key::Named(Named::PageUp), ctrl_shift()),
            None
        );
        assert_eq!(
            keymap.action_for(&Key::Named(Named::ArrowUp), ctrl_shift()),
            None
        );
    }

    #[test]
    fn other_shifted_combos_are_not_app_shortcuts() {
        let keymap = keymap();
        assert_eq!(
            keymap.action_for(&Key::Character("z".into()), ctrl_shift()),
            None
        );
        assert_eq!(
            keymap.action_for(&Key::Named(Named::Escape), ctrl_shift()),
            None
        );
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
        // 打开栏再灌查询词：栏的消息只在栏开着时才被处理（key/输入都走那条
        // 路），绕过打开流程发的词会被丢掉。
        let _ = app.update(Message::OpenSearch);
        let _ = app.update(Message::Search(SearchMessage::Query("needle".into())));

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
        let _ = app.update(Message::OpenSearch);
        let _ = app.update(Message::Search(SearchMessage::Query("needle".into())));
        app
    }

    #[test]
    fn enter_jumps_to_the_first_match_then_the_next() {
        let mut app = search_app();
        assert_eq!(app.terminal.search_match_count(), 3);
        assert_eq!(
            app.terminal.search_current_index(),
            None,
            "还没回车时不该有当前项"
        );

        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        assert_eq!(
            app.terminal.search_current_index(),
            Some(0),
            "第一次回车应落在第一处"
        );
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        assert_eq!(app.terminal.search_current_index(), Some(1));
    }

    #[test]
    fn shift_enter_jumps_backwards() {
        let mut app = search_app();
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        let _ = app.update(pressed(Key::Named(Named::Enter), Modifiers::default()));
        assert_eq!(app.terminal.search_current_index(), Some(1));

        let _ = app.update(pressed(Key::Named(Named::Enter), shift()));
        assert_eq!(
            app.terminal.search_current_index(),
            Some(0),
            "Shift+回车应回上一处"
        );
    }

    #[test]
    fn turning_regex_off_in_the_bar_switches_to_a_literal_search() {
        // 打开栏时选项默认全开（终端支持正则），所以 `.` 先当通配符；点一下
        // `.*` 关掉之后必须按字面量重搜。这条同时钉住「默认选项」的接线 ——
        // 默认值一改，用户的第一次搜索行为就变了。
        let mut app = test_app_with(b"abc and a.c\r\n");
        let _ = app.update(Message::OpenSearch);
        let _ = app.update(Message::Search(SearchMessage::Query("a.c".into())));
        assert_eq!(app.terminal.search_match_count(), 2, "默认正则：`.` 通配");

        let _ = app.update(Message::Search(SearchMessage::Toggle(SearchOption::Regex)));
        assert_eq!(app.terminal.search_match_count(), 1, "关掉正则后按字面量搜");

        let _ = app.update(Message::Search(SearchMessage::Toggle(SearchOption::Regex)));
        assert_eq!(app.terminal.search_match_count(), 2, "再点回来又是正则");
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
        mods(false, false, true)
    }

    /// 第 `line` 行第 `column` 列那个 cell 正中间的像素位置（**窗口**坐标 ——
    /// 终端区域从标题栏下面开始）。
    fn center_of(line: usize, column: usize) -> iced::Point {
        let metrics = metrics();
        iced::Point::new(
            metrics.cell_width * (column as f32 + 0.5),
            title_bar_height() + metrics.line_height * (line as f32 + 0.5),
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

    fn ctrl_alt() -> Modifiers {
        mods(false, true, true)
    }

    #[test]
    fn ctrl_alt_j_toggles_the_sidebar() {
        let mut app = test_app();
        assert!(!app.multi_workspace.sidebar_open(), "默认收起");

        let _ = app.update(pressed(Key::Character("j".into()), ctrl_alt()));
        assert!(
            app.multi_workspace.sidebar_open(),
            "Ctrl+Alt+J 应打开侧边栏"
        );

        let _ = app.update(pressed(Key::Character("j".into()), ctrl_alt()));
        assert!(!app.multi_workspace.sidebar_open(), "再按一次应收起");
    }

    #[test]
    fn opening_the_sidebar_narrows_the_terminal_grid() {
        // 侧边栏是主区的**兄弟节点**，不是浮层：终端可用宽度变小，网格列数与 PTY
        // 尺寸都得跟着重算 —— 否则 shell 会按旧宽度折行，内容折到屏幕外。
        let mut app = test_app();
        let before = app.geometry.num_columns();

        let _ = app.update(Message::ToggleWorkspaceSidebar);

        let width = app.multi_workspace.sidebar_width();
        assert_eq!(
            app.terminal_size().width,
            app.viewport.width - width - workspace::SIDEBAR_RESIZE_HANDLE_SIZE,
            "终端可用宽度 = 窗口宽 - 侧边栏宽 - 手柄"
        );
        assert!(
            app.geometry.num_columns() < before,
            "列数应变少：{before} → {}",
            app.geometry.num_columns()
        );
        assert_eq!(
            app.geometry.num_columns(),
            aa_terminal_iced::geometry_for(app.terminal_size(), app.metrics).num_columns(),
            "网格应按新宽度重排"
        );

        let _ = app.update(Message::ToggleWorkspaceSidebar);
        assert_eq!(app.geometry.num_columns(), before, "收起后应回到原样");
    }

    #[test]
    fn a_click_on_the_sidebar_does_not_start_a_selection() {
        // 侧边栏压在终端左边：点它不该顺带选中终端第一列。
        let mut app = sidebar_app();
        let _ = app.update(Message::CursorMoved(iced::Point::new(10., 10.)));
        let _ = app.update(Message::MouseDown);
        assert!(!app.dragging, "侧边栏上的按下不归终端");
        assert!(app.terminal.selected_text().is_none_or(|t| t.is_empty()));
    }

    #[test]
    fn the_new_thread_button_adds_a_thread_that_shows_up_in_the_list() {
        let mut app = sidebar_app();
        assert_eq!(app.threads.entries().count(), 0);

        click(&mut app, "＋");

        assert_eq!(app.threads.entries().count(), 1, "点 ＋ 应建一条线程");
        let mut ui = iced_test::simulator(app.view());
        assert!(
            ui.find(agent_ui::DEFAULT_THREAD_TITLE).is_ok(),
            "新线程应出现在列表里（草稿用占位标题）"
        );
    }

    #[test]
    fn clicking_a_row_activates_that_thread() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10), ("标题-新", 20)]);

        click(&mut app, "标题-旧");

        let sidebar = app.multi_workspace.sidebar().expect("侧边栏还在");
        assert_eq!(
            sidebar.active_entry(),
            Some(&sidebar::ActiveEntry::Thread {
                thread_id: agent_ui::ThreadId::new("标题-旧")
            }),
            "点哪一行就该选中哪一行"
        );
    }

    #[test]
    fn dragging_the_handle_sizes_the_sidebar_and_the_grid_follows() {
        let mut app = sidebar_app();
        let _ = app.update(Message::SidebarResizeStart);
        assert!(app.sidebar_resize);

        let _ = app.update(Message::CursorMoved(iced::Point::new(380., 5.)));
        assert_eq!(app.multi_workspace.sidebar_width(), 380.);
        assert_eq!(
            app.geometry.num_columns(),
            aa_terminal_iced::geometry_for(app.terminal_size(), app.metrics).num_columns(),
            "拖宽过程中网格要跟着走"
        );
        assert!(!app.dragging, "拖手柄不该同时开始选区");

        let _ = app.update(Message::MouseUp);
        assert!(!app.sidebar_resize, "松手应结束拖拽");
    }

    #[test]
    fn double_clicking_the_handle_restores_the_default_width() {
        let mut app = sidebar_app();
        let default_width = app.multi_workspace.sidebar_width();

        let _ = app.update(Message::SidebarResizeStart);
        let _ = app.update(Message::CursorMoved(iced::Point::new(700., 5.)));
        assert_ne!(app.multi_workspace.sidebar_width(), default_width);

        let _ = app.update(Message::SidebarResetWidth);
        assert_eq!(app.multi_workspace.sidebar_width(), default_width);
        assert!(!app.sidebar_resize, "双击复位后不该还停在拖拽态");
    }

    /// 右键某一行的线程：菜单该出现在指针那儿，条目与 zed 一致（4 条，中间隔一条）。
    #[test]
    fn right_clicking_a_row_opens_the_menu_at_the_cursor() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let _ = app.update(Message::CursorMoved(iced::Point::new(120., 40.)));

        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            agent_ui::ThreadId::new("标题-旧"),
        )));

        let menu = app.menu.as_ref().expect("右键应弹出菜单");
        assert_eq!(
            menu.position(),
            iced::Point::new(120., 40.),
            "菜单贴在指针处"
        );
        assert_eq!(
            menu.item_count(),
            4,
            "Rename/Regenerate/Open as Markdown/Archive"
        );
    }

    /// 菜单形状照 zed：两条我们还没有依赖的（重生成标题、按 markdown 打开）置灰 ——
    /// 点它们不产出消息，菜单也不关。
    #[test]
    fn the_entries_without_backends_are_shown_but_disabled() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            agent_ui::ThreadId::new("标题-旧"),
        )));

        let messages: Vec<Message> = {
            let mut ui = iced_test::simulator(app.view());
            assert!(
                ui.click("Regenerate Thread Title").is_ok(),
                "置灰条目也要画出来（照上游显示、不省略）"
            );
            ui.into_messages().collect()
        };
        assert!(messages.is_empty(), "置灰条目点不动：{messages:?}");
        assert!(app.menu.is_some(), "点置灰条目不该关掉菜单");
    }

    /// 点「Rename Title」→ 菜单收起、那一行就地变成输入框（初值是现标题）。
    #[test]
    fn rename_from_the_menu_opens_the_inline_editor() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let thread_id = agent_ui::ThreadId::new("标题-旧");
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            thread_id.clone(),
        )));

        click(&mut app, "Rename Title");

        assert!(app.menu.is_none(), "选了条目菜单就收起来");
        let sidebar = app.multi_workspace.sidebar().expect("侧边栏还在");
        assert_eq!(sidebar.renaming_thread_id(), Some(&thread_id));
        assert_eq!(sidebar.renaming_text(), Some("标题-旧"));
    }

    /// 重命名提交后写进 store（`title_override` 压过原 title），列表跟着刷新。
    #[test]
    fn committing_the_rename_writes_the_title_override() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let thread_id = agent_ui::ThreadId::new("标题-旧");

        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            thread_id.clone(),
        )));
        click(&mut app, "Rename Title");
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::RenameInput(
            "新名字".to_owned(),
        )));
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::RenameCommit));

        let entry = app.threads.entry(&thread_id).expect("线程还在 store 里");
        assert_eq!(entry.title_override.as_deref(), Some("新名字"));
        assert_eq!(entry.display_title(), "新名字", "显示名该跟着覆盖走");

        let sidebar = app.multi_workspace.sidebar().expect("侧边栏还在");
        assert_eq!(sidebar.renaming_thread_id(), None, "提交后退出重命名");
        assert_eq!(
            sidebar.contents().entries.len(),
            1,
            "还是那一条，只是换了名字"
        );
    }

    /// 空标题当取消：不动 store（上游空标题也不写）。
    #[test]
    fn committing_an_empty_rename_leaves_the_title_alone() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let thread_id = agent_ui::ThreadId::new("标题-旧");
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            thread_id.clone(),
        )));
        click(&mut app, "Rename Title");
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::RenameInput(
            "   ".to_owned(),
        )));
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::RenameCommit));

        assert_eq!(
            app.threads.entry(&thread_id).expect("还在").title_override,
            None,
            "空白标题不该写进去"
        );
        assert_eq!(
            app.multi_workspace
                .sidebar()
                .expect("侧边栏还在")
                .renaming_thread_id(),
            None,
            "退出重命名"
        );
    }

    /// Esc 取消重命名：store 不动，那一行回到普通行。
    #[test]
    fn cancelling_the_rename_keeps_the_old_title() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let thread_id = agent_ui::ThreadId::new("标题-旧");
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            thread_id.clone(),
        )));
        click(&mut app, "Rename Title");
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::RenameInput(
            "打了一半".to_owned(),
        )));
        let _ = app.update(pressed(Key::Named(Named::Escape), Modifiers::default()));

        let sidebar = app.multi_workspace.sidebar().expect("侧边栏还在");
        assert_eq!(sidebar.renaming_thread_id(), None, "Esc 退出重命名");
        assert_eq!(
            app.threads.entry(&thread_id).expect("还在").title_override,
            None,
            "取消不动 store"
        );
    }

    /// 点「Archive Thread」→ 归档进 store（列表里不再出现），选中也清掉。
    #[test]
    fn archive_from_the_menu_drops_the_row_and_the_selection() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10), ("标题-新", 20)]);
        let thread_id = agent_ui::ThreadId::new("标题-旧");
        let _ = app.update(Message::MouseDown);
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ActivateEntry(
            thread_id.clone(),
        )));
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            thread_id.clone(),
        )));

        click(&mut app, "Archive Thread");

        assert!(app.menu.is_none(), "选了条目菜单就收起来");
        assert!(
            app.threads
                .entry(&thread_id)
                .expect("还在 store 里")
                .archived,
            "归档标记要落到 store"
        );
        let sidebar = app.multi_workspace.sidebar().expect("侧边栏还在");
        assert!(
            !sidebar.contents().entries.iter().any(|entry| matches!(
                entry,
                sidebar::ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id
            )),
            "归档的那行要从列表里消失"
        );
        assert_eq!(
            sidebar.active_entry(),
            None,
            "归档的正是打开着的那条，选中要清"
        );
        assert_eq!(
            app.threads.archived_entries().count(),
            1,
            "归档视图那边查得到"
        );
    }

    /// Esc 优先级：菜单开着时先关菜单，别去动搜索框。
    #[test]
    fn escape_closes_the_menu_before_the_search_box() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let _ = app.update(Message::OpenSearch);
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            agent_ui::ThreadId::new("标题-旧"),
        )));

        let _ = app.update(pressed(Key::Named(Named::Escape), Modifiers::default()));
        assert!(app.menu.is_none(), "Esc 先关菜单");
        assert!(app.search.is_some(), "搜索框这时还开着");

        let _ = app.update(pressed(Key::Named(Named::Escape), Modifiers::default()));
        assert!(app.search.is_none(), "再按一次才轮到搜索框");
    }

    /// 菜单是窗口级浮层：叠在终端与侧边栏之上（渲染得出来）。
    #[test]
    fn the_menu_renders_over_the_whole_window() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            agent_ui::ThreadId::new("标题-旧"),
        )));

        let mut ui = iced_test::simulator(app.view());
        assert!(ui.find("Rename Title").is_ok(), "菜单条目要画出来");
        assert!(ui.find("Archive Thread").is_ok());
    }

    /// 点菜单外面（整屏垫层）收起菜单，不误伤终端。
    #[test]
    fn clicking_outside_the_menu_dismisses_it() {
        let mut app = sidebar_app();
        seed_threads(&mut app, &[("标题-旧", 10)]);
        let _ = app.update(Message::Sidebar(sidebar::SidebarMessage::ShowContextMenu(
            agent_ui::ThreadId::new("标题-旧"),
        )));

        // 左上角离菜单（贴在 0,0 附近时菜单会占住左上）以外的位置点一下。
        let messages: Vec<Message> = {
            let mut ui = iced_test::simulator(app.view());
            ui.point_at(iced::Point::new(600.0, 500.0));
            ui.simulate([
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
            ]);
            ui.into_messages().collect()
        };
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::CloseContextMenu)),
            "垫层应产出关闭消息：{messages:?}"
        );
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

    // ---- 标题栏（自绘 CSD）------------------------------------------------

    /// 一个带项目名与分支的标题栏 —— 在临时目录里建个 `.git/HEAD`，`set_project_at`
    /// 才会认出来。目录名带进程号，所以断言用 `display_project_name()` 拿画出来的
    /// 那个（超长会被截断）。
    fn title_bar_in_temp_repo(name: &str, branch: &str) -> title_bar::TitleBar {
        let dir = std::env::temp_dir().join(format!(
            "aa_term_title_bar_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).expect("建临时仓库");
        std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}")).expect("写 HEAD");
        let _ = name;

        let mut bar = title_bar::TitleBar::new();
        assert!(bar.set_project_at(&dir), "临时目录应被认成仓库根");
        bar
    }

    /// 自绘装饰（CSD）：窗口自己的装饰要关掉，标题栏归我们画。
    #[test]
    fn the_window_is_undecorated_so_we_can_draw_the_title_bar_ourselves() {
        assert!(!window_settings().decorations, "自绘标题栏必须关掉窗口装饰");
    }

    /// 大头：菜单栏 8 项、项目名、分支都画得出来。
    #[test]
    fn the_title_bar_shows_the_menus_the_project_and_the_branch() {
        let mut app = test_app();
        app.title_bar = title_bar_in_temp_repo("标题栏", "feature/标题栏");

        let name = app.title_bar.display_project_name().expect("有项目名");
        let mut ui = iced_test::simulator(app.view());
        for label in [
            "File",
            "Edit",
            "Selection",
            "View",
            "Go",
            "Run",
            "Window",
            "Help",
        ] {
            assert!(ui.find(label).is_ok(), "菜单栏应有 {label}");
        }
        assert!(ui.find(name.as_str()).is_ok(), "应画出项目名");
        assert!(ui.find("feature/标题栏").is_ok(), "应画出分支");
    }

    /// 窗口按钮是自绘的（CSD 才有）：最小化/最大化/关闭三个字形。
    #[test]
    fn the_window_buttons_are_drawn_on_the_right() {
        let app = test_app();
        let mut ui = iced_test::simulator(app.view());
        for glyph in ["\u{2212}", "\u{25a1}", "\u{2715}"] {
            assert!(ui.find(glyph).is_ok(), "应有窗口按钮 {glyph:?}");
        }
    }

    /// 点菜单栏里的 `View` 弹出它的菜单（内容与上游一致：Toggle Sidebar）。
    #[test]
    fn clicking_a_menu_button_opens_its_menu() {
        let mut app = test_app();
        click(&mut app, "View");

        // `item_count` 数的是可点的条目（分隔线不算）：Toggle Sidebar + 3 个禁用项。
        assert_eq!(app.menu.as_ref().expect("应弹出 View 菜单").item_count(), 4);

        let mut ui = iced_test::simulator(app.view());
        assert!(ui.find("Toggle Sidebar").is_ok(), "菜单条目要画出来");
    }

    /// 菜单条目走的是同一批动作：`Toggle Sidebar` 真的收起/展开侧边栏。
    #[test]
    fn the_toggle_sidebar_entry_toggles_the_sidebar() {
        let mut app = test_app();
        click(&mut app, "View");

        let _ = app.update(Message::TitleBarAction(
            title_bar::MenuAction::ToggleSidebar,
        ));
        assert!(app.multi_workspace.sidebar().is_some(), "应展开侧边栏");
        assert!(app.menu.is_none(), "选完条目菜单要收起来");
    }

    /// Esc 优先关标题栏菜单（与侧边栏右键菜单同一条路）。
    #[test]
    fn escape_closes_the_title_bar_menu_first() {
        let mut app = test_app();
        let _ = app.update(Message::OpenSearch);
        click(&mut app, "File");
        assert!(app.menu.is_some());

        let _ = app.update(pressed(Key::Named(Named::Escape), Modifiers::default()));
        assert!(app.menu.is_none(), "Esc 先关菜单");
        assert!(app.search.is_some(), "搜索框这时还开着");
    }

    /// `Reveal in File Manager` 走终端的同一个 opener（目录也能开）。
    #[test]
    fn revealing_the_project_calls_the_opener() {
        static OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        fn record(uri: &str) {
            OPENED.lock().expect("锁没坏").push(uri.to_owned());
        }
        fn take() -> Vec<String> {
            std::mem::take(&mut *OPENED.lock().expect("锁没坏"))
        }

        let _ = take();
        let mut app = test_app();
        app.opener = record;
        app.title_bar = title_bar_in_temp_repo("标题栏", "main");
        let path = app
            .title_bar
            .project_path()
            .expect("有项目路径")
            .to_path_buf();

        let _ = app.update(Message::TitleBarAction(
            title_bar::MenuAction::RevealProject,
        ));
        assert_eq!(take(), vec![path.display().to_string()]);
    }

    /// 窗口按钮与 Window 菜单是同一批动作；这里只钉住映射（真的窗口调用是副作用，
    /// 测试拿不到）。
    #[test]
    fn the_window_buttons_map_to_the_same_actions_as_the_menu() {
        let mut app = test_app();
        let before = app.multi_workspace.sidebar().is_some();

        // 关闭按钮 → 关窗口（单窗口应用即退出）；这条只验证不会接到别的动作上：
        // 侧边栏状态与菜单都不该被碰到。
        let _ = app.update(Message::WindowControl(WindowControl::Close));
        assert_eq!(app.multi_workspace.sidebar().is_some(), before);
        assert!(app.menu.is_none());
    }

    /// 焦点变化进状态：标题栏底色按它选（失焦变暗）。
    #[test]
    fn focus_change_updates_the_title_bar() {
        let mut app = test_app();
        assert!(app.focused, "刚开窗应该有焦点");

        let _ = app.update(Message::FocusChanged(false));
        assert!(!app.focused);
        let _ = app.update(Message::FocusChanged(true));
        assert!(app.focused);
    }

    /// 缩放方向有两个值一样 —— `iced::window::Direction` 没实现 `PartialEq`
    /// （上游只按 variant 用），测试就按判别式比。
    fn same_direction(left: &iced::window::Direction, right: &iced::window::Direction) -> bool {
        std::mem::discriminant(left) == std::mem::discriminant(right)
    }

    /// 窗口边缘按下 → 对应方向的缩放消息（四角优先于四条边）。
    #[test]
    fn pressing_a_window_edge_resizes_towards_that_direction() {
        let (width, height) = (INITIAL_WIDTH, INITIAL_HEIGHT);
        let cases: [(iced::Point, iced::window::Direction); 8] = [
            (
                iced::Point::new(2.0, 2.0),
                iced::window::Direction::NorthWest,
            ),
            (
                iced::Point::new(width - 1.0, 2.0),
                iced::window::Direction::NorthEast,
            ),
            (
                iced::Point::new(2.0, height - 2.0),
                iced::window::Direction::SouthWest,
            ),
            (
                iced::Point::new(width - 1.0, height - 2.0),
                iced::window::Direction::SouthEast,
            ),
            (
                iced::Point::new(width / 2.0, 2.0),
                iced::window::Direction::North,
            ),
            (
                iced::Point::new(width / 2.0, height - 2.0),
                iced::window::Direction::South,
            ),
            (
                iced::Point::new(2.0, height / 2.0),
                iced::window::Direction::West,
            ),
            (
                iced::Point::new(width - 1.0, height / 2.0),
                iced::window::Direction::East,
            ),
        ];

        for (position, direction) in cases {
            let app = test_app();
            let messages: Vec<Message> = {
                let mut ui = iced_test::simulator(app.view());
                ui.point_at(position);
                ui.simulate([iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                    iced::mouse::Button::Left,
                ))]);
                ui.into_messages().collect()
            };
            assert!(
                messages.iter().any(|message| matches!(
                    message,
                    Message::ResizeWindow(d) if same_direction(d, &direction)
                )),
                "{position:?} 应缩放 {direction:?}：{messages:?}"
            );
        }
    }

    /// 标题栏空白处按下 → 拖窗口（缩放热区只占边缘，别把整条标题栏都吃了）。
    #[test]
    fn pressing_the_empty_title_bar_drags_the_window() {
        let app = test_app();
        let messages: Vec<Message> = {
            let mut ui = iced_test::simulator(app.view());
            ui.point_at(iced::Point::new(INITIAL_WIDTH / 2.0, 10.0));
            ui.simulate([iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Left,
            ))]);
            ui.into_messages().collect()
        };
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::DragWindow)),
            "标题栏空白处应拖窗口：{messages:?}"
        );
    }

    /// 控件吃掉的按下不再传到终端：靠边缘那条缩放热区按下时，不该再开始选区。
    #[test]
    fn a_captured_press_does_not_start_a_selection() {
        // `Captured` 是 iced 在控件 `capture_event` 之后给的状态。
        assert!(
            on_mouse_event(
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                event::Status::Captured,
                window::Id::unique(),
            )
            .is_none(),
            "被控件吃掉的按下不该变成终端的 MouseDown"
        );
        // 光标位置还是要记（控件拖拽、菜单定位都靠它）。
        assert!(matches!(
            on_mouse_event(
                iced::Event::Mouse(iced::mouse::Event::CursorMoved {
                    position: iced::Point::new(1.0, 2.0)
                }),
                event::Status::Captured,
                window::Id::unique(),
            ),
            Some(Message::CursorMoved(position)) if position == iced::Point::new(1.0, 2.0)
        ));
    }

    /// 菜单栏总是画在窗口最上面（含侧边栏那一行之上）。
    #[test]
    fn the_title_bar_spans_the_whole_window() {
        let app = sidebar_app();
        let mut ui = iced_test::simulator(app.view());
        assert!(ui.find("Selection").is_ok(), "侧边栏开着时菜单栏也在");
    }
}
