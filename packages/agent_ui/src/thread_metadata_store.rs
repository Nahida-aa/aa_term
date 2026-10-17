//! 线程元数据的存储 —— 对应 Zed 的
//! `crates/agent_ui/src/thread_metadata_store.rs`。
//!
//! 侧边栏要的只是「有哪些线程、各自叫什么、什么时候动过、是不是还被选中」，所以
//! 这里存的是**轻量元数据**，不是会话本身（Zed 的注释也这么写：*enough to populate
//! the sidebar list and route to the correct load path when clicked*）。
//!
//! **与上游的差别**（都在类型注释里标了）：上游的 `ThreadId` 是 `uuid::Uuid` 包装、
//! `SessionId` 来自 `acp` crate、`AgentId` 来自 `project`；我们还没有这些东西，
//! 用字符串包装顶上，名字保持一致。worktree / remote / 归档时间桶等字段等有消费
//! 方再补。

use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, Utc};

/// 线程 id。
///
/// 上游是 `uuid::Uuid` 的包装（`thread_id` crate）；这里用字符串包装，对上层
/// 是不透明的 —— 侧边栏只拿它做比较和查表，不解析内容。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(String);

impl ThreadId {
    /// 用一个显式的字符串造 id（测试与恢复持久化数据时用）。
    pub fn new(id: impl Into<String>) -> ThreadId {
        ThreadId(id.into())
    }

    /// 造一个进程内唯一的 id。
    ///
    /// 上游的 id 是 UUID、天然唯一；我们省掉这个依赖，用一个自增计数器 ——
    /// 单一进程内的列表够用，等要做持久化/跨进程再换 UUID。
    pub fn fresh() -> ThreadId {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        ThreadId(format!("thread-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    /// 借用成 `&str`。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// ACP 会话 id。
///
/// 上游是 `acp::SessionId`。去掉 agent 后端之后它只剩一个作用：**区分草稿与真线程**
/// （见 [`ThreadMetadata::is_draft`]），所以先用字符串顶上，名字不变。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// 用显式字符串造一个。
    pub fn new(id: impl Into<String>) -> SessionId {
        SessionId(id.into())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 一条线程的轻量元数据。
///
/// 字段名与语义照搬 Zed（见模块头说明）。**未搬**：`agent_id`、
/// `worktree_paths`、`remote_connection`。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadMetadata {
    /// 线程 id。
    pub thread_id: ThreadId,
    /// 起了会话才有（草稿没有）。
    pub session_id: Option<SessionId>,
    /// agent 起的标题。
    pub title: Option<String>,
    /// 用户改的标题，优先于 `title`。
    ///
    /// 保留这个字段是为了「用户改过之后，agent 再自动更新标题也不覆盖」——
    /// 与 Zed 的 `title_override` 同一语义。
    pub title_override: Option<String>,
    /// 最近一次更新时间（agent 写的标题、状态等）。
    pub updated_at: DateTime<Utc>,
    /// 创建时间。
    pub created_at: Option<DateTime<Utc>>,
    /// 用户最后一次发消息的时间。
    pub interacted_at: Option<DateTime<Utc>>,
    /// 是否已归档（归档的线程不进主列表）。
    pub archived: bool,
}

impl ThreadMetadata {
    /// 新线程的元数据。
    ///
    /// 上游没有这个构造函数（那边由反序列化与面板各自拼字段）；加一个是为了让
    /// 「新建线程」这条路径只在一处拼默认值。
    pub fn new(thread_id: ThreadId, now: DateTime<Utc>) -> ThreadMetadata {
        ThreadMetadata {
            thread_id,
            session_id: None,
            title: None,
            title_override: None,
            updated_at: now,
            created_at: Some(now),
            interacted_at: None,
            archived: false,
        }
    }

    /// 还没发过第一条消息的就是草稿 —— 与 Zed 同判据（`session_id.is_none()`）。
    pub fn is_draft(&self) -> bool {
        self.session_id.is_none()
    }

    /// 列表上显示的标题；没标题时用占位标题。
    pub fn display_title(&self) -> String {
        self.title()
            .unwrap_or_else(|| crate::DEFAULT_THREAD_TITLE.to_string())
    }

    /// 生效的标题：用户改过的优先。
    pub fn title(&self) -> Option<String> {
        self.title_override.clone().or_else(|| self.title.clone())
    }
}

/// 线程元数据的存储。
///
/// 上游是个 gpui `Global`（任何地方 `ThreadMetadataStore::global(cx)` 都能拿到）；
/// 这里是个普通结构，由宿主持有。持久化（上游存 sqlite）也还没做。
#[derive(Debug, Default)]
pub struct ThreadMetadataStore {
    threads: HashMap<ThreadId, ThreadMetadata>,
}

impl ThreadMetadataStore {
    /// 空存储。
    pub fn new() -> ThreadMetadataStore {
        ThreadMetadataStore::default()
    }

    /// 某条线程的元数据。
    pub fn entry(&self, thread_id: &ThreadId) -> Option<&ThreadMetadata> {
        self.threads.get(thread_id)
    }

    /// 全部线程（顺序不定，排序由侧边栏负责）。
    pub fn entries(&self) -> impl Iterator<Item = &ThreadMetadata> + '_ {
        self.threads.values()
    }

    /// 已归档的线程。
    pub fn archived_entries(&self) -> impl Iterator<Item = &ThreadMetadata> + '_ {
        self.entries().filter(|thread| thread.archived)
    }

    /// 写入（新建或覆盖）。
    pub fn save(&mut self, metadata: ThreadMetadata) {
        self.threads.insert(metadata.thread_id.clone(), metadata);
    }

    /// 记一次「用户与这条线程交互了」。
    ///
    /// 未命中的 id 直接忽略 —— 上游同样只在存在时更新。
    pub fn update_interacted_at(&mut self, thread_id: &ThreadId, time: DateTime<Utc>) {
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.interacted_at = Some(time);
            thread.updated_at = time;
        }
    }

    /// 用户改标题：写进 `title_override`，压住 agent 自动起的标题。
    pub fn set_title_override(&mut self, thread_id: &ThreadId, title: String) {
        if let Some(thread) = self.threads.get_mut(thread_id) {
            if thread.title_override.as_ref() == Some(&title) {
                return;
            }
            thread.title_override = Some(title);
            thread.updated_at = Utc::now();
        }
    }

    /// 归档（主列表不再显示）。上游的删除分「归档」与真删两条路，这里只留归档。
    pub fn archive(&mut self, thread_id: &ThreadId) {
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.archived = true;
        }
    }

    /// 删除。
    pub fn delete(&mut self, thread_id: &ThreadId) {
        self.threads.remove(thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("合法时间戳")
    }

    fn store_with_two_threads() -> (ThreadMetadataStore, ThreadId, ThreadId) {
        let mut store = ThreadMetadataStore::new();
        let first = ThreadId::new("a");
        let second = ThreadId::new("b");
        store.save(ThreadMetadata::new(first.clone(), time(10)));
        store.save(ThreadMetadata::new(second.clone(), time(20)));
        (store, first, second)
    }

    #[test]
    fn a_new_thread_is_a_draft_until_it_gets_a_session() {
        let mut metadata = ThreadMetadata::new(ThreadId::fresh(), time(0));
        assert!(metadata.is_draft(), "还没起会话就是草稿");

        metadata.session_id = Some(SessionId::new("session-1"));
        assert!(!metadata.is_draft());
    }

    #[test]
    fn the_display_title_falls_back_to_the_placeholder() {
        let mut metadata = ThreadMetadata::new(ThreadId::fresh(), time(0));
        assert_eq!(metadata.display_title(), crate::DEFAULT_THREAD_TITLE);

        metadata.title = Some("agent 起的标题".into());
        assert_eq!(metadata.display_title(), "agent 起的标题");
    }

    #[test]
    fn a_user_title_override_beats_the_agent_title() {
        let mut metadata = ThreadMetadata::new(ThreadId::fresh(), time(0));
        metadata.title = Some("agent 起的".into());
        metadata.title_override = Some("用户改的".into());
        assert_eq!(metadata.display_title(), "用户改的");
    }

    #[test]
    fn a_fresh_id_does_not_repeat() {
        assert_ne!(ThreadId::fresh(), ThreadId::fresh());
    }

    #[test]
    fn saving_the_same_id_overwrites() {
        let (mut store, first, _) = store_with_two_threads();
        let mut replacement = ThreadMetadata::new(first.clone(), time(30));
        replacement.title = Some("改过".into());
        store.save(replacement);

        assert_eq!(store.entries().count(), 2, "同名 id 应覆盖而不是新增");
        assert_eq!(store.entry(&first).unwrap().title(), Some("改过".into()));
    }

    #[test]
    fn renaming_a_thread_sets_the_override_and_touches_the_store() {
        let (mut store, first, _) = store_with_two_threads();
        let before = store.entry(&first).unwrap().updated_at;
        store.set_title_override(&first, "我改的".into());

        let thread = store.entry(&first).unwrap();
        assert_eq!(thread.title_override.as_deref(), Some("我改的"));
        assert!(thread.updated_at >= before);
    }

    #[test]
    fn interacting_updates_both_timestamps() {
        let (mut store, first, _) = store_with_two_threads();
        store.update_interacted_at(&first, time(99));

        let thread = store.entry(&first).unwrap();
        assert_eq!(thread.interacted_at, Some(time(99)));
        assert_eq!(thread.updated_at, time(99));
    }

    #[test]
    fn archived_threads_leave_the_main_list() {
        let (mut store, first, second) = store_with_two_threads();
        store.archive(&first);

        let listed: Vec<_> = store
            .entries()
            .filter(|thread| !thread.archived)
            .map(|thread| thread.thread_id.clone())
            .collect();
        assert_eq!(listed, vec![second.clone()]);
        assert_eq!(store.archived_entries().count(), 1);
    }

    #[test]
    fn deleting_removes_the_thread() {
        let (mut store, first, _) = store_with_two_threads();
        store.delete(&first);
        assert!(store.entry(&first).is_none());
    }

    #[test]
    fn updating_an_unknown_thread_is_ignored() {
        let (mut store, _, _) = store_with_two_threads();
        let ghost = ThreadId::new("ghost");
        store.update_interacted_at(&ghost, time(1));
        store.set_title_override(&ghost, "不该出现".into());
        assert_eq!(store.entries().count(), 2);
    }
}
