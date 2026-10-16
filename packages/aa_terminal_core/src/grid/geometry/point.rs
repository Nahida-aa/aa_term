//! 网格位置的纯数据类型：位置、区间、选区。
//!
//! 这一层只描述「网格里的一个位置/一段范围」，不碰像素，也不碰终端状态。

/// 网格中的一个位置。
///
/// `line` 是 `i32` 且可以为负；`column` 是 `usize`（负列没有意义）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Point {
    /// 行号，对应 alacritty 的 `Line`。
    pub line: i32,
    /// 列号，从 0 开始。
    pub column: usize,
}

impl Point {
    /// 构造一个网格位置。
    pub fn new(line: i32, column: usize) -> Self {
        Self { line, column }
    }
}

/// 一个闭区间 `[start, end]`。
///
/// 注意 `Point` 的 `Ord` 是按 `(line, column)` 字典序，因此可以直接用 `<=`
/// 判断包含关系，无需自己写比较。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Range {
    start: Point,
    end: Point,
}

impl Range {
    /// 构造一个闭区间。
    pub fn new(start: Point, end: Point) -> Self {
        Self { start, end }
    }

    /// 起点。
    pub fn start(&self) -> Point {
        self.start
    }

    /// 终点。
    pub fn end(&self) -> Point {
        self.end
    }

    /// 闭区间包含判定，两端都算在内。
    pub fn contains(&self, point: Point) -> bool {
        self.start <= point && point <= self.end
    }

    /// 规范化成 `start <= end`。
    ///
    /// 拖选方向可以和视觉顺序相反（从下往上、从右往左），不规范化的话
    /// `contains` 会恒为 false。
    pub fn normalized(self) -> Self {
        if self.start <= self.end {
            self
        } else {
            Self::new(self.end, self.start)
        }
    }
}

/// 一段选区，以及它是否按块（整格）选取。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    /// 起点。
    pub start: Point,
    /// 终点。
    pub end: Point,
    /// 是否按块（整格）选取。
    pub is_block: bool,
}

impl SelectionRange {
    /// 转成不含方向信息的 [`Range`]。
    pub fn point_range(self) -> Range {
        Range::new(self.start, self.end)
    }

    /// 规范化后的范围。选区拖反方向时必须用它来判包含关系。
    pub fn normalized(self) -> Range {
        self.point_range().normalized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_normalizes_reversed_drag() {
        let a = Point::new(5, 2);
        let b = Point::new(1, 9);
        let forward = Range::new(a, b);
        let backward = Range::new(b, a);

        // 不规范化时反向拖选的 contains 恒 false
        assert!(!forward.contains(Point::new(3, 5)));
        assert!(backward.contains(Point::new(3, 5)));
        assert!(forward.normalized().contains(Point::new(3, 5)));
        assert_eq!(backward.normalized(), forward.normalized());
    }

    #[test]
    fn range_contains_is_inclusive_on_both_ends() {
        let r = Range::new(Point::new(2, 3), Point::new(4, 5));
        assert!(r.contains(Point::new(2, 3)), "起点应含");
        assert!(r.contains(Point::new(4, 5)), "终点应含");
        assert!(!r.contains(Point::new(2, 2)));
        assert!(!r.contains(Point::new(4, 6)));
    }

    #[test]
    fn selection_range_normalizes_and_keeps_block_flag() {
        let sel = SelectionRange {
            start: Point::new(7, 1),
            end: Point::new(2, 8),
            is_block: true,
        };
        assert_eq!(sel.normalized().start(), Point::new(2, 8));
        assert_eq!(sel.normalized().end(), Point::new(7, 1));
        assert!(sel.is_block, "is_block 不应被规范化丢掉");
    }

    #[test]
    fn point_ordering_is_line_then_column() {
        assert!(Point::new(1, 9) < Point::new(2, 0));
        assert!(Point::new(1, 1) < Point::new(1, 2));
        // 负行排在最前 —— 视口上方的显示行
        assert!(Point::new(-1, 0) < Point::new(0, 0));
    }
}
