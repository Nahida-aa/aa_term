//! 承载侧边栏的那一层 —— 对应 Zed 的 `crates/workspace/src/multi_workspace.rs`。
//!
//! Zed 里这条链是这样的：`MultiWorkspace` 持有 `Option<Box<dyn SidebarHandle>>`，
//! 侧边栏（`crates/sidebar`，线程列表）实现 `workspace::Sidebar` 并注册进来；
//! 具体是谁它不知道，所以 `workspace` 不必依赖 `sidebar`，两者不构成循环依赖。
//! 布局上侧边栏是**工作区的兄弟节点**（不是浮层）：一行里
//! `[侧边栏?, 工作区, 侧边栏?]`，中间夹一条 6px 的拖拽手柄。
//!
//! **与上游的差别**（有意为之，逐条记在这里）：
//!
//! - **侧边栏类型是泛型参数，不是 `Box<dyn Sidebar>`**。上游那对
//!   `Sidebar` / `SidebarHandle` 双 trait 存在，只是为了把 gpui 的 `Entity<T>` 塞
//!   进 `MultiWorkspace`（`impl<T: Sidebar> SidebarHandle for Entity<T>`）—— 面板要
//!   拿到 `Entity` 才能跨处共享状态。iced 没有 entity 这层间接，而且宿主还需要
//!   **在 trait 之外**驱动侧边栏自己的操作（如「新建线程」），所以这里直接用
//!   `MultiWorkspace<S: Sidebar>`：`register_sidebar` 收的是实体本身，
//!   `sidebar_mut()` 给回 `&mut S`，两个需求一起满足。
//! - **`Sidebar` 多了 `type Message` 与 `view`**：gpui 的 `Render` 超 trait 换成
//!   iced 的「返回自己的消息 + 由宿主 `map` 上去」，这样 `layout` 能替宿主把侧边栏
//!   摆好位置，而消息类型不必是 workspace 知道的类型。
//! - **没搬 `SidebarEvent`**（`SerializeNeeded`）与 `Focusable`/`EventEmitter`：
//!   iced 里没有对应物。持久化真要做时，再决定是返回值还是消息。
//! - `Sidebar::prepare_for_focus` / `toggle_thread_switcher` / `cycle_project` /
//!   `cycle_thread` 是键盘导航用的，等侧边栏有了键盘导航再补。
//! - [`SidebarSide`] 与宽度上下限的**位置**与上游不同：`SidebarSide` 上游在
//!   `settings_content::agent`，宽度上下限在 `agent_settings`（`THREADS_LIST_MIN_WIDTH`
//!   / `MAX_WIDTH`）。我们还没有这两个 crate，先放在这里 —— 等 settings 起来再挪，
//!   名字不变。

use aa_theme_iced::Theme as AppTheme;
use iced::widget::{Space, mouse_area, row};
use iced::{Element, Fill, Length};

/// 侧边栏与工作区之间那条拖拽手柄的宽度（像素）。
///
/// 与 Zed 的 `SIDEBAR_RESIZE_HANDLE_SIZE` 同名同值。
pub const SIDEBAR_RESIZE_HANDLE_SIZE: f32 = 6.0;

/// 线程列表的宽度下限。
///
/// 与 Zed 的 `agent_settings::THREADS_LIST_MIN_WIDTH` 同名同值。
pub const THREADS_LIST_MIN_WIDTH: f32 = 200.0;

/// 线程列表的宽度上限。
///
/// 与 Zed 的 `agent_settings::THREADS_LIST_MAX_WIDTH` 同名同值。
pub const THREADS_LIST_MAX_WIDTH: f32 = 800.0;

/// 侧边栏停在哪一侧。
///
/// 上游是 `settings_content::agent` 的枚举（见模块头的位置说明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarSide {
    /// 左侧（默认）。
    #[default]
    Left,
    /// 右侧。
    Right,
}

impl SidebarSide {
    /// 是不是在右侧。
    pub fn is_right(self) -> bool {
        matches!(self, SidebarSide::Right)
    }

    /// 另一侧。
    pub fn opposite(self) -> SidebarSide {
        match self {
            SidebarSide::Left => SidebarSide::Right,
            SidebarSide::Right => SidebarSide::Left,
        }
    }
}

/// 侧边栏当前的可见性状态。
///
/// 与 Zed 的 `SidebarRenderState` 同名字段：`open` + `side`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarRenderState {
    /// 是否展开。
    pub open: bool,
    /// 停在哪一侧。
    pub side: SidebarSide,
}

/// 能被 `MultiWorkspace` 挂上去的侧边栏。
///
/// 契约只保留 iced 侧用得上的部分：宽度与停靠侧由侧边栏自己持有（上游也是这么
/// 分的 —— `MultiWorkspace` 只是转手调用），因为宽度上下限是侧边栏的知识。
pub trait Sidebar {
    /// 侧边栏抛给宿主的消息类型。
    ///
    /// 渲染时由宿主 `map` 成自己的消息（iced 的 `Element::map`），所以
    /// `workspace` 不必知道它具体是什么。
    type Message;

    /// 渲染整条侧边栏。
    ///
    /// 主题要**显式传进来**：gpui 那边侧边栏是从 `cx.theme()` 这个环境里取色的，
    /// iced 没有等价物，而主题归 app 所有（将来切深/浅色只有一处状态）。
    fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Self::Message>;

    /// 当前宽度（像素）。
    fn width(&self) -> f32;

    /// 设置宽度；`None` 表示回到默认宽度。
    ///
    /// 实现方负责夹到 [`THREADS_LIST_MIN_WIDTH`] / [`THREADS_LIST_MAX_WIDTH`]。
    fn set_width(&mut self, width: Option<f32>);

    /// 停靠侧。
    fn side(&self) -> SidebarSide {
        SidebarSide::Left
    }

    /// 当前显示的是不是线程列表（而不是归档视图之类）。
    ///
    /// 上游用它决定「打开侧边栏要 focus 到哪」；这里先留着，通知/焦点逻辑接上时
    /// 就有用。
    fn is_threads_list_view_active(&self) -> bool {
        true
    }
}

/// 工作区 + 侧边栏的外壳。
///
/// 只管「侧边栏开着没、它多宽、停在哪一侧」，不持有列表内容。
#[derive(Debug, Default)]
pub struct MultiWorkspace<S> {
    sidebar: Option<S>,
    sidebar_open: bool,
}

impl<S: Sidebar> MultiWorkspace<S> {
    /// 空外壳（没有侧边栏、没展开）。
    pub fn new() -> MultiWorkspace<S> {
        MultiWorkspace {
            sidebar: None,
            sidebar_open: false,
        }
    }

    /// 挂上侧边栏。
    ///
    /// 与 Zed 的 `register_sidebar` 同名：注册本身**不**改变展开状态，展开是
    /// `open_sidebar` / `toggle_sidebar` 的事。
    pub fn register_sidebar(&mut self, sidebar: S) {
        self.sidebar = Some(sidebar);
    }

    /// 取侧边栏。
    pub fn sidebar(&self) -> Option<&S> {
        self.sidebar.as_ref()
    }

    /// 改侧边栏（宿主驱动它自己的操作，比如「新建线程」）。
    pub fn sidebar_mut(&mut self) -> Option<&mut S> {
        self.sidebar.as_mut()
    }

    /// 侧边栏是否展开。
    pub fn sidebar_open(&self) -> bool {
        self.sidebar.is_some() && self.sidebar_open
    }

    /// 侧边栏停靠侧。
    pub fn sidebar_side(&self) -> SidebarSide {
        self.sidebar.as_ref().map(Sidebar::side).unwrap_or_default()
    }

    /// 渲染侧边栏需要的那点状态。
    pub fn sidebar_render_state(&self) -> SidebarRenderState {
        SidebarRenderState {
            open: self.sidebar_open(),
            side: self.sidebar_side(),
        }
    }

    /// 打开（没挂侧边栏时什么也不做）。
    pub fn open_sidebar(&mut self) {
        if self.sidebar.is_some() {
            self.sidebar_open = true;
        }
    }

    /// 关闭。
    pub fn close_sidebar(&mut self) {
        self.sidebar_open = false;
    }

    /// 开关。
    pub fn toggle_sidebar(&mut self) {
        if self.sidebar_open() {
            self.close_sidebar();
        } else {
            self.open_sidebar();
        }
    }

    /// 侧边栏当前宽度（没挂侧边栏时是 0）。
    pub fn sidebar_width(&self) -> f32 {
        self.sidebar.as_ref().map(Sidebar::width).unwrap_or(0.)
    }

    /// 拖拽改宽；`None` 回到默认宽度。
    ///
    /// 夹取范围由侧边栏自己负责（它才知道上下限）。
    pub fn set_sidebar_width(&mut self, width: Option<f32>) {
        if let Some(sidebar) = self.sidebar.as_mut() {
            sidebar.set_width(width);
        }
    }

    /// 侧边栏显示的是不是线程列表。
    pub fn is_threads_list_view_active(&self) -> bool {
        self.sidebar
            .as_ref()
            .is_some_and(Sidebar::is_threads_list_view_active)
    }

    /// 排出 `[侧边栏?, 工作区, 侧边栏?]` 这一行。
    ///
    /// `main` 由宿主排好递进来；侧边栏由本函数自己渲染（`Sidebar::view`），再用
    /// `map` 把它的消息并进宿主的消息类型。侧边栏没挂或没展开时原样返回 `main`。
    ///
    /// 手柄的**拖拽过程**不在这里处理：iced 的手柄控件只在指针位于 6px 命中区内
    /// 时才收得到事件，拖快了就跟丢。所以这里只在按下时报一条消息，由宿主用全局
    /// 指针事件跟着走（与 app 里拖滚动条是同一套做法）。
    pub fn layout<'a, Message: Clone + 'a>(
        &'a self,
        theme: &'a AppTheme,
        main: Element<'a, Message>,
        on_resize_start: Message,
        on_reset_width: Message,
        map: impl Fn(S::Message) -> Message + 'a,
    ) -> Element<'a, Message> {
        let Some(sidebar) = self.sidebar.as_ref().filter(|_| self.sidebar_open()) else {
            return main;
        };

        let handle = mouse_area(
            Space::new()
                .width(Length::Fixed(SIDEBAR_RESIZE_HANDLE_SIZE))
                .height(Fill),
        )
        .interaction(iced::mouse::Interaction::ResizingHorizontally)
        .on_press(on_resize_start)
        // 双击回到默认宽度 —— 与 Zed 的 `on_double_click(.. set_width(None) ..)` 一致。
        .on_double_click(on_reset_width);

        let sidebar = sidebar.view(theme).map(map);

        if self.sidebar_side().is_right() {
            row![main, handle, sidebar].into()
        } else {
            row![sidebar, handle, main].into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::text;

    /// 布局要透给侧边栏的主题；深浅两套的取值由 aa_theme_iced 自己测。
    static THEME: std::sync::LazyLock<AppTheme> = std::sync::LazyLock::new(AppTheme::dark);

    /// 测试替身。Zed 的同名替身是 `agent_ui::test_support::TestWorkspaceSidebar`；
    /// 我们的 `Sidebar` 只需实现 `view` 与宽度两项。
    struct TestSidebar {
        width: f32,
        default_width: f32,
        side: SidebarSide,
    }

    impl TestSidebar {
        fn new(side: SidebarSide) -> TestSidebar {
            TestSidebar {
                width: 300.,
                default_width: 300.,
                side,
            }
        }
    }

    impl Sidebar for TestSidebar {
        type Message = ();

        fn view(&self, _theme: &AppTheme) -> Element<'_, ()> {
            text("侧边栏").into()
        }

        fn width(&self) -> f32 {
            self.width
        }

        fn set_width(&mut self, width: Option<f32>) {
            let requested = width.unwrap_or(self.default_width);
            self.width = requested.clamp(THREADS_LIST_MIN_WIDTH, THREADS_LIST_MAX_WIDTH);
        }

        fn side(&self) -> SidebarSide {
            self.side
        }
    }

    fn workspace(side: SidebarSide) -> MultiWorkspace<TestSidebar> {
        let mut multi_workspace = MultiWorkspace::new();
        multi_workspace.register_sidebar(TestSidebar::new(side));
        multi_workspace
    }

    #[test]
    fn a_registered_sidebar_starts_closed() {
        let multi_workspace = workspace(SidebarSide::Left);
        assert!(!multi_workspace.sidebar_open(), "注册不等于展开");
        assert_eq!(
            multi_workspace.sidebar_render_state(),
            SidebarRenderState {
                open: false,
                side: SidebarSide::Left
            }
        );
    }

    #[test]
    fn toggling_flips_the_open_state() {
        let mut multi_workspace = workspace(SidebarSide::Left);
        multi_workspace.toggle_sidebar();
        assert!(multi_workspace.sidebar_open());
        multi_workspace.toggle_sidebar();
        assert!(!multi_workspace.sidebar_open());
    }

    #[test]
    fn opening_without_a_sidebar_does_nothing() {
        let mut multi_workspace: MultiWorkspace<TestSidebar> = MultiWorkspace::new();
        multi_workspace.open_sidebar();
        multi_workspace.toggle_sidebar();
        assert!(!multi_workspace.sidebar_open(), "没挂侧边栏时开关是空操作");
        assert_eq!(multi_workspace.sidebar_width(), 0.);
        assert!(!multi_workspace.is_threads_list_view_active());
    }

    #[test]
    fn dragging_the_width_out_of_range_is_clamped_by_the_sidebar() {
        let mut multi_workspace = workspace(SidebarSide::Left);
        multi_workspace.set_sidebar_width(Some(10.));
        assert_eq!(multi_workspace.sidebar_width(), THREADS_LIST_MIN_WIDTH);

        multi_workspace.set_sidebar_width(Some(10_000.));
        assert_eq!(multi_workspace.sidebar_width(), THREADS_LIST_MAX_WIDTH);

        multi_workspace.set_sidebar_width(None);
        assert_eq!(multi_workspace.sidebar_width(), 300., "None 回到默认宽度");
    }

    #[test]
    fn the_host_can_drive_the_sidebar_through_sidebar_mut() {
        // 宿主需要在 trait 之外驱动侧边栏自己的方法（新建线程之类），所以
        // `MultiWorkspace` 存的是实体、不是 `dyn Sidebar`。
        let mut multi_workspace = workspace(SidebarSide::Left);
        multi_workspace.sidebar_mut().unwrap().width = 321.;
        assert_eq!(multi_workspace.sidebar_width(), 321.);
    }

    #[test]
    fn the_side_comes_from_the_sidebar() {
        let multi_workspace = workspace(SidebarSide::Right);
        assert!(multi_workspace.sidebar_side().is_right());
    }

    #[test]
    fn the_two_sides_are_opposites() {
        assert_eq!(SidebarSide::Left.opposite(), SidebarSide::Right);
        assert_eq!(SidebarSide::Right.opposite(), SidebarSide::Left);
    }

    #[test]
    fn a_closed_sidebar_leaves_only_the_main_view() {
        let multi_workspace = workspace(SidebarSide::Left);
        let main: Element<'_, ()> = text("主区").into();
        let mut ui = iced_test::simulator(multi_workspace.layout(&THEME, main, (), (), |_| ()));
        assert!(ui.find("主区").is_ok());
        assert!(ui.find("侧边栏").is_err(), "关着的时候不该渲染侧边栏");
    }

    #[test]
    fn an_open_sidebar_shows_up_next_to_the_main_view() {
        let mut multi_workspace = workspace(SidebarSide::Left);
        multi_workspace.open_sidebar();
        let main: Element<'_, ()> = text("主区").into();
        let mut ui = iced_test::simulator(multi_workspace.layout(&THEME, main, (), (), |_| ()));
        assert!(ui.find("主区").is_ok());
        assert!(ui.find("侧边栏").is_ok(), "开着的时候应渲染侧边栏");
    }
}
