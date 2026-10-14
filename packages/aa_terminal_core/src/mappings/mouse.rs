//! 鼠标映射：像素坐标 → 终端语义，以及滚轮的方向键回退。
//!
//! 对应 Zed 的 `terminal/src/mappings/mouse.rs`。那边还包含鼠标事件上报
//! （`mouse_moved_report` / `mouse_button_report` / X10/SGR 协议）—— 那是
//! 「让终端里的 vim 响应鼠标」用的，我们这版还没接，先只做本地交互
//! （拖选 + 滚动）。
//!
//! ## 一个必须记住的约定
//!
//! iced 给的是**窗口内**像素坐标，而所有命中测试要的是**终端画布内**的坐标。
//! 应用层需要先减去画布原点再进来；少了这一步，鼠标点上方实际会命中下方
//! 对应位置，症状是「点哪都不对」。

use crate::buffer::Terminal;
use crate::grid::{GridGeometry, Point, Rounding, SelectionSide};

impl Terminal {
    /// 像素坐标 → 视口网格点。
    ///
    /// 截断而不是四舍五入：命中应该落在 cell 内，四舍五入会让点击半个 cell 时
    /// 跳到下一格去。这一点 Zed 与我们一致，但 AAEnt 下游还有另一套用
    /// `round()` 的实现，别混用。
    pub fn hit_test(&self, x: f32, y: f32, geometry: &GridGeometry) -> Point {
        geometry.point_at(x, y, Rounding::Truncate)
    }

    /// 命中的位置在 cell 左半边还是右半边。
    ///
    /// 块选区用得到：右半边的命中应该让选区多含一列。
    pub fn hit_test_side(&self, x: f32, geometry: &GridGeometry) -> SelectionSide {
        if geometry.side_at(x) == SelectionSide::Right {
            SelectionSide::Right
        } else {
            SelectionSide::Left
        }
    }
}
