//! 查找栏：查询词、选项开关、命中跳转。
#![warn(missing_docs)]
#![warn(
    clippy::correctness,
    clippy::suspicious,
    clippy::style,
    clippy::complexity,
    clippy::perf
)]

//! 对应 Zed 的 `crates/search`，但只搬「框架无关的内核 + 一条 iced 的通用
//! 查找栏」，不照它的目录铺。
//!
//! 理由：Zed 那个 crate 有一万七千行，且深度绑 gpui（`Div` / `Entity` /
//! `Window` / `StatusItemView`），整包搬没意义；而终端实际只用到其中两样东西
//!
//! 1. **通用的查找栏**（Zed 的 `BufferSearchBar`）—— 编辑器和终端共用同一条；
//! 2. **视图侧的 adapter**（Zed 的 `workspace::SearchableItem`）—— 谁被搜索谁
//!    实现它，终端那侧只有 `supported_options` / `activate_match` 那几行。
//!
//! 这里一一对应地分成三层：
//!
//! | 本 crate | Zed 里的位置 | 命名 | 是否依赖框架 |
//! |---|---|---|---|
//! | [`SearchOptions`] / [`SearchOption`] | `search::SearchOptions` / `SearchOption` | 沿用（变体是子集） | 否 |
//! | [`SearchableItem`] | `workspace::SearchableItem` | 沿用（方法有增删） | 否（契约）/ 实现方定 |
//! | [`SearchBarState`] | `search::BufferSearchBar`（状态在那条 entity 里） | 自取 | 是（iced） |
//! | [`SearchMessage`] | action + `SearchEvent` | 自取 | 是（iced） |
//! | [`SearchStatus`] / [`view`] | 栏直接读命中情况 / `search_bar` | 自取 | 否 / 是（iced） |
//!
//! **命名约定**：能沿用上游（Zed / AAgent）名字的一律沿用，自取的那几处都在各自
//! 的文档注释里写了原委 —— 见 [`SearchBarState`] / [`SearchMessage`] /
//! [`SearchStatus`]、[`SearchableItem`] 的方法对照表，以及
//! `aa_terminal_core::search` 模块头的对照表。
//!
//! **分层约定**：栏不认识终端也不认识编辑器，只认 [`SearchableItem`]；栏不持有
//! 宿主的命中状态 —— 命中数与当前下标渲染时从 [`SearchStatus`] 现取，栏自己只
//! 管查询词与选项开关（Zed 也是这么分的，所以「当前第几处」在它那边归搜索 UI
//! 而不归终端）。消息由宿主映射：[`SearchMessage`] → 宿主自己的 `Message`。
//!
//! ```ignore
//! // 宿主（如 aa_term 的 App）侧：
//! let item = TerminalSearch::new(self.terminal);
//! let status = SearchStatus {
//!     matches: item.match_count(),
//!     current: item.active_match_index(),
//! };
//! search::view(state, status, item.supported_options(), Message::Search)
//! ```

pub mod bar;
pub mod item;
pub mod options;

pub use bar::{SEARCH_INPUT_ID, SearchBarState, SearchMessage, SearchStatus, view};
pub use item::SearchableItem;
pub use options::{SearchOption, SearchOptions};
