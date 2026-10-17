//! 滚动几何 —— 滚动条要的「视口在回滚区里的位置与长度」。
//!
//! ## 术语
//!
//! `display_offset` 是「从底部往上退了多少行」：0 = 贴着最新输出（底部），
//! 等于 `history_size` 就是翻到了回滚区最顶端。
//!
//! ## 为什么是只读的
//!
//! 权威状态在 alacritty 的 `Grid` 里（`display_offset()` / `history_size()`），
//! 这里只是**每帧读出来**算滚动条几何。别在这儿留一份可变的镜像：PTY 又吐了
//! 行、用户翻了页，两边一旦不同步，滚动条就指到错的位置。
//!
//! ## 与 `Point::line` 无关
//!
//! 这里全是行数计数，不涉及 `Point::line` 的符号约定；行列换算在 `grid` 与
//! `search` 里各自完成。

/// 一次本地滚动的量。
///
/// 对应 alacritty 的 `Scroll`：翻一屏 / 回顶 / 到底由它按屏高与回滚区大小自己
/// 算，我们只转手。不直接暴露 alacritty 的类型，是为了让 app / iced 层不必依赖
/// alacritty —— 那两个 `Cargo.toml` 里确实没有这个依赖。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollAmount {
    /// 按行滚。正数 = 看更早的内容。
    Lines(i32),
    /// 往上翻一屏。
    PageUp,
    /// 往下翻一屏。
    PageDown,
    /// 翻到回滚区最顶端。
    Top,
    /// 回到最新输出。
    Bottom,
}

/// 一帧的滚动几何。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollState {
    /// 从底部往回退了多少行。0 = 底部。
    display_offset: usize,
    /// 回滚区总行数（不含可见屏幕）。
    history_size: usize,
    /// 可见屏幕行数。
    screen_lines: usize,
}

impl ScrollState {
    /// 由权威状态构造。
    ///
    /// `display_offset` 会被夹进 `history_size`：调用方拿到的两个值可能来自
    /// 不同时刻（PTY 恰好在中间吐了行），不夹会让拇指起点越出轨道。
    pub fn new(display_offset: usize, history_size: usize, screen_lines: usize) -> Self {
        Self {
            display_offset: display_offset.min(history_size),
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

    /// 可见屏幕行数。
    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    /// 已翻到回滚区顶端？
    pub fn is_at_top(&self) -> bool {
        self.history_size > 0 && self.display_offset >= self.history_size
    }

    /// 已贴到最新输出？
    pub fn is_at_bottom(&self) -> bool {
        self.display_offset == 0
    }

    /// 拇指长度比例 = 可见行数 / 全部行数（回滚区 + 屏幕）。
    ///
    /// 拇指代表**视口**，轨道代表全部内容 —— 所以长度是屏幕占比，不是回滚区
    /// 占比。搞反的话拇指会画得跟内容一样长。
    ///
    /// 没有内容时返回 0。
    pub fn thumb_length(&self) -> f32 {
        let total = self.history_size + self.screen_lines;
        if total == 0 {
            return 0.;
        }
        self.screen_lines as f32 / total as f32
    }

    /// 拇指起点比例（`0.0..=1.0`，0 = 轨道顶端，1 = 轨道底端）。
    ///
    /// `display_offset == 0` 是「最新输出」，视口在最底下 → 拇指贴轨道底端，
    /// 起点是 `1 - 拇指长`（正好让拇指末端落在 1.0）。翻到顶端则起点为 0。
    pub fn thumb_position(&self) -> f32 {
        if self.history_size == 0 {
            return 0.;
        }
        // 0 = 底部（最新），1 = 顶部（最早）
        let progress = self.display_offset as f32 / self.history_size as f32;
        let travel = 1.0 - self.thumb_length();
        ((1.0 - progress) * travel).clamp(0., 1.)
    }

    /// 拇指几何 `(起点, 长度)`，都是 `0.0..=1.0` 的比例。
    pub fn thumb(&self) -> (f32, f32) {
        (self.thumb_position(), self.thumb_length())
    }

    /// 把拇指顶端在**轨道**上的比例反解成滚动偏移 —— [`Self::thumb_position`]
    /// 的逆。
    ///
    /// 拖动/点击滚动条时用：先把光标位置换算成「希望拇指顶端落在这个比例」，
    /// 再解出该滚到哪一行。拇指长度已经吃掉了部分行程，所以要先除以可移动距离。
    pub fn offset_for_thumb_position(&self, position: f32) -> usize {
        let travel = 1.0 - self.thumb_length();
        if travel <= 0. || self.history_size == 0 {
            return 0;
        }
        // 0 = 顶端，1 = 底端
        let from_top = (position / travel).clamp(0., 1.);
        let progress = 1.0 - from_top;
        (progress * self.history_size as f32).round() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100 行回滚区 + 24 行屏幕：内容 124 行，拇指长 24/124。
    fn state() -> ScrollState {
        ScrollState::new(0, 100, 24)
    }

    const TOTAL: f32 = 124.;

    #[test]
    fn starts_at_the_bottom() {
        let s = state();
        assert_eq!(s.display_offset(), 0);
        assert!(s.is_at_bottom());
        assert!(!s.is_at_top());
    }

    #[test]
    fn constructor_clamps_a_stale_offset() {
        // 两个值来自不同时刻时，偏移可能已经超过新的回滚上限
        assert_eq!(ScrollState::new(500, 100, 24).display_offset(), 100);
        assert_eq!(ScrollState::new(30, 100, 24).display_offset(), 30);
    }

    #[test]
    fn thumb_length_is_the_visible_fraction() {
        // 视口 24 / 内容 124
        assert!((state().thumb_length() - 24. / TOTAL).abs() < 1e-6);
        // 没有回滚区时，视口就是全部 → 拇指占满
        assert!((ScrollState::new(0, 0, 24).thumb_length() - 1.0).abs() < 1e-6);
        // 空视口不能除零
        assert_eq!(ScrollState::new(0, 0, 0).thumb_length(), 0.);
    }

    #[test]
    fn thumb_sits_at_the_bottom_when_at_bottom() {
        let s = state();
        // 拇指贴底 → 起点 = 1 - 拇指长，末端正好落在 1.0
        let (start, length) = s.thumb();
        assert!(
            (start - (1.0 - length)).abs() < 1e-6,
            "底部时拇指应贴轨道底端"
        );
        assert!((start + length - 1.0).abs() < 1e-6);
    }

    #[test]
    fn thumb_sits_at_the_top_when_at_top() {
        let s = ScrollState::new(100, 100, 24);
        assert_eq!(s.thumb_position(), 0.0);
        assert!(s.is_at_top());
    }

    #[test]
    fn thumb_position_is_monotonic_and_in_range() {
        let mut previous = f32::INFINITY;
        for offset in 0..=100 {
            let s = ScrollState::new(offset, 100, 24);
            let start = s.thumb_position();
            assert!((0.0..=1.0).contains(&start), "offset={offset} 起点越界");
            assert!(start <= previous + 1e-6, "越往上滚起点应越小");
            previous = start;
        }
    }

    #[test]
    fn thumb_never_overshoots_the_track() {
        for offset in 0..=100 {
            let (start, length) = ScrollState::new(offset, 100, 24).thumb();
            assert!(start + length <= 1.0 + 1e-6, "offset={offset} 拇指越出轨道");
        }
    }

    #[test]
    fn thumb_start_round_trips_through_the_offset() {
        // 起点 → 偏移 → 起点 应当稳定（取整误差在一个行距内）
        for offset in 0..=100 {
            let s = ScrollState::new(offset, 100, 24);
            let start = s.thumb_position();
            let back = s.offset_for_thumb_position(start);
            assert!(
                back.abs_diff(offset) <= 1,
                "offset={offset} 往返得到 {back}，差得太多"
            );
        }
    }

    #[test]
    fn dragging_to_the_extremes_hits_the_ends() {
        let s = state();
        assert_eq!(s.offset_for_thumb_position(0.0), 100, "拖到顶端 = 翻到最早");
        let travel = 1.0 - s.thumb_length();
        assert_eq!(
            s.offset_for_thumb_position(travel),
            0,
            "拖到底端 = 回到最新"
        );
        // 超出轨道也要夹住
        assert_eq!(s.offset_for_thumb_position(5.0), 0);
        assert_eq!(s.offset_for_thumb_position(-5.0), 100);
    }

    #[test]
    fn no_history_means_nothing_to_drag() {
        let s = ScrollState::new(0, 0, 24);
        assert_eq!(s.thumb_position(), 0.);
        assert_eq!(s.offset_for_thumb_position(0.5), 0);
        assert!(!s.is_at_top(), "没有回滚区时「在顶端」不该误报");
        assert!(s.is_at_bottom());
    }
}
