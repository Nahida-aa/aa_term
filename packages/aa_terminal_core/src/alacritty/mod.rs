//! 与 alacritty 库直接打交道的那一层。
//!
//! 对应 Zed 的 `terminal/src/alacritty.rs`：把 alacritty 的类型、事件、PTY
//! 细节都收在这里，外面只看到我们自己的抽象。这样上游升级 rev 时，改动被
//! 圈在这一个模块里，不会溅到配色、网格、映射那些与 alacritty 无关的逻辑上。

mod listener;

pub use listener::{ChannelListener, PtyEvent, channel, channel_with_generation};
