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
//! **未搬**（等有消费方）：项目分组头（`ListEntry::ProjectHeader`）、终端行
//! （`ListEntry::Terminal`）、归档视图（`ThreadsArchiveView`）、线程切换器
//! （`thread_switcher.rs`）、重命名内联编辑器、右键菜单、工作树标签、通知点、
//! 过滤框、键盘导航（`select_next`/`select_previous`/`confirm`）、虚拟化列表
//! （上游用 gpui 的 `list()`，我们先用普通 `scrollable`）。
//!
//! **消息**：上游没有消息枚举（动作由 gpui 的 action 系统派发），这里是 iced 的
//! 惯例 —— 控件往上抛 [`SidebarMessage`]，宿主映射成自己的消息类型。
#![warn(missing_docs)]

use std::rc::Rc;

use aa_ui_iced::{AgentThreadStatus, ThreadItem};
use agent_ui::{ThreadId, ThreadMetadata, ThreadMetadataStore};
use chrono::{DateTime, Utc};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Fill, Theme};
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

    fn view(&self) -> Element<'_, SidebarMessage> {
        view(self, |message| message)
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
}

/// 渲染侧边栏。
///
/// `on_message` 把侧边栏的消息映射成宿主的消息类型（与查找栏同一套做法）。
pub fn view<'a, Message: Clone + 'a>(
    state: &'a Sidebar,
    on_message: impl Fn(SidebarMessage) -> Message + 'a,
) -> Element<'a, Message> {
    // 每个按钮/每行都要一份闭包，而 `Fn` 不一定 `Clone` —— 共享同一份。
    let on_message: Rc<dyn Fn(SidebarMessage) -> Message + 'a> = Rc::new(on_message);

    let new_thread = button(text("＋").size(14.))
        .padding([2, 8])
        .style(|theme: &Theme, _status| button::Style {
            background: None,
            text_color: theme.extended_palette().background.base.text,
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
        row![text("线程").size(12.), new_thread]
            .spacing(8)
            .align_y(iced::Alignment::Center),
    )
    .padding([8, 10])
    .width(Fill);

    let rows: Vec<Element<'a, Message>> = state
        .contents
        .entries
        .iter()
        .map(|entry| match entry {
            ListEntry::Thread(thread) => render_thread(state, thread, &on_message),
        })
        .collect();

    let body: Element<'a, Message> = if rows.is_empty() {
        // 上游这里用 `ui::ProjectEmptyState`（带插画与引导按钮）；等那个组件搬
        // 进来再换，先给一句说明。
        container(text("还没有线程").size(12.))
            .padding(12)
            .width(Fill)
            .into()
    } else {
        scrollable(column(rows).spacing(1).padding([0, 6]))
            .height(Fill)
            .into()
    };

    container(column![header, body].spacing(0))
        .width(iced::Length::Fixed(state.width))
        .height(Fill)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.extended_palette().background.weak.color.into()),
            border: iced::Border {
                color: theme.extended_palette().background.strong.color,
                width: 0.,
                ..iced::Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// 渲染一行线程。
///
/// 上游同名函数有三百多行（图标、工作树标签、diff 统计、重命名编辑器、悬停按钮、
/// 右键菜单）；这里只留「图标 + 标题 + 时间 + 选中态 + 点击」这条主干。
fn render_thread<'a, Message: Clone + 'a>(
    state: &'a Sidebar,
    thread: &'a ThreadEntry,
    on_message: &Rc<dyn Fn(SidebarMessage) -> Message + 'a>,
) -> Element<'a, Message> {
    let is_active = state
        .active_entry
        .as_ref()
        .is_some_and(|active| active.matches_entry(&ListEntry::Thread(thread.clone())));

    let thread_id = thread.metadata.thread_id.clone();
    let on_message = Rc::clone(on_message);

    ThreadItem::new("✦")
        .title(thread.metadata.display_title())
        .timestamp(format_history_entry_timestamp(
            Sidebar::thread_display_time(&thread.metadata),
            Utc::now(),
        ))
        .status(thread.status)
        .selected(is_active)
        .on_click(move || on_message(SidebarMessage::ActivateEntry(thread_id.clone())))
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
        let mut ui = iced_test::simulator(view(&sidebar, |_| ()));
        assert!(ui.find("还没有线程").is_ok(), "空列表要给一句说明");
    }

    #[test]
    fn the_rows_render_and_clicking_one_reports_it() {
        let store = store();
        let sidebar = sidebar_on(&store);
        let mut ui = iced_test::simulator(view(&sidebar, |message| message));

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
        let mut ui = iced_test::simulator(view(&sidebar, |message| message));

        assert!(ui.click("＋").is_ok(), "应有新建按钮");
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            vec![SidebarMessage::CreateNewThread]
        );
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
