//! 网格坐标 —— 行/列与像素之间的模型。
//!
//! 坐标系有两套，用之前先分清：
//!
//! - [`Point`]: `(line, column)`。`line` 对应 alacritty 的 `Line`，**以视口顶部
//!   为 0、允许为负**（负数表示视口上方还没被滚动出去的显示行）。滚动改变的是
//!   这个映射，不是 `line` 本身。
//! - 像素坐标 `(x, y)`：**相对于视口原点**。带滚动偏移的坐标要先减掉
//!   `scroll_top` 再进来。
//!
//! 混用这两套是 Zed / AAgent 里最常见的几何 bug 来源，所以这里把换算收敛到
//! [`GridGeometry`] 的几个方法上，不让调用方各自写除法。

pub mod geometry;

pub use geometry::cursor::{Cursor, CursorShape};
pub use geometry::point::{Point, Range, SelectionRange};

/// 命中位置落在 cell 的左半边还是右半边。
///
/// 双击选词时需要这个：命中右半边才算词尾，词要多吃一格。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionSide {
    /// cell 左半边。
    Left,
    /// cell 右半边。
    Right,
}

/// 像素换算成列/行时的取整方式。
///
/// 见 [`GridGeometry::point_at`] 里为什么必须显式选。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rounding {
    /// 截断。`4.9 / 5.0 → 0`。
    Truncate,
    /// 四舍五入。`4.9 / 5.0 → 1`。
    Round,
}

/// 视口尺寸 + cell 尺寸，决定了几何换算的一切。
///
/// 刻意用裸 `f32` 而不是 gpui 的 `Pixels`：本 crate 不依赖任何 UI 框架，
/// iced 侧在边界处转换即可。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridGeometry {
    /// 视口宽（像素）。
    pub width: f32,
    /// 视口高（像素）。
    pub height: f32,
    /// 单个 cell 的宽（像素）。
    pub cell_width: f32,
    /// 单行的行高（像素）。
    pub line_height: f32,
}

impl GridGeometry {
    /// 由视口尺寸与 cell 尺寸构造。
    pub fn new(width: f32, height: f32, cell_width: f32, line_height: f32) -> Self {
        Self {
            width,
            height,
            cell_width,
            line_height,
        }
    }

    /// 视口能放下多少**整行**。
    ///
    /// 这里用 `next_up().floor()` 而不是 `floor()`：浮点除法会得到 N-epsilon
    /// （例如 `30.0 / 1.0000001`），直接 `floor()` 会少算一行，把本该可见的
    /// 首行推进不可见的回滚区。Zed 与 AAgent 都有这个修正，别去掉。
    pub fn num_lines(&self) -> usize {
        if self.line_height <= 0.0 {
            return 0;
        }
        let raw = self.height / self.line_height;
        raw.next_up().floor().max(0.0) as usize
    }

    /// 视口能放下多少**整列**，同理带 `next_up()` 修正。
    pub fn num_columns(&self) -> usize {
        if self.cell_width <= 0.0 {
            return 0;
        }
        let raw = self.width / self.cell_width;
        raw.next_up().floor().max(0.0) as usize
    }

    /// 把视口内像素坐标换算成网格位置，超出视口的部分会被夹到边界。
    ///
    /// ## 为什么 `rounding` 是显式参数
    ///
    /// Zed / AAgent 里存在两套并存的换算，结论并不一致：
    ///
    /// | 用途 | 位置 | 公式 |
    /// |---|---|---|
    /// | 拖选命中 | `mappings/mouse.rs` | `(pos.x / cell_width) as usize` |
    /// | 双击选词 | `mouse.rs::content_index_for_mouse` | `(...).round() as usize` |
    ///
    /// `cell_width = 8`、`pos.x = 6` 时，前者得第 0 列、后者得第 1 列 —— 同一
    /// 个点命中不同格子。两边都存在于上游，所以照抄就会把这个歧义一起继承。
    /// 这里不替上游做决定，而是把它变成调用方必须显式声明的选择。
    pub fn point_at(&self, x: f32, y: f32, rounding: Rounding) -> Point {
        let column = self.coordinate(x, self.cell_width, rounding).max(0.0) as usize;
        let last_line = i32::try_from(self.num_lines().saturating_sub(1)).unwrap_or(i32::MAX);
        // 行号先转 i32 再夹取：负的 `line` 是「视口上方的显示行」，命中测试
        // 要把它夹回 0，但网格模型本身必须允许它为负。
        let line = (self.coordinate(y, self.line_height, rounding) as i32).clamp(0, last_line);

        Point {
            line,
            column: column.min(self.num_columns().saturating_sub(1)),
        }
    }

    /// 取整后的「第几格」，返回 `f32` 而非整数 —— 行号要落成 `i32`
    /// （可以是负数），列号要落成 `usize`（负列无意义），由调用方各自转换。
    fn coordinate(&self, position: f32, extent: f32, rounding: Rounding) -> f32 {
        if extent <= 0.0 {
            return 0.0;
        }
        let ratio = position / extent;
        match rounding {
            Rounding::Truncate => ratio.floor(),
            Rounding::Round => ratio.round(),
        }
    }

    /// 命中位置在 cell 的左半边还是右半边。
    ///
    /// 越界与负值都算左边 —— 视口外没有 cell 可言。
    pub fn side_at(&self, x: f32) -> SelectionSide {
        if self.cell_width <= 0.0 || x <= 0.0 {
            return SelectionSide::Left;
        }
        if (x % self.cell_width) > self.cell_width / 2.0 {
            SelectionSide::Right
        } else {
            SelectionSide::Left
        }
    }

    /// 某个网格位置左上角的视口内像素坐标。
    pub fn origin_of(&self, point: Point) -> (f32, f32) {
        (
            point.column as f32 * self.cell_width,
            point.line as f32 * self.line_height,
        )
    }

    /// 某个网格位置覆盖的像素矩形 `(x, y, w, h)`。
    pub fn rect_of(&self, point: Point) -> (f32, f32, f32, f32) {
        let (x, y) = self.origin_of(point);
        (x, y, self.cell_width, self.line_height)
    }

    /// 折算成 `cells[row * num_columns + col]` 的扁平下标。
    ///
    /// 只对**视口内**的点有意义；越界会被夹进来。这正是
    /// `content_index_for_mouse` 的用途（从缓冲区里取 cell）。
    pub fn content_index(&self, point: Point, rounding: Rounding) -> usize {
        let clamped = self.point_at(
            point.column as f32 * self.cell_width,
            point.line as f32 * self.line_height,
            rounding,
        );
        clamped.line as usize * self.num_columns() + clamped.column
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> GridGeometry {
        // 80x24 标准网格，故意选能被整除的尺寸以隔离取整逻辑。
        GridGeometry::new(800., 480., 10., 20.)
    }

    #[test]
    fn counts_whole_cells() {
        let g = geometry();
        assert_eq!(g.num_columns(), 80);
        assert_eq!(g.num_lines(), 24);
    }

    #[test]
    fn next_up_keeps_a_row_that_floor_would_lose() {
        // 这是 Zed 的修正点：浮点除法得到 N-epsilon，直接 floor 会少一行。
        // 构造一个整除但因浮点表示略小于整除的情形。
        let cell = 10.000001_f32;
        let g = GridGeometry::new(800., 240. * cell, 10., cell);
        assert!(
            g.num_lines() >= 240,
            "整除的行数不该被 floor 抹掉，实际 {}",
            g.num_lines()
        );
    }

    #[test]
    fn zero_or_negative_extents_never_divide() {
        // 退化的是「那个轴」，健康的那个轴照常算。
        for (w, h, lines, columns) in [
            (0., 0., 0, 0),
            (-10., -10., 0, 0),
            (100., 0., 0, 10), // 高度为 0 → 无行，但宽度仍能放下 10 列
            (0., 100., 5, 0),
        ] {
            let g = GridGeometry::new(w, h, 10., 20.);
            assert_eq!(g.num_lines(), lines, "h={h} w={w}");
            assert_eq!(g.num_columns(), columns, "h={h} w={w}");
        }
    }

    #[test]
    fn origin_and_point_at_are_inverse_within_a_cell() {
        let g = geometry();
        for line in 0..g.num_lines() {
            for column in 0..g.num_columns() {
                let point = Point::new(line as i32, column);
                let (x, y) = g.origin_of(point);
                // 取 cell 正中间，避开边界上取整归属相邻格的问题。
                let back = g.point_at(
                    x + g.cell_width / 2.,
                    y + g.line_height / 2.,
                    Rounding::Truncate,
                );
                assert_eq!(back, point, "({line},{column}) 往返失败");
            }
        }
    }

    #[test]
    fn truncate_and_round_disagree_on_the_half_cell() {
        // 这条测试就是记录上游那处不一致：cell_width=10、x=6，
        // 截断得第 0 列，四舍五入得第 1 列。
        let g = GridGeometry::new(800., 480., 10., 20.);
        assert_eq!(g.point_at(6., 0., Rounding::Truncate).column, 0);
        assert_eq!(g.point_at(6., 0., Rounding::Round).column, 1);
        assert_eq!(g.point_at(8., 0., Rounding::Truncate).column, 0);
        assert_eq!(g.point_at(8., 0., Rounding::Round).column, 1);

        // 两者是否一致，取决于该 cell 内的**小数部分**，而不是绝对位置：
        // floor(r) 与 round(r) 只在 frac(r) >= 0.5 时才分叉。
        // 这里穷举一个 cell 内的所有 0.5 步进，把规律本身钉住 ——
        // 之前手挑几个点断言，把「过了中点就一致」写成了错的。
        let mut x = 0.0_f32;
        while x < 10.0 {
            let trunc = g.point_at(x, 0., Rounding::Truncate).column;
            let round = g.point_at(x, 0., Rounding::Round).column;
            let ratio = x / 10.;
            let frac = ratio - ratio.floor();
            if frac < 0.5 {
                assert_eq!(trunc, round, "x={x}: frac={frac} < 0.5 时应一致");
            } else {
                assert_eq!(round, trunc + 1, "x={x}: frac={frac} >= 0.5 时应进位");
            }
            x += 0.5;
        }
    }

    #[test]
    fn point_at_clamps_to_the_viewport() {
        let g = geometry();
        // 远超视口 → 夹到最后一格
        let last = g.point_at(99999., 99999., Rounding::Truncate);
        assert_eq!(last.line, 23);
        assert_eq!(last.column, 79);
        // 负坐标夹到第 0 行第 0 列
        let neg = g.point_at(-50., -50., Rounding::Truncate);
        assert_eq!(neg.line, 0);
        assert_eq!(neg.column, 0);
    }

    #[test]
    fn side_at_splits_on_the_half_cell() {
        let g = geometry();
        assert_eq!(g.side_at(0.), SelectionSide::Left);
        assert_eq!(g.side_at(4.9), SelectionSide::Left);
        assert_eq!(g.side_at(5.1), SelectionSide::Right);
        assert_eq!(g.side_at(10.), SelectionSide::Left, "整格起点属左半边");
        // 负坐标与零宽
        assert_eq!(g.side_at(-3.), SelectionSide::Left);
        assert_eq!(
            GridGeometry::new(100., 100., 0., 20.).side_at(3.),
            SelectionSide::Left
        );
    }

    #[test]
    fn content_index_is_row_major_and_never_out_of_range() {
        let g = geometry();
        assert_eq!(g.content_index(Point::new(0, 0), Rounding::Truncate), 0);
        assert_eq!(g.content_index(Point::new(0, 1), Rounding::Truncate), 1);
        assert_eq!(g.content_index(Point::new(1, 0), Rounding::Truncate), 80);
        assert_eq!(
            g.content_index(Point::new(23, 79), Rounding::Truncate),
            24 * 80 - 1
        );

        // 越界点夹进来后仍在缓冲区内
        let idx = g.content_index(Point::new(9999, 9999), Rounding::Truncate);
        assert!(idx < g.num_lines() * g.num_columns(), "下标 {idx} 越界");
    }
}
