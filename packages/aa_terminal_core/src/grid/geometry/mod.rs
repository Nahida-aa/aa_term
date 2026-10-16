//! 网格几何：位置的纯数据类型，以及行列与像素之间的换算。
//!
//! 换算式（`GridGeometry`）目前还在 [`super`] 里，纯数据类型先拆到这里。
//! 目录划分跟着 AAgent 的 `packages/terminal/src/grid/geometry/`。

pub mod cursor;
pub mod point;
