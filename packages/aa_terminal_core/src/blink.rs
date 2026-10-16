//! 光标闪烁状态机。
//!
//! ## 为什么在 core 里（Zed / AAgent 不在终端层）
//!
//! Zed/AAgent 的 `BlinkManager`（`editor/src/blink_manager.rs`）把状态和 gpui 的
//! 定时器、通知绑在一起：状态一变就 `cx.spawn` 一个 timer，timer 回调再改状态。
//! 这种写法有个真实存在的竞态 —— 用户快速连续输入会排多个 timer，早排的那个
//! 可能后触发，把光标错误地切回闪烁态。Zed/AAgent 用**epoch 计数**解决：每次
//! 操作 `blink_epoch += 1`，回调携带自己那次的 epoch，不等于当前 epoch 就丢弃。
//!
//! 那个 epoch 机制是纯逻辑，值得留下；`Context`/timer/`notify` 是 iced 侧的事。
//! 我们没有 editor 层，而它又不依赖任何 UI 框架，所以放在 core：本模块只提供
//! 状态机并返回「要不要排下一次」，由调用方去驱动。
//!
//! 光标本身（位置 + 形状）不在这个模块，见
//! [`crate::grid::Cursor`]。

/// 一次闪烁操作的代号。
///
/// `BlinkState` 内部维护一个自增计数；调用方在排定时器时记下当前的
/// [`BlinkEpoch`]，回调时带回，用来丢弃过期的定时器。
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct BlinkEpoch(u64);

impl BlinkEpoch {
    /// 取回单调递增的计数。
    pub fn get(self) -> u64 {
        self.0
    }
}

/// 光标闪烁状态机。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlinkState {
    /// 光标当前是否可见（"on" 阶段）。
    visible: bool,
    /// 是否已启用闪烁。
    enabled: bool,
    /// 是否处于暂停（暂停期间不闪，但保持可见）。
    paused: bool,
    /// 过期定时器的判据。
    epoch: u64,
}

/// 暂停闪烁后多久恢复闪烁。
pub const RESUME_AFTER_PAUSING: std::time::Duration = std::time::Duration::from_millis(500);

impl Default for BlinkState {
    fn default() -> Self {
        Self {
            // 初始可见：与 Zed/AAgent 的 BlinkManager 一致。
            visible: true,
            enabled: false,
            paused: false,
            epoch: 0,
        }
    }
}

impl BlinkState {
    /// 新建：光标可见、闪烁未启用。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前是否处于可见相位。
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 闪烁是否已启用。
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 是否处于暂停（收到输入后、恢复前）。
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 当前 epoch，用于给定时器打标记。
    pub fn epoch(&self) -> BlinkEpoch {
        BlinkEpoch(self.epoch)
    }

    fn bump(&mut self) -> BlinkEpoch {
        self.epoch = self.epoch.wrapping_add(1);
        BlinkEpoch(self.epoch)
    }

    /// 开启/关闭闪烁（通常跟随设置项变化）。
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            // 关掉闪烁时光标必须稳定可见，否则会停在一个看不见的相位上。
            self.visible = true;
        }
    }

    /// 强制显示光标。
    pub fn show(&mut self) -> bool {
        if self.visible {
            return false;
        }
        self.visible = true;
        true
    }

    /// 暂停闪烁（收到输入时）。返回需要在 500ms 后回调的 epoch。
    ///
    /// 副作用：立即显示光标并进入暂停态。排定时器的责任在调用方 ——
    /// 本 crate 没有异步运行时。
    pub fn pause(&mut self) -> BlinkEpoch {
        self.show();
        self.paused = true;
        self.bump()
    }

    /// 暂停到期后恢复。`epoch` 是 [`BlinkState::pause`] 的返回值。
    ///
    /// 过期 epoch 直接返回 `None` 且不改任何状态。
    pub fn resume(&mut self, epoch: BlinkEpoch) -> Option<BlinkEpoch> {
        if epoch.0 != self.epoch {
            return None;
        }
        self.paused = false;
        self.tick(epoch)
    }

    /// 走一个闪烁节拍。
    ///
    /// `epoch` 不匹配说明这是个过期定时器，直接忽略。有效时翻转可见性，
    /// 并返回「要不要再排一次定时器」的 epoch —— 返回 `None` 表示已停。
    pub fn tick(&mut self, epoch: BlinkEpoch) -> Option<BlinkEpoch> {
        if epoch.0 != self.epoch {
            return None;
        }
        if !self.enabled || self.paused {
            return None;
        }
        self.visible = !self.visible;
        Some(self.bump())
    }

    /// 处理「终端自己改变了闪烁开关」的通知（DECSET 1004 之类）。
    ///
    /// 与 [`BlinkState::set_enabled`] 的区别：这个来自远端程序，通常也会触发
    /// 设置项变更，两者最终都要落到同一个 `enabled` 上。
    pub fn on_blinking_changed(&mut self, blinking: bool) {
        self.set_enabled(blinking);
        // 设置变了说明之前的相位作废，重置到可见。
        self.bump();
        self.visible = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blinking() -> BlinkState {
        let mut s = BlinkState::new();
        s.set_enabled(true);
        s
    }

    #[test]
    fn starts_visible_and_disabled() {
        let s = BlinkState::new();
        assert!(s.is_visible(), "初始必须可见");
        assert!(!s.is_enabled());
    }

    #[test]
    fn ticking_toggles_visibility() {
        let mut s = blinking();
        let e = s.epoch();
        let next = s.tick(e).expect("应继续排程");
        assert!(!s.is_visible(), "第一次 tick 应熄灭");

        s.tick(next).expect("应继续排程");
        assert!(s.is_visible(), "第二次 tick 应点亮");
    }

    #[test]
    fn disabled_state_does_not_tick() {
        let mut s = BlinkState::new();
        assert!(s.tick(s.epoch()).is_none());
        assert!(s.is_visible(), "未启用时不该变暗");
    }

    #[test]
    fn disabling_blinking_pins_the_cursor_visible() {
        let mut s = blinking();
        s.tick(s.epoch()).unwrap();
        assert!(!s.is_visible());
        // 关掉闪烁：不能停在看不见的相位上
        s.set_enabled(false);
        assert!(s.is_visible());
    }

    #[test]
    fn pause_shows_cursor_and_stops_ticking() {
        let mut s = blinking();
        s.tick(s.epoch()).unwrap(); // 先变暗
        assert!(!s.is_visible());

        let epoch = s.pause();
        assert!(s.is_visible(), "暂停时应立即显示");
        assert!(s.is_paused());
        // 暂停期间 tick 不再翻转
        assert!(s.tick(s.epoch()).is_none());
        assert!(s.is_visible());
        // 恢复用的 epoch 就是 pause 返回的
        assert_eq!(epoch.get(), s.epoch().get());
    }

    #[test]
    fn stale_epoch_is_ignored() {
        // 这是 epoch 机制的全部意义：过期定时器不得改动状态。
        let mut s = blinking();
        let stale = s.pause();
        // 连续两次输入 → 产生更新的 epoch
        let _newer = s.pause();

        let visible_before = s.is_visible();
        let paused_before = s.is_paused();
        assert!(s.resume(stale).is_none(), "过期 epoch 的 resume 应被丢弃");
        assert_eq!(s.is_visible(), visible_before);
        assert_eq!(s.is_paused(), paused_before, "过期回调不得解除暂停");
    }

    #[test]
    fn stale_tick_is_ignored() {
        let mut s = blinking();
        let stale = s.epoch();
        s.pause(); // 产生新 epoch，使 stale 失效
        let before = s.is_visible();
        assert!(s.tick(stale).is_none());
        assert_eq!(s.is_visible(), before, "过期 tick 不得翻转可见性");
    }

    #[test]
    fn resume_clears_pause_and_resumes_ticking() {
        let mut s = blinking();
        let epoch = s.pause();
        let next = s.resume(epoch).expect("有效 epoch 应恢复");
        assert!(!s.is_paused());
        assert!(s.tick(next).is_some(), "恢复后应继续闪");
    }

    #[test]
    fn rapid_pauses_leave_only_the_last_one_live() {
        // 模拟快速连打：pause 若干次，只有最后一个 epoch 能 resume。
        let mut s = blinking();
        let mut epochs = Vec::new();
        for _ in 0..5 {
            epochs.push(s.pause());
        }
        let last = *epochs.last().unwrap();
        for e in &epochs[..4] {
            assert!(s.resume(*e).is_none(), "前 4 个 epoch 都应失效");
        }
        assert!(s.resume(last).is_some());
    }

    #[test]
    fn remote_blinking_change_resets_the_phase() {
        let mut s = blinking();
        s.tick(s.epoch()).unwrap();
        assert!(!s.is_visible());

        // 终端报告自己关掉了闪烁
        s.on_blinking_changed(false);
        assert!(s.is_visible(), "相位应重置为可见");
        assert!(!s.is_enabled());

        // 旧 epoch 随之失效
        assert!(s.tick(BlinkEpoch(0)).is_none());
    }

    #[test]
    fn show_reports_whether_it_changed_anything() {
        let mut s = BlinkState::new();
        assert!(!s.show(), "本来就可见时不该报告变化");
        let mut s = blinking();
        s.tick(s.epoch()).unwrap();
        assert!(s.show());
        assert!(s.is_visible());
    }
}
