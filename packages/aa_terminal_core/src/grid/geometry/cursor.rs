//! 光标：位置 + 形状。
//!
//! 与 AAgent 的 `packages/terminal/src/grid/geometry/cursor.rs` 同构 —— Zed 把
//! 这两个类型和 `Point` 挤在单文件 `terminal::terminal.rs` 里（不分模块），
//! AAgent 拆分时把它们放到了几何目录下；这里跟着 AAgent 走。
//!
//! 和 [`Point`] 放一起是因为 `Cursor` 就是「一个 `Point` 加一个形状」。
//! **闪烁相位不在这里** —— 它是状态机，见 [`crate::blink`]。

use super::point::Point;

/// 光标形状。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorShape {
    /// 实心块。
    Block,
    /// 下划线。
    Underline,
    /// 竖线。
    Bar,
    /// 空心块（用背景色描边）。
    HollowBlock,
    /// 不显示。
    Hidden,
}

impl CursorShape {
    /// 是否应当绘制。
    pub fn is_visible(&self) -> bool {
        !matches!(self, CursorShape::Hidden)
    }
}

/// 光标的完整可视状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    /// 形状。
    pub shape: CursorShape,
    /// 位置。
    pub point: Point,
    /// 是否处于可见相位（闪烁中）。
    pub visible: bool,
}

impl Cursor {
    /// 形状隐藏、或处于闪烁的暗相位时都不画。
    pub fn should_render(&self) -> bool {
        self.shape.is_visible() && self.visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_render_decides_on_shape_and_phase() {
        let base = Cursor {
            shape: CursorShape::Block,
            point: Point::new(0, 0),
            visible: true,
        };
        assert!(base.should_render());

        let hidden = Cursor {
            shape: CursorShape::Hidden,
            ..base
        };
        assert!(!hidden.should_render(), "Hidden 形状不画");

        let dim = Cursor {
            visible: false,
            ..base
        };
        assert!(!dim.should_render(), "闪烁暗相位不画");

        for shape in [
            CursorShape::Block,
            CursorShape::Bar,
            CursorShape::Underline,
            CursorShape::HollowBlock,
        ] {
            assert!(Cursor { shape, ..base }.should_render());
        }
    }
}
