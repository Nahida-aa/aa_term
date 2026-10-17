//! 会话（thread）元数据 —— 侧边栏列表的数据源。
//!
//! 对应 Zed 的 `crates/agent_ui`，但只搬侧边栏真正要用的那一块：线程元数据与它
//! 的存储（Zed: `crates/agent_ui/src/thread_metadata_store.rs`）。conversation、
//! ACP、工具调用那些等有 agent 后端再说。
//!
//! Zed 那版的 store 是个 gpui `Global`（`ThreadMetadataStore::global(cx)`），因为
//! 面板、侧边栏、归档视图都要能随手拿到同一份；iced 这边没有全局 entity，store
//! 由宿主（[`MultiWorkspace`](../workspace/index.html) 之上的 App）持有并往下传。
#![warn(missing_docs)]

pub mod thread_metadata_store;

/// 新线程还没起标题时显示的占位标题。
///
/// 与 Zed 的 `agent_ui::DEFAULT_THREAD_TITLE` 同名同值。
pub const DEFAULT_THREAD_TITLE: &str = "New Agent Thread";

pub use thread_metadata_store::{SessionId, ThreadId, ThreadMetadata, ThreadMetadataStore};
