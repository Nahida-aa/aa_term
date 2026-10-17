//! Workspace 框架 —— 对应 Zed 的 `crates/workspace`。
//!
//! 目前只搬了承载线程侧边栏的那一层：[`multi_workspace`]。面板（`Panel`）、
//! dock、pane 组等等真有消费方再搬。
#![warn(missing_docs)]

pub mod multi_workspace;

pub use multi_workspace::{
    MultiWorkspace, SIDEBAR_RESIZE_HANDLE_SIZE, Sidebar, SidebarRenderState, SidebarSide,
    THREADS_LIST_MAX_WIDTH, THREADS_LIST_MIN_WIDTH,
};
