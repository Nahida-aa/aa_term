//! 线程侧边栏 —— 对应 Zed 的 `crates/sidebar`（AAAgent 里是
//! `packages/sidebar/src/lib.rs`）。
//!
//! 上游那个 crate 有八千四百行，且每件事都摊在 gpui 的 `Entity`/`Window` 上：项目
//! 分组、工作树标签、通知、归档视图、线程切换器、重命名内联编辑器……本文件只搬
//! **一条能跑起来的主干**：
//!
//! ```text
//! ThreadMetadataStore（agent_ui）
//!         │  rebuild_contents()
//!         ▼
//! SidebarContents { entries: Vec<ListEntry> }   ← 列表数据（这一层可单独测）
//!         │  view()
//!         ▼
//! aa_ui_iced::ThreadItem 逐行渲染 + 「新线程」按钮
//! ```
//!
//! 与上游一致的两条划分：
//!
//! - **列表数据不持有线程本身**，只持有轻量元数据（`ThreadMetadata`）；Zed 的
//!   注释也是这么写的：*enough to populate the sidebar list and route to the
//!   correct load path when clicked*。
//! - **「当前选中哪条」在侧边栏上**（`active_entry`），不在终端/会话那边。
//!
//! **取色**全部走 zed 语义令牌（`aa_theme_iced`）：侧边栏底色 = `surface_background`、
//! 与主区之间那道 1px 分隔 = `border`、标题 = `text_muted`、行内令牌见 `ThreadItem`。
//! 主题由宿主在渲染时传进来（gpui 那边是从 `cx.theme()` 环境里取的）。
//!
//! **未搬**（等有消费方）：项目分组头（`ListEntry::ProjectHeader`）、终端行
//! （`ListEntry::Terminal`）、归档视图（`ThreadsArchiveView`）、线程切换器
//! （`thread_switcher.rs`）、工作树标签、通知点、过滤框、键盘导航
//! （`select_next`/`select_previous`/`confirm`）、虚拟化列表（上游用 gpui 的
//! `list()`，我们先用普通 `scrollable`）、菜单里的「Regenerate Thread Title」与
//! 「Open Thread as Markdown」（要 agent 后端与 markdown 面板）。
//!
//! **消息**：上游没有消息枚举（动作由 gpui 的 action 系统派发），这里是 iced 的
//! 惯例 —— 控件往上抛 [`SidebarMessage`]，宿主映射成自己的消息类型。
#![warn(missing_docs)]

use std::rc::Rc;

use aa_theme_iced::Theme as AppTheme;
use aa_ui_iced::{AgentThreadStatus, ThreadItem};
use agent_ui::{ThreadId, ThreadMetadata, ThreadMetadataStore};
use chrono::{DateTime, Utc};
use iced::widget::{button, column, container, row, rule, scrollable, text, text_input};
use iced::{Alignment, Element, Fill, Length};
use workspace::{SidebarSide, THREADS_LIST_MAX_WIDTH, THREADS_LIST_MIN_WIDTH};

/// 列表里的一行。
///
/// 上游还有 `ProjectHeader`（项目分组头）与 `Terminal`（终端线程行）两个变体；
/// 我们还没有项目与终端线程的概念，所以现在只有线程一种。保留 `enum` 的形状是
/// 为了让那两个变体加进来时不用改调用方。
#[derive(Debug, Clone, PartialEq)]
pub enum ListEntry {
    /// 一条线程。
    Thread(ThreadEntry),
}

/// 线程行的数据。
///
/// 上游是 `ThreadEntry { metadata, icon, status, workspace, is_live, worktrees,
/// diff_stats, .. }`；这里只留渲染一行真正要用的两样。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadEntry {
    /// 元数据（标题、时间、草稿与否……）。
    pub metadata: ThreadMetadata,
    /// 实时状态（跑着/等确认/出错）。
    pub status: AgentThreadStatus,
}

/// 列表内容。
///
/// 上游是 `SidebarContents { entries, notified_threads, notified_terminals,
/// project_header_indices, has_open_projects }` —— 后几项都是通知/分组用的，
/// 等它们搬进来时再加。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SidebarContents {
    /// 已按显示时间倒序排好的行。
    pub entries: Vec<ListEntry>,
}

/// 当前打开的那一条。
///
/// 上游是 `ActiveEntry::{ Thread { thread_id, session_id, workspace },
/// Terminal { .. } }`；我们只有线程，且 workspace/终端行还没有，所以先只带 id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveEntry {
    /// 打开的是一条线程。
    Thread {
        /// 它的 id。
        thread_id: ThreadId,
    },
}

impl ActiveEntry {
    /// 这个 active 状态是不是就是这一行。
    ///
    /// 上游 `ActiveEntry::matches_entry` 还要比对 `session_id`（线程被重开时
    /// 换 id 不换 session）；我们还没有 session 重开这条路，先只比 id。
    pub fn matches_entry(&self, entry: &ListEntry) -> bool {
        match (self, entry) {
            (ActiveEntry::Thread { thread_id }, ListEntry::Thread(thread)) => {
                thread_id == &thread.metadata.thread_id
            }
        }
    }
}

/// 侧边栏自己持有的状态。
///
/// 宽度由它持有（[`workspace::MultiWorkspace`] 只是转手调用），与上游一致 ——
/// 宽度上下限是侧边栏的知识。
pub struct Sidebar {
    width: f32,
    default_width: f32,
    side: SidebarSide,
    contents: SidebarContents,
    active_entry: Option<ActiveEntry>,
    /// 正在重命名的那一行（`None` = 没在重命名）。
    ///
    /// 上游把这件事放在 gpui 的 `TextInput` 实体 + `rename_target` 里
    /// （`start_renaming_entry`），我们这里就是一个字符串 + 谁的 id。
    renaming: Option<RenamingEntry>,
}

/// 行内重命名的状态。
#[derive(Debug, Clone, PartialEq)]
struct RenamingEntry {
    thread_id: ThreadId,
    /// 输入框里的当前文本（宿主每敲一个字符回灌一次）。
    text: String,
}

impl Sidebar {
    /// 给定停靠侧建一个空侧边栏。
    pub fn new(side: SidebarSide) -> Sidebar {
        let default_width = 300.;
        Sidebar {
            width: default_width,
            default_width,
            side,
            contents: SidebarContents::default(),
            active_entry: None,
            renaming: None,
        }
    }

    /// 从元数据存储重建列表。
    ///
    /// 归档的线程不进主列表（上游由 `entries_for_main_worktree_path` 之类顺带
    /// 过滤掉，判据同为 `archived`）；顺序按**显示时间倒序**，也就是最近动过的
    /// 在最上面 —— 上游 `push_entries_by_display_time` 里那个 `.reverse()`。
    pub fn rebuild_contents(&mut self, store: &ThreadMetadataStore) {
        let mut threads: Vec<ThreadEntry> = store
            .entries()
            .filter(|metadata| !metadata.archived)
            .map(|metadata| ThreadEntry {
                metadata: metadata.clone(),
                // 我们有实时状态的只有 agent 后端；还没有它，一律按「跑完了」。
                status: AgentThreadStatus::Completed,
            })
            .collect();
        threads.sort_by(|left, right| {
            Self::thread_display_time(&right.metadata)
                .cmp(&Self::thread_display_time(&left.metadata))
        });

        self.contents.entries = threads.into_iter().map(ListEntry::Thread).collect();

        // 选中的那条要是被归档/删了，就别再留着它。
        if let Some(active) = &self.active_entry
            && !self
                .contents
                .entries
                .iter()
                .any(|entry| active.matches_entry(entry))
        {
            self.active_entry = None;
        }
    }

    /// 列表内容。
    pub fn contents(&self) -> &SidebarContents {
        &self.contents
    }

    /// 当前打开的那一条。
    pub fn active_entry(&self) -> Option<&ActiveEntry> {
        self.active_entry.as_ref()
    }

    /// 把某条线程设为当前打开的。
    ///
    /// 上游的 `activate_entry` 还要通知 agent 面板切过去、处理终端与线程的切换；
    /// 我们没有面板，只剩「记下来」这一步。
    pub fn activate_entry(&mut self, thread_id: &ThreadId) {
        self.active_entry = Some(ActiveEntry::Thread {
            thread_id: thread_id.clone(),
        });
    }

    /// 开始重命名某条线程（上游：`start_renaming_entry`）。
    ///
    /// 初值是当前标题 —— 与上游一致（那边把 `rename_title` 传进编辑器）。
    pub fn start_renaming(&mut self, thread_id: &ThreadId) {
        let Some(entry) = self.entry(thread_id) else {
            return;
        };
        let text = entry.metadata.display_title();
        self.renaming = Some(RenamingEntry {
            thread_id: thread_id.clone(),
            text,
        });
    }

    /// 结束重命名（确认或取消都一样，文本的去留由宿主决定）。
    pub fn stop_renaming(&mut self) {
        self.renaming = None;
    }

    /// 正在重命名的哪条线程。
    pub fn renaming_thread_id(&self) -> Option<&ThreadId> {
        self.renaming.as_ref().map(|entry| &entry.thread_id)
    }

    /// 重命名输入框里的当前文本。
    pub fn renaming_text(&self) -> Option<&str> {
        self.renaming.as_ref().map(|entry| entry.text.as_str())
    }

    /// 输入框内容变了（宿主把输入事件回灌进来）。
    pub fn set_renaming_text(&mut self, text: String) {
        if let Some(renaming) = self.renaming.as_mut() {
            renaming.text = text;
        }
    }

    /// 列表里的某条线程。
    fn entry(&self, thread_id: &ThreadId) -> Option<&ThreadEntry> {
        self.contents.entries.iter().find_map(|entry| match entry {
            ListEntry::Thread(thread) if &thread.metadata.thread_id == thread_id => Some(thread),
            _ => None,
        })
    }

    /// 开一条新线程。
    ///
    /// 上游同名方法是异步的（要起 ACP 会话、建条目、写 store），我们这里只做
    /// 数据侧：写一条草稿元数据、重建列表、选中它。会话本身等 agent 后端。
    pub fn create_new_thread(
        &mut self,
        store: &mut ThreadMetadataStore,
        now: DateTime<Utc>,
    ) -> ThreadId {
        let thread_id = ThreadId::fresh();
        store.save(ThreadMetadata::new(thread_id.clone(), now));
        self.rebuild_contents(store);
        self.activate_entry(&thread_id);
        thread_id
    }

    /// 一行按哪个时间排序。
    ///
    /// 与上游 `Sidebar::thread_display_time` 同判据：用户最后一次交互优先，否则
    /// 用最后更新时间。
    pub fn thread_display_time(metadata: &ThreadMetadata) -> DateTime<Utc> {
        metadata.interacted_at.unwrap_or(metadata.updated_at)
    }
}

impl workspace::Sidebar for Sidebar {
    type Message = SidebarMessage;

    fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, SidebarMessage> {
        view(self, theme, |message| message)
    }

    fn width(&self) -> f32 {
        self.width
    }

    /// 夹取范围与上游 `agent_settings::THREADS_LIST_MIN/MAX_WIDTH` 相同：
    /// 既约束拖拽结果，也约束配置里的默认宽度。
    fn set_width(&mut self, width: Option<f32>) {
        let requested = width.unwrap_or(self.default_width);
        self.width = requested.clamp(THREADS_LIST_MIN_WIDTH, THREADS_LIST_MAX_WIDTH);
    }

    fn side(&self) -> SidebarSide {
        self.side
    }
}

/// 侧边栏抛给宿主的消息。
///
/// 变体名取自上游的同名方法（`create_new_thread` / `activate_entry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarMessage {
    /// 新建一条线程。
    CreateNewThread,
    /// 打开某条线程。
    ActivateEntry(ThreadId),
    /// 右键了某条线程：请宿主在鼠标位置弹出上下文菜单。
    ///
    /// 菜单本体不在这里 ── 条目里的动作（改名、归档……）要动宿主持有的 store 与
    /// 别的面板，**菜单内容归宿主**；侧边栏只负责报「哪一行被右键了」。位置也不用
    /// 传：鼠标位置宿主自己有（它要自己跟踪指针）。
    ShowContextMenu(ThreadId),
    /// 重命名输入框的内容变了。
    RenameInput(String),
    /// 重命名确认（回车）。
    RenameCommit,
    /// 重命名取消（Esc）。
    RenameCancel,
}

/// 渲染侧边栏。
///
/// `theme` 是取色用的主题，`on_message` 把侧边栏的消息映射成宿主的消息类型
/// （与查找栏同一套做法）。
pub fn view<'a, Message: Clone + 'a>(
    state: &'a Sidebar,
    theme: &'a AppTheme,
    on_message: impl Fn(SidebarMessage) -> Message + 'a,
) -> Element<'a, Message> {
    // 每个按钮/每行都要一份闭包，而 `Fn` 不一定 `Clone` —— 共享同一份。
    let on_message: Rc<dyn Fn(SidebarMessage) -> Message + 'a> = Rc::new(on_message);
    let colors = theme.colors();

    let new_thread = button(text("＋").size(14.).color(colors.text_muted))
        .padding([2, 8])
        .style(move |_, status| button::Style {
            // 悬停才浮出底 —— 与 zed 的 `IconButton` ghost 样式同一条路子。
            background: matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then(|| colors.ghost_element_hover.into()),
            text_color: colors.text,
            border: iced::Border {
                radius: 4.0.into(),
                ..iced::Border::default()
            },
            ..button::Style::default()
        })
        .on_press_with({
            let on_message = Rc::clone(&on_message);
            move || on_message(SidebarMessage::CreateNewThread)
        });

    let header = container(
        row![text("线程").size(12.).color(colors.text_muted), new_thread]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding([8, 10])
    .width(Fill);

    let rows: Vec<Element<'a, Message>> = state
        .contents
        .entries
        .iter()
        .map(|entry| match entry {
            ListEntry::Thread(thread) => render_thread(state, theme, thread, &on_message),
        })
        .collect();

    let body: Element<'a, Message> = if rows.is_empty() {
        // 上游这里用 `ui::ProjectEmptyState`（带插画与引导按钮）；等那个组件搬
        // 进来再换，先给一句说明。
        container(text("还没有线程").size(12.).color(colors.text_muted))
            .padding(12)
            .width(Fill)
            .into()
    } else {
        scrollable(column(rows).spacing(1).padding([0, 6]))
            .height(Fill)
            .into()
    };

    let content = container(column![header, body].spacing(0))
        // 1px 留给分隔线，总宽仍是 `state.width` —— 宿主的「侧边栏占多宽」是按这个
        // 宽度算的，分隔线必须算在里面，否则主区会少 1px。
        .width(Length::Fixed(state.width - SEPARATOR_WIDTH))
        .height(Fill)
        .style(move |_| container::Style {
            background: Some(colors.surface_background.into()),
            ..container::Style::default()
        });

    // 与主区之间的分隔线（上游是 `border_r_1` / `border_l_1` + `border_color(border)`）。
    // iced 的 `Border` 四面同宽，画不出「只画靠主区那一边」，所以用一条 1px 的
    // `rule` 当兄弟节点 —— 效果一样，宽度还更好算。
    let separator = rule::vertical(SEPARATOR_WIDTH).style(move |_| rule::Style {
        color: colors.border,
        radius: 0.0.into(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    });

    let sidebar: Element<'a, Message> = match state.side {
        SidebarSide::Left => row![content, separator].into(),
        SidebarSide::Right => row![separator, content].into(),
    };

    sidebar
}

/// 与主区之间那道分隔线的宽度（像素）。
const SEPARATOR_WIDTH: f32 = 1.0;

/// 重命名输入框的 id（上游的编辑器也有 id，用于聚焦）。
const RENAME_EDITOR_ID: &str = "sidebar-rename-editor";

/// 渲染一行线程。
///
/// 上游同名函数有三百多行（图标、工作树标签、diff 统计、重命名编辑器、悬停按钮、
/// 右键菜单）；这里留「图标 + 标题 + 时间 + 选中态 + 点击 + 右键 + 重命名输入框」。
fn render_thread<'a, Message: Clone + 'a>(
    state: &'a Sidebar,
    theme: &'a AppTheme,
    thread: &'a ThreadEntry,
    on_message: &Rc<dyn Fn(SidebarMessage) -> Message + 'a>,
) -> Element<'a, Message> {
    let is_active = state
        .active_entry
        .as_ref()
        .is_some_and(|active| active.matches_entry(&ListEntry::Thread(thread.clone())));

    let thread_id = thread.metadata.thread_id.clone();

    // 正在重命名的这一行：标题位置换成输入框（上游 `render_rename_title_editor`）。
    if state.renaming_thread_id() == Some(&thread_id) {
        let on_message = Rc::clone(on_message);
        let input = text_input("线程名", state.renaming_text().unwrap_or_default())
            // 给个 id：宿主与测试都能按 id 选中这个输入框。
            .id(RENAME_EDITOR_ID)
            .on_input({
                let on_message = Rc::clone(&on_message);
                move |text| on_message(SidebarMessage::RenameInput(text))
            })
            .on_submit(on_message(SidebarMessage::RenameCommit))
            .size(13.)
            .padding([4, 6]);
        return container(input).padding([2, 6]).width(Fill).into();
    }

    // 两个闭包各要一份 `Rc` 与一份 id。
    let on_click_message = Rc::clone(on_message);
    let on_right_press_message = Rc::clone(on_message);
    let click_id = thread_id.clone();
    let context_menu_id = thread_id;
    ThreadItem::new(theme, "✦")
        .title(thread.metadata.display_title())
        .timestamp(format_history_entry_timestamp(
            Sidebar::thread_display_time(&thread.metadata),
            Utc::now(),
        ))
        .status(thread.status)
        .selected(is_active)
        .on_click(move || on_click_message(SidebarMessage::ActivateEntry(click_id.clone())))
        .on_right_press(move || {
            on_right_press_message(SidebarMessage::ShowContextMenu(context_menu_id.clone()))
        })
        .into()
}

/// 把时间压成 `5m` / `3h` / `2d` 这样的短标签。
///
/// 与 Zed 的 `agent_ui::threads_archive_view::format_history_entry_timestamp` 同一
/// 口径（分钟/小时/天/周/月，各段不足 1 也显示 1）。两处不同：**位置**（上游在
/// agent_ui 的归档视图里，等归档视图搬过来再挪过去）与**入参**（上游自己在里面读
/// `Utc::now()`，那样没法测；这里把 `now` 显式传进来）。
pub fn format_history_entry_timestamp(entry_time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let duration = now.signed_duration_since(entry_time);

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();
    let weeks = days / 7;
    let months = days / 30;

    if minutes < 60 {
        format!("{}m", minutes.max(1))
    } else if hours < 24 {
        format!("{}h", hours.max(1))
    } else if days < 7 {
        format!("{}d", days.max(1))
    } else if weeks < 4 {
        format!("{}w", weeks.max(1))
    } else {
        format!("{}mo", months.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ui::DEFAULT_THREAD_TITLE;
    use workspace::Sidebar as _;

    /// 渲染测试用的主题；令牌取值本身由 aa_theme_iced 自己测。
    static THEME: std::sync::LazyLock<AppTheme> = std::sync::LazyLock::new(AppTheme::dark);

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("合法时间戳")
    }

    /// 造一个存了三线程（其中一条已归档）的 store。
    ///
    /// 时间刻意取在很久以前，这样 `view` 里的 `Utc::now()` 不会让相对时间抖动。
    fn store() -> ThreadMetadataStore {
        let mut store = ThreadMetadataStore::new();
        for (id, interacted) in [("old", 10), ("new", 200), ("archived", 300)] {
            let mut metadata = ThreadMetadata::new(ThreadId::new(id), time(interacted));
            metadata.title = Some(format!("标题-{id}"));
            metadata.interacted_at = Some(time(interacted));
            metadata.archived = id == "archived";
            store.save(metadata);
        }
        store
    }

    fn sidebar_on(store: &ThreadMetadataStore) -> Sidebar {
        let mut sidebar = Sidebar::new(SidebarSide::Left);
        sidebar.rebuild_contents(store);
        sidebar
    }

    fn titles(sidebar: &Sidebar) -> Vec<String> {
        sidebar
            .contents()
            .entries
            .iter()
            .map(|entry| match entry {
                ListEntry::Thread(thread) => thread.metadata.display_title(),
            })
            .collect()
    }

    #[test]
    fn the_list_is_newest_first_and_skips_archived_threads() {
        let store = store();
        let sidebar = sidebar_on(&store);
        assert_eq!(
            titles(&sidebar),
            vec!["标题-new", "标题-old"],
            "最近的在最上面，归档的不出现"
        );
    }

    #[test]
    fn activating_an_entry_marks_that_row_active() {
        let store = store();
        let mut sidebar = sidebar_on(&store);
        assert!(sidebar.active_entry().is_none());

        sidebar.activate_entry(&ThreadId::new("old"));
        let active = sidebar.active_entry().expect("应记下当前线程");
        let entry = sidebar.contents().entries.last().expect("应有行").clone();
        assert!(active.matches_entry(&entry), "active 应指向被点的那一行");
        assert!(
            !active.matches_entry(&sidebar.contents().entries[0]),
            "不该把别的行也算成当前"
        );
    }

    #[test]
    fn creating_a_thread_adds_a_draft_at_the_top_and_activates_it() {
        let mut store = store();
        let mut sidebar = sidebar_on(&store);

        let thread_id = sidebar.create_new_thread(&mut store, time(400));

        assert_eq!(sidebar.contents().entries.len(), 3, "草稿应进列表");
        assert_eq!(
            titles(&sidebar)[0],
            DEFAULT_THREAD_TITLE,
            "草稿用占位标题，而且排在最上面"
        );
        assert!(store.entry(&thread_id).expect("应写进 store").is_draft());
        assert_eq!(
            sidebar.active_entry(),
            Some(&ActiveEntry::Thread { thread_id })
        );
    }

    #[test]
    fn archiving_the_active_thread_clears_the_selection() {
        let mut store = store();
        let mut sidebar = sidebar_on(&store);
        sidebar.activate_entry(&ThreadId::new("old"));

        store.archive(&ThreadId::new("old"));
        sidebar.rebuild_contents(&store);

        assert!(sidebar.active_entry().is_none(), "被归档的那条不该还是当前");
    }

    #[test]
    fn the_width_is_clamped_and_reset_goes_back_to_the_default() {
        let mut sidebar = Sidebar::new(SidebarSide::Left);
        sidebar.set_width(Some(10.));
        assert_eq!(sidebar.width(), THREADS_LIST_MIN_WIDTH);
        sidebar.set_width(Some(10_000.));
        assert_eq!(sidebar.width(), THREADS_LIST_MAX_WIDTH);
        sidebar.set_width(None);
        assert_eq!(sidebar.width(), 300.);
    }

    #[test]
    fn the_sidebar_says_it_shows_the_thread_list() {
        let sidebar = Sidebar::new(SidebarSide::Right);
        assert!(workspace::Sidebar::is_threads_list_view_active(&sidebar));
        assert_eq!(workspace::Sidebar::side(&sidebar), SidebarSide::Right);
    }

    #[test]
    fn an_empty_sidebar_says_so() {
        let sidebar = Sidebar::new(SidebarSide::Left);
        let mut ui = iced_test::simulator(view(&sidebar, &THEME, |_| ()));
        assert!(ui.find("还没有线程").is_ok(), "空列表要给一句说明");
    }

    #[test]
    fn the_rows_render_and_clicking_one_reports_it() {
        let store = store();
        let sidebar = sidebar_on(&store);
        let mut ui = iced_test::simulator(view(&sidebar, &THEME, |message| message));

        assert!(ui.find("标题-new").is_ok(), "应渲染标题");
        assert!(ui.find("标题-old").is_ok());

        assert!(ui.click("标题-old").is_ok(), "行应可点");
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            vec![SidebarMessage::ActivateEntry(ThreadId::new("old"))]
        );
    }

    #[test]
    fn the_new_thread_button_reports_it() {
        let store = store();
        let sidebar = sidebar_on(&store);
        let mut ui = iced_test::simulator(view(&sidebar, &THEME, |message| message));

        assert!(ui.click("＋").is_ok(), "应有新建按钮");
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            vec![SidebarMessage::CreateNewThread]
        );
    }

    /// 右键要报「哪一行」，菜单内容由宿主决定（条目里的动作要动 store）。
    #[test]
    fn right_clicking_a_row_asks_the_host_for_a_context_menu() {
        let store = store();
        let sidebar = sidebar_on(&store);
        // 夹具里显示时间最新的那条（"new"）排在最上面。
        let title = match &sidebar.contents().entries[0] {
            ListEntry::Thread(thread) => thread.metadata.display_title(),
        };
        let mut ui = iced_test::simulator(view(&sidebar, &THEME, |message| message));

        let target = ui.find(title.as_str()).expect("找得到那一行的标题");
        let bounds = target.visible_bounds().expect("标题可见");
        ui.point_at(bounds.center());
        ui.simulate([
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Right,
            )),
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Right,
            )),
        ]);

        let messages: Vec<SidebarMessage> = ui.into_messages().collect();
        assert_eq!(messages.len(), 1, "右键应恰好产出一条消息：{messages:?}");
        assert!(
            matches!(messages[0], SidebarMessage::ShowContextMenu(_)),
            "应是「请宿主弹菜单」：{:?}",
            messages[0]
        );
    }

    /// 重命名：开始之后那一行的标题位置换成输入框，且初值是当前标题
    /// （上游 `start_renaming_entry` 也是把现标题填进去）。
    #[test]
    fn renaming_a_row_shows_an_input_prefilled_with_its_title() {
        let store = store();
        let mut sidebar = sidebar_on(&store);
        let (thread_id, title) = match &sidebar.contents().entries[0] {
            ListEntry::Thread(thread) => (
                thread.metadata.thread_id.clone(),
                thread.metadata.display_title(),
            ),
        };

        sidebar.start_renaming(&thread_id);
        assert_eq!(sidebar.renaming_thread_id(), Some(&thread_id));
        assert_eq!(sidebar.renaming_text(), Some(title.as_str()));

        let mut ui = iced_test::simulator(view(&sidebar, &THEME, |message| message));
        assert!(
            ui.find(iced_test::selector::id(RENAME_EDITOR_ID)).is_ok(),
            "重命名时标题位置应换成输入框"
        );
        // 输入框里就是当前标题（初值），所以按文本一样查得到 —— 查得到的是
        // 输入框的**内容**而不是普通 Label。
        assert!(ui.find(title.as_str()).is_ok(), "输入框里应带着当前标题");
        // 其他行照旧是普通行（夹具里还有 "old" 那条）。
        assert!(ui.find("标题-old").is_ok(), "其他行不受影响");
        // 普通行的文本仍然按普通文字找得到（只有那一行有输入框，id 是唯一的）。
        assert!(ui.find(iced_test::selector::id(RENAME_EDITOR_ID)).is_ok());
    }

    /// 输入与结束都走消息，宿主只负责把文本落到 store。
    #[test]
    fn the_rename_editor_reports_typing_and_finishing() {
        let store = store();
        let mut sidebar = sidebar_on(&store);
        let thread_id = sidebar.renaming_thread_id().cloned();
        assert!(thread_id.is_none(), "默认不在重命名");

        sidebar.set_renaming_text("改了名".to_string());
        assert_eq!(sidebar.renaming_text(), None, "没在重命名时改文本应无效果");

        let thread_id = match &sidebar.contents().entries[0] {
            ListEntry::Thread(thread) => thread.metadata.thread_id.clone(),
        };
        sidebar.start_renaming(&thread_id);
        sidebar.set_renaming_text("改了名".to_string());
        assert_eq!(sidebar.renaming_text(), Some("改了名"));

        sidebar.stop_renaming();
        assert_eq!(sidebar.renaming_thread_id(), None, "结束后不再重命名");
    }

    /// 给不存在的线程开重命名：什么也不做（上游同样先查 entry）。
    #[test]
    fn renaming_an_unknown_thread_does_nothing() {
        let store = store();
        let mut sidebar = sidebar_on(&store);
        sidebar.start_renaming(&ThreadId::fresh());
        assert_eq!(sidebar.renaming_thread_id(), None);
    }

    #[test]
    fn timestamps_are_shrunk_to_a_short_label() {
        let now = time(1_000_000);
        let ago = |seconds: i64| now - chrono::Duration::seconds(seconds);

        assert_eq!(
            format_history_entry_timestamp(now, now),
            "1m",
            "刚刚也显示 1m"
        );
        assert_eq!(format_history_entry_timestamp(ago(300), now), "5m");
        assert_eq!(format_history_entry_timestamp(ago(3 * 3_600), now), "3h");
        assert_eq!(format_history_entry_timestamp(ago(24 * 3_600), now), "1d");
        assert_eq!(format_history_entry_timestamp(ago(7 * 86_400), now), "1w");
        assert_eq!(
            format_history_entry_timestamp(ago(5 * 30 * 86_400), now),
            "5mo"
        );
    }
}
