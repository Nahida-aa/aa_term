//! 滚动状态 —— 视口相对回滚区的偏移，以及所有夹取规则。
//!
//! ## 术语
//!
//! `display_offset` 是「从底部往上退了多少行」。它为 0 表示贴着最新输出
//! （底部），等于 `history_size()` 表示翻到了回滚区最顶端。
//!
//! 注意它**不是**网格的 `Point::line`。`Point::line` 以视口顶部为 0 且可为负，
//! `display_offset` 恒为非负，两者的换算是：
//!
//! ```text
//! line = display_offset - 视口内第几行
//! ```
//!
//! ## 为什么要自己维护一份
//!
//! alacritty 的 `Grid::scroll_display` 已经做了全部夹取，理论上直接用即可。
//! 但它把状态关在 `Grid` 内部且要求 `&mut Grid`，而 UI 侧要回答的问题
//! （「滚动条多长、拇指多大、当前在什么位置」）只需要只读视图。这里镜像同一套
//! 规则，使 UI 可以在不触碰网格的前提下计算这些量。
//!
//! 规则逐条抄自 alacritty `grid/mod.rs::scroll_display`，改动会让滚动边界
//! 与上游终端不一致，测试会挡住。

/// 一次滚动请求。变体与 alacritty 的 `grid::Scroll` 一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scroll {
    /// 相对滚动，`count` 为正表示往上（看更早的内容）。
    Delta(i32),
    /// 向上翻一屏。
    PageUp,
    /// 向下翻一屏。
    PageDown,
    /// 跳到回滚区最顶端。
    Top,
    /// 跳回最新输出。
    Bottom,
}

/// 滚动状态机。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollState {
    /// 从底部往回退了多少行。0 = 底部。
    display_offset: usize,
    /// 回滚区总行数（不含可见屏幕）。滚动上限。
    history_size: usize,
    /// 可见屏幕行数。PageUp/PageDown 一次滚这么多。
    screen_lines: usize,
}

impl ScrollState {
    /// 构造。`display_offset` 从 0（底部）开始。
    pub fn new(history_size: usize, screen_lines: usize) -> Self {
        Self {
            display_offset: 0,
            history_size,
            screen_lines,
        }
    }

    /// 当前从底部回退了多少行。
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// 回滚区总行数。
    pub fn history_size(&self) -> usize {
        self.history_size
    }

    /// 已翻到回滚区顶端？
    pub fn is_at_top(&self) -> bool {
        self.history_size > 0 && self.display_offset >= self.history_size
    }

    /// 已贴到最新输出？
    pub fn is_at_bottom(&self) -> bool {
        self.display_offset == 0
    }

    /// 回滚区占可滚动总高度的比例，`0.0..=1.0`。
    ///
    /// 用于滚动条整体长度：没有回滚区时返回 0（画不出可拖的部分）。
    pub fn scrollbar_ratio(&self) -> f32 {
        let total = self.history_size + self.screen_lines;
        if total == 0 {
            return 0.;
        }
        self.history_size as f32 / total as f32
    }

    /// 滚动条拇指的起点比例（`0.0..=1.0`，0 = 顶部）。
    ///
    /// 视口在「回滚区 + 屏幕」这条总时间线上的位置。`offset == 0`（底部）
    /// 时拇指应在最下端，所以是 `1.0 - 拇指长度`。
    pub fn thumb_position(&self) -> f32 {
        let ratio = self.scrollbar_ratio();
        if ratio <= 0. {
            return 0.;
        }
        if self.history_size == 0 {
            return 0.;
        }
        // offset=0 表示「最新」，对应拇指在最下端；offset 越大越往回翻，
        // 拇指越往上。所以是 (1 - 进度) 而不是进度。
        // 拇指长度 = ratio，可移动距离 = 1 - ratio。
        let progress = self.display_offset as f32 / self.history_size as f32;
        ((1.0 - progress) * (1.0 - ratio)).clamp(0., 1.)
    }

    /// 应用一次滚动，返回 `display_offset` 是否真的变了。
    ///
    /// 夹取规则逐条对齐 alacritty：
    /// - `Delta`：先 `max(.., 0)` 再 `min(.., history_size)`，
    ///   即向下滚过底部会停在 0 而不是下溢。
    /// - `PageUp`：`min(offset + lines, history_size)`
    /// - `PageDown`：`saturating_sub(lines)`
    /// - `Top`：直接跳到 `history_size`；`Bottom`：归 0。
    pub fn scroll(&mut self, request: Scroll) -> bool {
        let before = self.display_offset;
        self.display_offset = match request {
            Scroll::Delta(count) => {
                // 注意：`as i32` 之后 `max(.., 0)` 再 `as usize`，顺序与
                // alacritty 一致。反过来先转 usize 会让负数变成巨大正数。
                let shifted = (before as i32).saturating_add(count).max(0);
                (shifted as usize).min(self.history_size)
            }
            Scroll::PageUp => before
                .saturating_add(self.screen_lines)
                .min(self.history_size),
            Scroll::PageDown => before.saturating_sub(self.screen_lines),
            Scroll::Top => self.history_size,
            Scroll::Bottom => 0,
        };
        self.display_offset != before
    }

    /// 回滚区增长（PTY 输出了新行）后收缩偏移量。
    ///
    /// `decrease_scroll_limit` 的语义：上限变小则把偏移夹回新的上限。返回
    /// 是否变了 —— 变了就意味着用户当前看的不是最新输出了。
    pub fn clamp_to(&mut self, history_size: usize) -> bool {
        let before = self.display_offset;
        self.history_size = history_size;
        self.display_offset = self.display_offset.min(history_size);
        self.display_offset != before
    }

    /// 视口内第 `row` 行对应的绝对 `Point::line`。
    ///
    /// 这是 `display_offset` 与 [`crate::grid::Point`] 之间唯一的换算口。
    pub fn line_of_row(&self, row: i32) -> i32 {
        self.display_offset as i32 - row
    }

    /// 绝对 `line` 落在视口内的第几行；视口外返回 `None`。
    pub fn row_of_line(&self, line: i32) -> Option<i32> {
        let row = self.display_offset as i32 - line;
        (0..self.screen_lines as i32).contains(&row).then_some(row)
    }

    /// 让某个绝对 `line` 滚进视口，必要时调整偏移。
    ///
    /// ## 可达的 line 范围
    ///
    /// `offset=0`（底部）时视口覆盖 `line 0 ..= -(n-1)`，所以合法的 `line`
    /// 区间是 `[-(n-1), history_size]`。超出这个范围的 line（尤其是小于
    /// `-(n-1)` 的）指向尚不存在的「更下方」内容，**再怎么滚也进不了视口** ——
    /// 此时 offset 会被夹到边界，返回值表示「有没有因此改动过」。
    pub fn scroll_into_view(&mut self, line: i32) -> bool {
        if self.row_of_line(line).is_some() {
            return false;
        }
        let n = self.screen_lines as i32;
        // 视口覆盖的 line 是 [offset, offset - (n-1)]（屏幕上往下走 line 减小）。
        let desired = if line > self.display_offset as i32 {
            // 目标在视口上方（历史更早处）→ 把 offset 抬到 target 行本身
            line
        } else {
            // 目标在视口下方 → 把 offset 压下来，让 target 落在视口底行
            line.saturating_add(n - 1)
        };
        let before = self.display_offset;
        self.display_offset = (desired.max(0) as usize).min(self.history_size);
        self.display_offset != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100 行回滚区 + 24 行屏幕。
    fn state() -> ScrollState {
        ScrollState::new(100, 24)
    }

    #[test]
    fn starts_at_the_bottom() {
        let s = state();
        assert_eq!(s.display_offset(), 0);
        assert!(s.is_at_bottom());
        assert!(!s.is_at_top());
    }

    #[test]
    fn delta_scrolls_up_and_down() {
        let mut s = state();
        assert!(s.scroll(Scroll::Delta(10)));
        assert_eq!(s.display_offset(), 10);
        assert!(s.scroll(Scroll::Delta(-4)));
        assert_eq!(s.display_offset(), 6);
    }

    #[test]
    fn delta_clamps_at_both_ends() {
        let mut s = state();
        // 往下滚过头停在 0，不能下溢成巨大值
        assert!(!s.scroll(Scroll::Delta(-5)));
        assert_eq!(s.display_offset(), 0);

        // 往上滚过头停在 history_size
        assert!(s.scroll(Scroll::Delta(9999)));
        assert_eq!(s.display_offset(), 100);
        assert!(s.is_at_top());
    }

    #[test]
    fn scrolling_beyond_a_limit_reports_no_change() {
        // 调用方靠这个返回值决定要不要重绘/发事件。
        let mut s = state();
        assert!(!s.scroll(Scroll::Bottom), "已在底部时 Bottom 不算变化");
        assert!(s.scroll(Scroll::Delta(1)));
        assert!(!s.scroll(Scroll::Delta(0)), "原地不动不算变化");
    }

    #[test]
    fn page_scroll_moves_a_full_screen() {
        let mut s = state();
        assert!(s.scroll(Scroll::PageUp));
        assert_eq!(s.display_offset(), 24);
        assert!(s.scroll(Scroll::PageUp));
        assert_eq!(s.display_offset(), 48);

        assert!(s.scroll(Scroll::PageDown));
        assert_eq!(s.display_offset(), 24);
        assert!(s.scroll(Scroll::PageDown));
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn page_scroll_saturates_instead_of_underflowing() {
        let mut s = state();
        // 已经在底部还要翻页 → 停在 0
        assert!(!s.scroll(Scroll::PageDown));
        assert_eq!(s.display_offset(), 0);

        // 接近顶端时翻页 → 夹到 history_size
        s.scroll(Scroll::Delta(95));
        assert!(s.scroll(Scroll::PageUp));
        assert_eq!(s.display_offset(), 100);
    }

    #[test]
    fn top_and_bottom_jump() {
        let mut s = state();
        assert!(s.scroll(Scroll::Top));
        assert_eq!(s.display_offset(), 100);
        assert!(s.is_at_top());
        assert!(s.scroll(Scroll::Bottom));
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn no_history_means_nowhere_to_scroll() {
        let mut s = ScrollState::new(0, 24);
        assert!(!s.scroll(Scroll::PageUp));
        assert_eq!(s.display_offset(), 0);
        // Top 在没有回滚区时也应该是 0，而不是别的
        assert!(!s.scroll(Scroll::Top));
        assert_eq!(s.display_offset(), 0);
        // 此时「在顶端」和「在底部」是同一件事，别让 is_at_top 误报
        assert!(!s.is_at_top());
        assert!(s.is_at_bottom());
    }

    #[test]
    fn shrink_clamps_an_offset_that_would_dangle() {
        let mut s = state();
        s.scroll(Scroll::Delta(90));
        assert_eq!(s.display_offset(), 90);
        // 上限缩到 50：偏移被夹回去
        assert!(s.clamp_to(50), "偏移应被夹回上限");
        assert_eq!(s.display_offset(), 50);
        assert_eq!(s.history_size(), 50);

        // 上限没变则不报变化
        assert!(!s.clamp_to(50));
        // 上限变大不影响当前偏移
        assert!(!s.clamp_to(500));
        assert_eq!(s.display_offset(), 50);
    }

    #[test]
    fn row_and_line_are_inverse_over_the_viewport() {
        let mut s = state();
        s.scroll(Scroll::Delta(30));
        for row in 0..24 {
            let line = s.line_of_row(row);
            assert_eq!(s.row_of_line(line), Some(row), "row={row} 往返失败");
        }
    }

    #[test]
    fn lines_outside_the_viewport_have_no_row() {
        let s = state();
        // line 是「屏幕上往下走就减小」的（alacritty 里 Line(0) 是屏幕最底行，
        // 由 `line = row - display_offset` 得出）。所以 offset=0 时视口覆盖
        // line 0 ..= -23，不是 0..=23。
        assert_eq!(s.row_of_line(0), Some(0));
        assert_eq!(s.row_of_line(-23), Some(23));
        assert_eq!(s.row_of_line(1), None, "line 1 在视口上方");
        assert_eq!(s.row_of_line(-24), None, "line -24 在视口下方");

        // 往上滚 30 行后视口覆盖 line 30 ..= 7
        let mut s = state();
        s.scroll(Scroll::Delta(30));
        assert_eq!(s.row_of_line(30), Some(0));
        assert_eq!(s.row_of_line(7), Some(23));
        assert_eq!(s.row_of_line(31), None);
        assert_eq!(s.row_of_line(6), None);
    }

    #[test]
    fn line_of_row_matches_alacritty_convention() {
        // alacritty 里滚动后 topmost_line = -display_offset，
        // 视口首行的 line 就是 -offset。用它交叉验证符号约定。
        let mut s = state();
        s.scroll(Scroll::Delta(30));
        assert_eq!(s.line_of_row(0), 30, "往上滚 30 行后视口首行应是 line 30");
        assert_eq!(
            s.line_of_row(23),
            7,
            "视口末行应是 line 7（屏幕上往下 line 减小）"
        );
        // 底部时视口首行 line 0、末行 -23
        let bottom = state();
        assert_eq!(bottom.line_of_row(0), 0);
        assert_eq!(bottom.line_of_row(23), -23);
    }

    #[test]
    fn scroll_into_view_is_a_no_op_when_already_visible() {
        let mut s = state();
        // 底部时视口覆盖 line 0..=-23，line -10 在其中
        assert!(!s.scroll_into_view(-10));
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn scroll_into_view_reaches_a_line_above_the_viewport() {
        let mut s = state();
        // line 90 在视口上方（需要更早的历史）
        assert!(s.scroll_into_view(90));
        assert!(s.row_of_line(90).is_some(), "目标应进入视口");
        assert_eq!(s.display_offset(), 90);
    }

    #[test]
    fn scroll_into_view_reaches_a_line_below_the_viewport() {
        let mut s = state();
        s.scroll(Scroll::Delta(50)); // offset=50，视口覆盖 50..=27
        assert_eq!(s.row_of_line(5), None, "line 5 此时在视口下方");
        // 把目标放到视口底行
        assert!(s.scroll_into_view(5));
        assert_eq!(s.row_of_line(5), Some(23), "目标应落在视口底行");
        assert_eq!(s.display_offset(), 28);
    }

    #[test]
    fn scroll_into_view_cannot_reach_lines_that_do_not_exist_yet() {
        let mut s = state();
        // 底部时视口最低只到 -(n-1) = -23，更小的 line 是「更下方的未来」，
        // 滚任何位置都进不了视口。offset 应被夹到 0 而不是算出负数。
        assert!(!s.scroll_into_view(-50));
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn scroll_into_view_is_clamped_into_the_history() {
        let mut s = state();
        // line 500 超出 100 行的回滚区，夹到顶
        s.scroll_into_view(500);
        assert!(s.display_offset() <= s.history_size());
        // 反向：远超下界的负 line 夹到 0
        s.scroll_into_view(-9999);
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn scrollbar_ratio_is_history_over_total() {
        let s = ScrollState::new(100, 24);
        // 100 / 124
        assert!((s.scrollbar_ratio() - 100. / 124.).abs() < 1e-6);

        let none = ScrollState::new(0, 24);
        assert_eq!(none.scrollbar_ratio(), 0.);

        let empty = ScrollState::new(0, 0);
        assert_eq!(empty.scrollbar_ratio(), 0., "空视口不能除零");
    }

    #[test]
    fn thumb_sits_at_the_bottom_when_at_bottom() {
        let s = ScrollState::new(100, 24);
        let ratio = s.scrollbar_ratio();
        assert!(
            (s.thumb_position() - (1.0 - ratio)).abs() < 1e-6,
            "底部时拇指应在最下端"
        );
    }

    #[test]
    fn thumb_sits_at_the_top_when_at_top() {
        let mut s = ScrollState::new(100, 24);
        s.scroll(Scroll::Top);
        assert_eq!(s.thumb_position(), 0.0);
    }

    #[test]
    fn thumb_position_stays_in_range() {
        let mut s = ScrollState::new(100, 24);
        for _ in 0..200 {
            s.scroll(Scroll::Delta(3));
            assert!(s.thumb_position() >= 0.0 && s.thumb_position() <= 1.0);
            s.scroll(Scroll::Delta(-5));
            assert!(s.thumb_position() >= 0.0 && s.thumb_position() <= 1.0);
        }
    }

    #[test]
    fn delta_with_extreme_values_cannot_panic() {
        let mut s = state();
        s.scroll(Scroll::Delta(i32::MAX));
        assert_eq!(s.display_offset(), 100);
        s.scroll(Scroll::Delta(i32::MIN));
        assert_eq!(s.display_offset(), 0);
    }
}
