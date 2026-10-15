//! alacritty 事件 → UI 的通道。
//!
//! 对应 Zed 的 `ZedListener` + `PtyEvent`：
//!
//! ```ignore
//! enum PtyEvent {
//!     Event(TerminalBackendEvent, u64),
//! }
//! ```
//!
//! Zed 把事件和代数塞进**同一个 enum 的变体里**，而不是像我（第一版）那样另造
//! 一个 `GenerationEvent` struct。两者承载的信息一样，但 enum 的形状有两个
//! 实际好处：加新事件种类时是加变体而不是改字段；匹配时天然穷尽，编译器会提醒
//! 漏掉的分支。
//!
//! 另外 Zed 往里放的是**自己定义的** `TerminalBackendEvent`，不是 alacritty 的
//! `Event`。这一版还没到需要那层转换的程度（我们没有 ClipboardFormatter、
//! TextAreaSizeFormatter 这些回 adjust 设备），所以先直接透传 alacritty 的
//! `Event`，等真要回 adjust 时再补转换层。

use alacritty_terminal::event::{Event as AlacTermEvent, EventListener};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

/// PTY 侧产生的事件，附带它属于第几代终端。
#[derive(Debug)]
pub enum PtyEvent {
    /// 一个 alacritty 事件 + 生成它的终端代号。
    Event(AlacTermEvent, u64),
}

/// 把 alacritty 的事件转发给 UI。
///
/// ## 为什么走 channel
///
/// 而不能像最初那样攒在 `Arc<Mutex<Vec<_>>>`：后者攒下的新事件没人知道，UI 不会
/// 重绘。channel 收到即消息，可以直接作为 iced `Subscription` 的输入。
///
/// 它在 alacritty 的**子线程**上被调用 —— `unbounded_send` 是同步的，满足要求。
#[derive(Clone)]
pub struct ChannelListener {
    sender: UnboundedSender<PtyEvent>,
    /// 这个 listener 属于哪一代终端。
    ///
    /// 终端被整体替换时（Zed 的 `replace_display_output` 场景：同一个 UI 实体
    /// 换一份全新的 `Term`），旧 event loop 线程可能还活着，还会往同一个
    /// channel 灌事件。收件方按这个戳把「已被取代的那一代」发来的事件丢掉，
    /// 否则旧事件会污染新终端的显示 —— 症状是「重放后偶尔闪过一段旧内容」。
    generation: u64,
}

impl EventListener for ChannelListener {
    fn send_event(&self, event: AlacTermEvent) {
        // 收件人消失（窗口关了）不算错误。
        self.sender
            .unbounded_send(PtyEvent::Event(event, self.generation))
            .ok();
    }
}

/// 新建一对 `(listener, receiver)`。
pub fn channel() -> (ChannelListener, UnboundedReceiver<PtyEvent>) {
    channel_with_generation(0)
}

/// 同上，但显式指定代数。
pub fn channel_with_generation(generation: u64) -> (ChannelListener, UnboundedReceiver<PtyEvent>) {
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    (ChannelListener { sender, generation }, receiver)
}

impl ChannelListener {
    /// 这个 listener 属于第几代终端。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 复用同一个 channel，但改用另一个代号的 listener。
    ///
    /// 对应 Zed 的用法：`events_tx.clone()` 配新的 `generation`。如果每个
    /// generation 都新开一个 channel，UI 就得订阅 N 个流，而它们本该是同一个
    /// 终端实例的事件流。
    pub fn with_generation(&self, generation: u64) -> Self {
        Self {
            sender: self.sender.clone(),
            generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::Event as AlacEvent;
    #[test]
    fn events_reach_the_channel_listener() {
        // 事件不再攒在 Vec 里，而是经 ChannelListener 流入 channel —— 这样
        // UI 才知道该重绘。
        let (listener, mut events) = super::channel();
        assert!(events.try_recv().is_err(), "初始应为空");

        listener.send_event(AlacEvent::Wakeup);
        listener.send_event(AlacEvent::Bell);

        let first = events.try_recv().expect("应收到第一个事件");
        assert!(
            matches!(first, PtyEvent::Event(AlacEvent::Wakeup, 0)),
            "{first:?}"
        );
        let second = events.try_recv().expect("应收到第二个事件");
        assert!(
            matches!(second, PtyEvent::Event(AlacEvent::Bell, 0)),
            "{second:?}"
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn events_are_stamped_with_their_generation() {
        // Zed 靠这个戳丢弃「已被替换的旧终端」发来的事件。没有它，重放/换会话
        // 时旧 event loop 的残留事件会污染新终端的显示。
        let (base, mut events) = super::channel();
        // 同一个 channel 上的两个 listener，代号不同。
        let old = base.with_generation(1);
        let live = base.with_generation(2);

        old.send_event(AlacEvent::Wakeup);
        live.send_event(AlacEvent::Bell);

        // enum 变体里带代号，匹配时能一并取出来。
        match events.try_recv().expect("应收到一代 1 的事件") {
            PtyEvent::Event(AlacEvent::Wakeup, generation) => {
                assert_eq!(generation, 1, "代号应随 listener 走")
            }
            other => panic!("意外事件: {other:?}"),
        }
        match events.try_recv().expect("应收到一代 2 的事件") {
            PtyEvent::Event(_, generation) => assert_eq!(generation, 2),
        }
    }

    #[test]
    fn listener_survives_the_receiver_going_away() {
        // 窗口关闭后 channel 没人收了，listener 继续 send 不该 panic。
        let (listener, events) = super::channel();
        drop(events);
        listener.send_event(AlacEvent::Wakeup);
    }
}
