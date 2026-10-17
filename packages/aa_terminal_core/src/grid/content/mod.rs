//! 快照层 —— 把 `Term` 里的可见内容拷成可以脱离锁使用的数据。
//!
//! 对应 Zed `crates/terminal/src/grid/content/`（AAgent 同样是这个目录名）：
//! [`cell`] 是单格的包装，[`content`] 是整屏快照。

pub mod cell;
#[allow(clippy::module_inception)]
// 与 Zed / AAgent 的 `grid/content/content.rs` 保持同名：搬上游代码时路径能对上。
pub mod content;

pub use cell::{Cell, IndexedCell};
pub use content::Content;
