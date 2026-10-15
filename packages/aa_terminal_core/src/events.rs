//! 终端向后端产生的**事件类型**。
//!
//! 对应 Zed 的 `terminal/src/terminal.rs` 里那一小簇事件定义。与
//! [`crate::alacritty`] 分开：那边是「怎么和 alacritty 的库对接」的胶水，
//! 这边只是数据类型。上层（UI、会话）只依赖这里的类型，就不必知道 alacritty
//! 的 `EventListener` 长什么样。

/// PTY 侧产生的事件，附带它属于第几代终端。
///
/// ## 为什么事件和代数塞进同一个 enum
///
/// Zed 就是这么做的。相比之下，另造一个 `GenerationEvent` struct 承载相同的
/// 信息会差一些：加新事件种类时要改字段而不是加变体，而且匹配时不会穷尽 ——
/// 编译器没法提醒你漏了哪个分支。
#[derive(Debug)]
pub enum PtyEvent {
    /// 一个 alacritty 事件 + 生成它的终端代号。
    ///
    /// 代号用于丢弃「已被替换的旧终端」发来的残留事件；见
    /// [`crate::alacritty::ChannelListener`] 的 `generation` 字段。
    Event(alacritty_terminal::event::Event, u64),
}
