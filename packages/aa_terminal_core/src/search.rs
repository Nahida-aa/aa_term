//! 终端的搜索：命中列表、当前命中，以及上下跳转。
//!
//! 对应 alacritty 的 `term/search.rs`，但**不每次重扫**：alacritty 的
//! `search_next` 从 origin 出发重跑一遍正则，而我们把全部命中一次性收集存下来，
//! 跳转只是移动下标 —— 用户每敲一次回车都重扫整块 scrollback 太亏。
//!
//! 命中坐标存的是 alacritty 的**绝对坐标**（不随滚动变）；对外给渲染和跳转的
//! 一律换算成**视口坐标**（0 = 视口顶，越往下越大），换算口只有 [`range_of`]
//! 一处，别在别的地方再写一遍加减偏移。

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Point as AlacPoint};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};

use crate::buffer::Terminal;
use crate::grid::{Point, Range};

/// 搜索状态：全部命中 + 当前跳转到的下标。
#[derive(Debug, Default)]
pub(crate) struct SearchState {
    /// 全部命中，alacritty 绝对坐标。
    matches: Vec<Match>,
    /// 当前命中在 `matches` 里的下标。
    ///
    /// `None` 表示**还没有开始跳转**（刚设完搜索词，或删空了词）。这样第一次
    /// 按回车会落在第一个命中上，而不是「第一个已经当前、于是被跳过」。
    current: Option<usize>,
}

/// 把一条 alacritty 绝对坐标的命中换算成视口范围。
///
/// `offset` 是当前 `display_offset`，由调用方一次性取出后传进来 —— 换算口只有
/// 这一处，且这个函数**不碰任何锁**。
fn range_of(hit: &Match, offset: i32) -> Range {
    // alacritty 的 `Match` 是 `RangeInclusive<Point>`，字段私有，取端点要用
    // start()/end()。
    let (start, end) = (hit.start(), hit.end());
    Range::new(
        Point::new(start.line.0 + offset, start.column.0),
        Point::new(end.line.0 + offset, end.column.0),
    )
}

impl Terminal {
    /// 设置搜索词，返回命中数量。
    ///
    /// 搜索范围是**整块可滚动内容**（`topmost_line..=bottommost_line`），不只是
    /// 可见屏 —— 这正是用户预期：往上翻也能看到命中高亮。范围取自 Zed 的
    /// `terminal/src/alacritty.rs` 的 `all_search_matches`。
    ///
    /// 设置会**重置跳转位置**（`current = None`）：换词之后还停在旧词的下标上
    /// 毫无意义。
    pub fn set_search(&self, query: &str) -> usize {
        let matches = self.scan(query);
        let count = matches.len();
        let mut state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        state.matches = matches;
        state.current = None;
        count
    }

    /// 清空搜索高亮。
    pub fn clear_search(&self) {
        let mut state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        state.matches.clear();
        state.current = None;
    }

    /// 全部命中（视口坐标）。
    pub fn search_matches(&self) -> Vec<Range> {
        let offset = self.scroll_offset() as i32;
        let state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .matches
            .iter()
            .map(|hit| range_of(hit, offset))
            .collect()
    }

    /// 当前跳转到的命中下标；还没开始跳转时为 `None`。
    pub fn search_current_index(&self) -> Option<usize> {
        self.search_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
    }

    /// 跳到下一处命中（环绕），并把它滚进视口。
    pub fn search_next(&self) {
        self.search_step(1);
    }

    /// 跳到上一处命中（环绕），并把它滚进视口。
    pub fn search_prev(&self) {
        self.search_step(-1);
    }

    /// 移动当前下标并按方向环绕，再把它滚进视口。
    fn search_step(&self, direction: isize) {
        let Some(index) = self.advance_current(direction) else {
            return;
        };
        self.scroll_to(index);
    }

    /// 移动当前命中下标并按方向环绕，返回新下标。
    fn advance_current(&self, direction: isize) -> Option<usize> {
        let mut state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        let len = state.matches.len();
        if len == 0 {
            state.current = None;
            return None;
        }
        let next = match state.current {
            None if direction > 0 => 0,
            None => len - 1,
            // rem_euclid 处理负数：prev 从 0 出发要绕到 len-1。
            Some(index) => (index as isize + direction).rem_euclid(len as isize) as usize,
        };
        state.current = Some(next);
        Some(next)
    }

    /// 把第 `index` 个命中滚进视口。
    fn scroll_to(&self, index: usize) {
        // 先取行号、放掉搜索锁，再进 `scroll_to_line` 拿 term 锁 —— 全程不同时
        // 持两把锁，避免与其它路径形成加锁顺序环。
        let line = {
            let state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
            match state.matches.get(index) {
                Some(hit) => hit.start().line.0,
                None => return,
            }
        };
        self.scroll_to_line(line);
    }

    /// 让某个 alacritty 绝对行滚进视口。
    ///
    /// 换算：视口行号 = `line + offset`。`line` 越小越靠上，所以
    /// - 落在视口上方（行号 < 0）→ 抬到视口顶：`offset = -line`
    /// - 落在视口下方（行号 ≥ 屏高）→ 压到视口底：`offset = 屏高 - 1 - line`
    ///
    /// 最后夹到 `[0, history_size]`；已经在视口内、或夹完仍在边界（内容还没
    /// 生成）时不动。
    fn scroll_to_line(&self, line: i32) {
        let mut term = self.term.lock();
        // 先把需要的数据拷出来，再改 offset —— 借用 `grid()` 期间不能改。
        let (screen, history, offset) = {
            let grid = term.grid();
            (
                grid.screen_lines() as i32,
                grid.history_size() as i32,
                grid.display_offset() as i32,
            )
        };
        let row = line + offset;
        let desired = if row < 0 {
            -line
        } else if row >= screen {
            screen - 1 - line
        } else {
            // 已经在视口里，不必动。
            return;
        }
        .clamp(0, history);
        if desired != offset {
            term.scroll_display(Scroll::Delta(desired - offset));
        }
    }

    /// 扫出全部命中（alacritty 绝对坐标）。
    ///
    /// 非法正则**不 panic**：用户敲键盘的每一步都可能构成一个临时非法的式子
    /// （例如刚输完 `[`），此时按「没有命中」处理。
    fn scan(&self, query: &str) -> Vec<Match> {
        if query.is_empty() {
            return Vec::new();
        }
        let Ok(mut regex) = RegexSearch::new(query) else {
            return Vec::new();
        };

        let term = self.term.lock();
        let start = AlacPoint::new(term.grid().topmost_line(), Column(0));
        let end = AlacPoint::new(term.grid().bottommost_line(), term.grid().last_column());
        // RegexIter 要 &mut（内部推进游标）；Item 就是 `Match`。
        RegexIter::new(start, end, Direction::Right, &term, &mut regex).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::tests::palette;
    use crate::grid::GridGeometry;
    use crate::selection::HighlightKind;
    use crate::session::window_size_from;

    /// 24 行 × 80 列的终端（800/10 × 480/20）。
    fn terminal() -> Terminal {
        let size = window_size_from(&GridGeometry::new(800., 480., 10., 20.));
        Terminal::for_test(size, palette())
    }

    #[test]
    fn a_fresh_search_has_no_current_until_the_first_jump() {
        let mut t = terminal();
        t.feed(b"abc abc abc");
        assert_eq!(t.set_search("abc"), 3);
        // 关键：第一次跳转要落在**第一处**，而不是「第一处已当前、于是跳过它」。
        assert_eq!(t.search_current_index(), None);
    }

    #[test]
    fn next_walks_forward_and_wraps_around() {
        let mut t = terminal();
        t.feed(b"abc xx abc xx abc");
        t.set_search("abc");

        for expected in [0, 1, 2, 0] {
            t.search_next();
            assert_eq!(t.search_current_index(), Some(expected));
        }
    }

    #[test]
    fn prev_from_a_fresh_search_lands_on_the_last_match() {
        let mut t = terminal();
        t.feed(b"abc xx abc xx abc");
        t.set_search("abc");

        t.search_prev();
        assert_eq!(t.search_current_index(), Some(2), "prev 应绕到最后一处");
        t.search_prev();
        assert_eq!(t.search_current_index(), Some(1));
        t.search_prev();
        assert_eq!(t.search_current_index(), Some(0));
        t.search_prev();
        assert_eq!(t.search_current_index(), Some(2), "再 prev 应从 0 绕回末尾");
    }

    #[test]
    fn navigation_without_matches_stays_at_none() {
        let mut t = terminal();
        t.feed(b"abc");
        assert_eq!(t.set_search("zzz"), 0);

        t.search_next();
        assert_eq!(t.search_current_index(), None);
        t.search_prev();
        assert_eq!(t.search_current_index(), None);
    }

    #[test]
    fn jumping_scrolls_the_match_into_view() {
        let mut t = terminal();
        // 命中放在最上面，随后灌满一屏多的输出把它顶到视口上方。
        t.feed(b"needle\r\n");
        for i in 0..40 {
            t.feed(format!("line-{i:02}\r\n").as_bytes());
        }
        assert_eq!(t.scroll_offset(), 0, "刚写完应停在最新输出处");

        t.set_search("needle");
        // 命中此刻在视口上方。
        assert!(
            t.search_matches()[0].start().line < 0,
            "命中应先在视口上方: {:?}",
            t.search_matches()[0]
        );

        t.search_next();
        assert_eq!(t.search_current_index(), Some(0));
        assert!(t.scroll_offset() > 0, "跳转应把视口向上滚");
        let row = t.search_matches()[0].start().line;
        assert!(
            (0..24).contains(&row),
            "跳转后命中应落在视口内，实际行 {row}"
        );
    }

    #[test]
    fn the_current_match_is_highlighted_apart_from_the_others() {
        let mut t = terminal();
        t.feed(b"abc xx abc");
        t.set_search("abc");

        let before: Vec<_> = t
            .highlight_ranges()
            .into_iter()
            .map(|(_, kind)| kind)
            .collect();
        assert_eq!(
            before,
            vec![HighlightKind::SearchMatch; 2],
            "未跳转前都是普通命中"
        );

        t.search_next();
        let after: Vec<_> = t
            .highlight_ranges()
            .into_iter()
            .map(|(_, kind)| kind)
            .collect();
        assert_eq!(
            after,
            vec![
                HighlightKind::CurrentSearchMatch,
                HighlightKind::SearchMatch
            ],
            "跳转后当前命中应单独着色"
        );
    }
}
