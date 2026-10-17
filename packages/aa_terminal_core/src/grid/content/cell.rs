//! 一个 cell 的快照，以及带视口坐标的 cell。
//!
//! 对应 Zed `terminal/src/terminal.rs` 的 `Cell` / `IndexedCell`：把 alacritty
//! 的 cell **原样**包起来，前景/背景仍然是符号化的 `Color`，等渲染层拿到主题
//! 调色板再解析。之前我们存的是解析好的 `RenderCell`，快照自然比 zed 大
//! （实测 68 字节 vs 24 字节）而且解析发生在**持锁**期间；改成这样之后快照只
//! 拷贝、不解析，锁的持有时间与每帧拷贝量都降下来。

use std::ops::Deref;

use alacritty_terminal::term::cell::Cell as AlacrittyCell;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Color;

use crate::grid::Point;

/// 一个可见 cell。
///
/// 与 Zed 一样只暴露**语义化访问器**（`is_bold` / `has_underline`…），不把
/// alacritty 的 `Flags` 泄漏出去 —— 渲染层不该自己去拼位运算，拼错了不报错。
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct Cell {
    pub(crate) cell: AlacrittyCell,
}

impl Cell {
    /// 从 alacritty 的 cell 拷贝一份。
    ///
    /// 对应 Zed 的 `terminal_cell_from_alacritty`。只克隆，不解析颜色。
    pub fn from_alacritty(cell: &AlacrittyCell) -> Self {
        Self { cell: cell.clone() }
    }

    /// 字符。
    ///
    /// 未写入的 cell 是 `'\0'`，渲染层容易把它画成豆腐块，统一成空格 ——
    /// Zed 原样返回 `'\0'`，那里靠绘制前跳过空 cell 规避，我们保持自己
    /// 之前的语义（`RenderCell::c` 就是这么做的），省得每个消费方各判一次。
    pub fn character(&self) -> char {
        if self.cell.c == '\0' {
            ' '
        } else {
            self.cell.c
        }
    }

    /// 前景色（符号化，未解析）。
    pub fn foreground(&self) -> Color {
        self.cell.fg
    }

    /// 背景色（符号化，未解析）。
    pub fn background(&self) -> Color {
        self.cell.bg
    }

    /// 下划线颜色（SGR 58）。
    ///
    /// `None` 表示「跟随前景色」；只有程序显式设过才有值。回退交给渲染层做
    /// （它已经拿着前景色）。
    pub fn underline_color(&self) -> Option<Color> {
        self.cell.underline_color()
    }

    /// 是否属于一个 OSC 8 超链接。
    ///
    /// 只给 bool，不给 URI：Zed 的 `Cell::hyperlink()` 每格都要现场构造一个
    /// 带 `Arc` 的包装类型，而渲染只关心「要不要当链接画」；真正要 URI 的只有
    /// 悬停/点击那一下，走 [`crate::Terminal::hyperlink_at`] 现取。
    pub fn has_hyperlink(&self) -> bool {
        self.cell.hyperlink().is_some()
    }

    /// 是否反色（高亮搜索命中时要反转处理）。
    pub fn is_inverse(&self) -> bool {
        self.cell.flags.contains(Flags::INVERSE)
    }

    /// 是否是宽字符占位的第二格。
    pub fn is_wide_char_spacer(&self) -> bool {
        self.cell.flags.contains(Flags::WIDE_CHAR_SPACER)
    }

    /// 是否变暗（SGR 2）。
    pub fn is_dim(&self) -> bool {
        self.cell.flags.contains(Flags::DIM)
    }

    /// 是否有下划线（含下卷曲，对应 Zed 的 `ALL_UNDERLINES`）。
    pub fn has_underline(&self) -> bool {
        self.cell.flags.intersects(Flags::ALL_UNDERLINES)
    }

    /// 是否有删除线。
    pub fn has_strikeout(&self) -> bool {
        self.cell.flags.intersects(Flags::STRIKEOUT)
    }

    /// 是否加粗。
    pub fn is_bold(&self) -> bool {
        self.cell.flags.intersects(Flags::BOLD)
    }

    /// 是否斜体。
    pub fn is_italic(&self) -> bool {
        self.cell.flags.intersects(Flags::ITALIC)
    }
}

/// 带视口坐标的 cell。
///
/// `point` 用 [`Point`] 的约定（**视口行号**，0 = 视口顶行）；Zed 这里存的是
/// alacritty 的**绝对** `Line`（回滚区为负），因为它的渲染层是按行分组后靠
/// 枚举下标定位的。我们 core 的 `Point` 一律是视口坐标（选区、命中测试、
/// `cell()` 都如此），所以转换放在 [`super::Content::new`] 里，渲染层拿到的
/// 行列可以直接定位。
#[derive(Debug, Clone)]
pub struct IndexedCell {
    /// 视口坐标。
    pub point: Point,
    /// 该位置的 cell。
    pub cell: Cell,
}

impl Deref for IndexedCell {
    type Target = Cell;

    #[inline]
    fn deref(&self) -> &Cell {
        &self.cell
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_with(flags: Flags) -> Cell {
        Cell::from_alacritty(&AlacrittyCell {
            flags,
            ..Default::default()
        })
    }

    #[test]
    fn flags_are_exposed_through_named_accessors() {
        assert!(cell_with(Flags::BOLD).is_bold());
        assert!(cell_with(Flags::ITALIC).is_italic());
        assert!(cell_with(Flags::DIM).is_dim());
        assert!(cell_with(Flags::INVERSE).is_inverse());
        assert!(cell_with(Flags::STRIKEOUT).has_strikeout());
        assert!(cell_with(Flags::UNDERLINE).has_underline());
        assert!(
            cell_with(Flags::UNDERCURL).has_underline(),
            "下卷曲也算下划线"
        );
        assert!(cell_with(Flags::WIDE_CHAR_SPACER).is_wide_char_spacer());

        let plain = cell_with(Flags::empty());
        assert!(!plain.is_bold() && !plain.is_dim() && !plain.has_underline());
    }

    #[test]
    fn unwritten_cells_read_as_space() {
        // alacritty 未写入的 cell 是 '\0'，直接画就是豆腐块
        let cell = cell_with(Flags::empty());
        assert_eq!(cell.character(), ' ');
    }

    #[test]
    fn indexed_cell_derefs_to_its_cell() {
        // Zed 与 AAgent 都给 IndexedCell 实现了 Deref，渲染层因此可以
        // `indexed.is_bold()` 而不必写 `indexed.cell.is_bold()`。
        let indexed = IndexedCell {
            point: Point::new(2, 3),
            cell: cell_with(Flags::BOLD),
        };
        assert!(indexed.is_bold());
        assert_eq!(indexed.point, Point::new(2, 3));
    }
}
