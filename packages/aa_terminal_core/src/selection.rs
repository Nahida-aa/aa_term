//! 选区与滚轮滚动。
//!
//! 选择不自己维护状态，直接走 alacritty 的 `Term::selection`：它已经把
//! 简单选区、块选区、语义（双击选词）、行选区都实现好了，而且复制时会
//! 正确处理宽字符与自动换行。自己写一份只会得到行为不一致的结果。
//!
//! ## 两边命名的对应
//!
//! alacritty 的坐标是**行号越小越靠上**（`Line(0)` 是视口首行，向下递增），
//! 而我们的 [`crate::grid::Point`] 是 `line = row - display_offset`（ Zed 约定，
//! 向下递增时 line 减小）。两者**不一致**，所以这里提供 `viewport_to_point`
//! 做转换，别把两套坐标系的值直接塞给 alacritty。

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Point as AlacPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};

use crate::buffer::{Session, Terminal};
use crate::grid::{GridGeometry, Rounding, SelectionSide};

impl Terminal {
    /// 像素坐标 → alacritty 的绝对网格点。
    ///
    /// 像素坐标相对**视口**左上角；返回值是绝对 `Line`（含滚动偏移），
    /// 因为 `Term::selection` 存的是绝对坐标。
    pub fn selection_point(&self, x: f32, y: f32, geometry: &GridGeometry) -> AlacPoint {
        // 截断取整：选区起点/终点应落在 cell 内，四舍五入会让点击半个 cell 时
        // 跳到下一格去。
        let point = geometry.point_at(x, y, Rounding::Truncate);
        let (columns, screen_lines) = self.dimensions();
        viewport_to_point(
            self.scroll_offset(),
            AlacPoint::new(
                point.line.clamp(0, screen_lines as i32 - 1) as usize,
                Column(point.column.min(columns.saturating_sub(1))),
            ),
        )
    }

    /// 命中的 column 落在 cell 左半边还是右半边。
    pub fn selection_side(&self, x: f32, geometry: &GridGeometry) -> Side {
        if geometry.side_at(x) == SelectionSide::Right {
            Side::Right
        } else {
            Side::Left
        }
    }

    /// 当前从底部回退了多少行（即滚动偏移）。
    ///
    /// 选区存的是**绝对**坐标，所以必须把视口坐标换算成绝对的 —— 少了这一步，
    /// 往上滚过之后选中的内容会整体错位同样的行数。
    pub fn scroll_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    /// 开始一次选区。
    pub fn start_selection(&self, ty: SelectionType, x: f32, y: f32, geometry: &GridGeometry) {
        let location = self.selection_point(x, y, geometry);
        let side = self.selection_side(x, geometry);
        let mut term = self.term.lock();
        term.selection = Some(Selection::new(ty, location, side));
    }

    /// 拖动更新选区终点。
    pub fn update_selection(&self, x: f32, y: f32, geometry: &GridGeometry) {
        let location = self.selection_point(x, y, geometry);
        let side = self.selection_side(x, geometry);
        let mut term = self.term.lock();
        if let Some(selection) = &mut term.selection {
            selection.update(location, side);
        }
    }

    /// 结束选区（清空）。
    pub fn clear_selection(&self) {
        let mut term = self.term.lock();
        term.selection = None;
    }

    /// 当前选中区域的文本。
    pub fn selected_text(&self) -> Option<String> {
        let term = self.term.lock();
        term.selection_to_string()
    }

    /// 是否有选区。
    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }
}

impl Session {
    /// 滚轮滚动。
    ///
    /// `delta` 为正表示往上（看更早的内容）。
    ///
    /// ## alternate screen 的处理
    ///
    /// 全屏程序（vim、less）的 alternate screen 上没有回滚区，`scroll_display`
    /// 无效。此时要把滚动转成方向键回发给程序 —— 这是 tmux / less 期望的行为；
    /// 自己消费掉滚动会让滚轮在全屏程序里彻底失效。
    ///
    /// 放在 `Session` 上而不是 `Terminal` 上：回发需要 `Notifier`，而它只在
    /// 会话里。
    pub fn scroll(&self, delta: i32) {
        if delta == 0 {
            return;
        }
        let needs_reply = self
            .terminal()
            .term
            .lock()
            .mode()
            .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN);

        if needs_reply {
            let line_cmd = if delta > 0 { b'A' } else { b'B' };
            let mut content = Vec::with_capacity(delta.unsigned_abs() as usize);
            for _ in 0..delta.abs() {
                content.extend_from_slice(&[0x1b, b'O', line_cmd]);
            }
            self.write(content);
        } else {
            self.terminal()
                .term
                .lock()
                .grid_mut()
                .scroll_display(Scroll::Delta(delta));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;

    use alacritty_terminal::selection::SelectionType;

    #[test]
    fn selecting_a_range_yields_exactly_that_text() {
        let mut terminal = terminal();
        terminal.feed(b"hello world");
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);

        // 从 (0,0) 拖到 (6 格, 0)：选中 "hello "
        terminal.start_selection(SelectionType::Simple, 0., 0., &geometry);
        terminal.update_selection(6. * 10., 0., &geometry);

        let text = terminal.selected_text().expect("应有选中文本");
        assert_eq!(text, "hello ", "实际选中: {text:?}");
    }

    #[test]
    fn selection_is_cleared_before_the_next_one() {
        let mut terminal = terminal();
        terminal.feed(b"hello world");
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);

        terminal.start_selection(SelectionType::Simple, 0., 0., &geometry);
        terminal.update_selection(5. * 10., 0., &geometry);
        assert!(terminal.has_selection());

        terminal.clear_selection();
        assert!(!terminal.has_selection(), "clear 后不应再报告有选区");
        assert!(terminal.selected_text().is_none());
    }

    #[test]
    fn dragging_a_selection_updates_its_end() {
        let mut terminal = terminal();
        terminal.feed(b"one two three four");
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);

        terminal.start_selection(SelectionType::Simple, 0., 0., &geometry);
        terminal.update_selection(3. * 10., 0., &geometry);
        assert_eq!(terminal.selected_text().unwrap(), "one");

        terminal.update_selection(7. * 10., 0., &geometry);
        assert_eq!(terminal.selected_text().unwrap(), "one two");
    }

    #[test]
    fn selection_end_clamps_to_the_grid() {
        let mut terminal = terminal();
        terminal.feed(b"short");
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);

        // 拖到远超屏幕右下角：不应 panic，而是夹到最后一格
        terminal.start_selection(SelectionType::Simple, 0., 0., &geometry);
        terminal.update_selection(99_999., 99_999., &geometry);
        let text = terminal.selected_text().expect("越界后仍应有选中文本");
        assert!(text.contains("short"), "实际: {text:?}");
    }

    #[test]
    fn selection_survives_scrolling_up() {
        // 选中后往上滚，选中内容仍然对得上：这是把视口坐标换算成绝对坐标的
        // 意义所在，少了这一步选区会整体错位。
        let mut terminal = terminal();
        for i in 0..40 {
            terminal.feed(format!("line-{i:02}\r\n").as_bytes());
        }
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);

        terminal.start_selection(SelectionType::Simple, 0., 0., &geometry);
        terminal.update_selection(7. * 10., 0., &geometry);
        let before = terminal.selected_text().unwrap();
        assert!(before.starts_with("line-"), "实际: {before:?}");

        // 注意：滚动由 Session 处理（alternate screen 要回发输入），
        // 这里用 TestTerminal 的裸网格验证坐标换算。
        let offset = terminal.scroll_offset();
        terminal
            .term
            .lock()
            .grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(3));
        assert_ne!(terminal.scroll_offset(), offset, "滚动应改变偏移");
        assert_eq!(
            terminal.selected_text().unwrap(),
            before,
            "滚动后选中文本不应变"
        );
    }

    #[test]
    fn search_highlights_the_whole_scrollback() {
        // Zed 的 search_matches 用 topmost..=bottommost 定义范围，即整块可滚动
        // 内容。少了这条，往上滚出去的命中就丢了。
        let mut terminal = terminal();
        for i in 0..40 {
            terminal.feed(format!("line-{i:02}\r\n").as_bytes());
        }
        terminal.feed(b"needle here");

        let count = terminal.set_search("needle");
        assert_eq!(count, 1, "应命中一次");

        let ranges = terminal.search_matches();
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn search_with_an_invalid_regex_clears_instead_of_panicking() {
        // 用户敲正则的每一步都可能是临时非法的（比如刚输完 `[`）。
        let mut terminal = terminal();
        terminal.feed(b"abc");
        assert_eq!(terminal.set_search("["), 0, "非法正则应清空");
        assert!(terminal.search_matches().is_empty());
    }

    #[test]
    fn empty_query_clears_previous_highlights() {
        let mut terminal = terminal();
        terminal.feed(b"abc");
        terminal.set_search("abc");
        assert_eq!(terminal.search_matches().len(), 1);

        terminal.set_search("");
        assert!(terminal.search_matches().is_empty(), "空查询应清空");
    }

    #[test]
    fn search_and_selection_can_coexist() {
        // 两者走同一份 highlight_ranges，这才是抽象存在的意义。
        let mut terminal = terminal();
        terminal.feed(b"abc abc");
        terminal.set_search("abc");
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);
        terminal.start_selection(crate::SelectionType::Simple, 6. * 10., 0., &geometry);

        let highlights = terminal.highlight_ranges();
        assert!(
            highlights.len() >= 2,
            "选区与搜索命中应同时上报: {highlights:?}"
        );
    }

    #[test]
    fn point_conversion_matches_the_line_convention() {
        // 视口第 0 行在 offset=0 时对应绝对 line 0。
        let mut terminal = terminal();
        terminal.feed(b"abc");
        let geometry = crate::grid::GridGeometry::new(800., 480., 10., 20.);
        let point = terminal.selection_point(0., 0., &geometry);
        assert_eq!(point.line.0, 0, "视口首行应对应 line 0");
        assert_eq!(point.column.0, 0);

        // 往下 2 行：我们的 Point.line 是 2（向下递增），绝对 line 同样是 2
        let point = terminal.selection_point(0., 2. * 20., &geometry);
        assert_eq!(point.line.0, 2);
    }

    #[test]
    fn viewport_point_uses_the_documented_sign() {
        // alacritty 的辅助函数：absolute = viewport - display_offset。
        // 我们传入的视口行号已经减过 display_offset 了吗？没有 ——
        // 这里验证明「谁做这个减法」。
        use alacritty_terminal::index::Point as P;
        use alacritty_terminal::term::viewport_to_point;

        use alacritty_terminal::index::Column;
        let converted = viewport_to_point(
            3,
            P {
                line: 0,
                column: Column(5),
            },
        );
        assert_eq!(converted.line.0, -3, "offset 3 时视口首行的绝对 line 是 -3");
        assert_eq!(converted.column.0, 5);
    }
}

#[cfg(test)]
mod tests_support {
    use crate::buffer::Terminal;
    use crate::buffer::tests::palette;
    use crate::grid::GridGeometry;
    use crate::session::window_size_from;

    pub(super) fn terminal() -> Terminal {
        let size = window_size_from(&GridGeometry::new(800., 480., 10., 20.));
        Terminal::for_test(size, palette())
    }
}

/// 高亮的种类。
///
/// ## 为什么要有这层
///
/// Zed 的 `terminal_element.rs` 把选区和搜索结果**统一成一份**
/// `relative_highlighted_ranges: Vec<(Range, Hsla)>`，选区只是另一种高亮。
/// 这样做的直接好处：渲染层只需要实现「按 Range 铺底色」一件事，选区、搜索
/// 命中、乃至以后的括号配对高亮都走同一条路。分开写的话，每加一种高亮都要
/// 改一遍绘制代码，而且各自的颜色混合顺序很容易对不上。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightKind {
    /// 用户拖出的选区。
    Selection,
    /// 搜索结果命中。
    SearchMatch,
}

impl Terminal {
    /// 当前选区对应的网格范围（未规范化，方向可能与拖拽方向相反）。
    pub fn selection_range(&self) -> Option<crate::grid::SelectionRange> {
        let term = self.term.lock();
        let range = term.selection.as_ref().and_then(|s| s.to_range(&term))?;
        Some(crate::grid::SelectionRange {
            // alacritty 的绝对坐标 → 我们的视口坐标。
            // line 方向相反：alacritty 是「越小越靠上」，我们是「越大越靠上」。
            start: crate::grid::Point::new(
                range.start.line.0 + self.scroll_offset() as i32,
                range.start.column.0,
            ),
            end: crate::grid::Point::new(
                range.end.line.0 + self.scroll_offset() as i32,
                range.end.column.0,
            ),
            is_block: range.is_block,
        })
    }

    /// 需要绘制的所有高亮区域。
    ///
    /// 返回的是「视口坐标 + 种类」，颜色交给渲染层按调色板决定 —— core 不
    /// 假设配色来自哪里。
    ///
    /// 顺序即绘制顺序：选区压搜索命中，因为用户主动选中的内容应该最醒目。
    pub fn highlight_ranges(&self) -> Vec<(crate::grid::Range, HighlightKind)> {
        let mut ranges = Vec::new();
        if let Some(selection) = self.selection_range() {
            ranges.push((selection.normalized(), HighlightKind::Selection));
        }
        ranges.extend(
            self.search_ranges()
                .into_iter()
                .map(|range| (range, HighlightKind::SearchMatch)),
        );
        ranges
    }

    /// 设置搜索词，返回命中数量。
    ///
    /// 搜索范围是**整块可滚动内容**（`topmost_line..=bottommost_line`），不只是
    /// 可见屏 —— 这正是用户预期：往上翻也能看到命中高亮。这段范围取自 Zed 的
    /// `alacritty/search.rs`。
    pub fn set_search(&self, query: &str) -> usize {
        // 空查询等于清空：用户删光搜索词时不该残留高亮。
        if query.is_empty() {
            self.clear_search();
            return 0;
        }

        let regex = match alacritty_terminal::term::search::RegexSearch::new(query) {
            Ok(regex) => regex,
            // 正则非法时清空而不是 panic：用户敲键盘的每一步都可能构成一个
            // 临时非法的式子（例如刚输完 `[`）。
            Err(_) => {
                self.clear_search();
                return 0;
            }
        };

        let term = self.term.lock();
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Direction, Point as AlacPoint};
        use alacritty_terminal::term::search::RegexIter;

        let start = AlacPoint::new(term.grid().topmost_line(), Column(0));
        let end = AlacPoint::new(term.grid().bottommost_line(), term.grid().last_column());
        // RegexIter 要 &mut（它内部推进游标），且 Item 就是 RangeInclusive<Point>，
        // 没有 .range 字段 —— alacritty 的 `pub type Match = RangeInclusive<Point>`。
        let mut regex = regex;
        let matches: Vec<_> =
            RegexIter::new(start, end, Direction::Right, &term, &mut regex).collect();
        let count = matches.len();
        *self.search_hits.lock().unwrap_or_else(|e| e.into_inner()) = matches;
        count
    }

    /// 清空搜索高亮。
    pub fn clear_search(&self) {
        self.search_hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 当前的搜索命中。
    pub fn search_matches(&self) -> Vec<crate::grid::Range> {
        self.search_ranges()
    }

    fn search_ranges(&self) -> Vec<crate::grid::Range> {
        // alacritty 的 `Match` 是 `RangeInclusive<Point>`，不是 Range。
        self.search_hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|hit| {
                // RangeInclusive 的字段私有，取端点要用 start() / end()。
                let (start, end) = (hit.start(), hit.end());
                // alacritty 搜的是绝对坐标，绘制要的是视口坐标，方向换算同选区。
                crate::grid::Range::new(
                    crate::grid::Point::new(
                        start.line.0 + self.scroll_offset() as i32,
                        start.column.0,
                    ),
                    crate::grid::Point::new(end.line.0 + self.scroll_offset() as i32, end.column.0),
                )
            })
            .collect()
    }
}
