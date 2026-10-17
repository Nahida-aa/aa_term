//! 终端模型层 —— 不依赖任何 UI 框架。
#![warn(missing_docs)]
#![warn(
    clippy::correctness,
    clippy::suspicious,
    clippy::style,
    clippy::complexity,
    clippy::perf
)]

//! 终端模型层 —— 不依赖任何 UI 框架。
//!
//! iced 只出现在 `aa_terminal_iced` / `aa_terminal_view_iced`。本 crate 负责
//! 那些「与怎么画无关」的逻辑：颜色语义解析、网格与光标模型、滚动状态。
//! 理由很实际 —— 这部分逻辑占了终端绝大部分的 bug 面，而它一旦依赖 iced
//! 就没法不起窗口做测试。
pub mod alacritty;
pub mod blink;
pub mod buffer;
pub mod events;
pub mod grid;
pub mod mappings;
pub mod palette;
pub mod resolve;
pub mod scroll;
pub mod search;
pub mod selection;
pub mod session;

pub use alacritty_terminal::selection::SelectionType;

pub use alacritty::{ChannelListener, channel};
pub use alacritty_terminal::event::{Event as TerminalEvent, WindowSize};
// 颜色的**符号**类型：解析成具体色值要经 [`resolve_color`]，而解析本身由渲染层
// 用主题调色板做（快照里存的就是这个类型）。与 Zed 的 `pub use vte::ansi::Color`
// 相同 —— 渲染层因此不必依赖 alacritty 本身。
pub use alacritty_terminal::vte::ansi::Color;
pub use blink::{BlinkEpoch, BlinkState, RESUME_AFTER_PAUSING};
pub use buffer::{Session, Terminal, Viewport};
pub use events::PtyEvent;
pub use grid::{
    Cell, Content, Cursor, CursorShape, GridGeometry, IndexedCell, Point, Range, Rounding,
    SelectionRange, SelectionSide,
};
pub use palette::{AnsiColors, TerminalPalette};
pub use resolve::{
    color_at_index, is_app_chosen_exact_color, is_default_background_color, resolve_color,
    resolve_named, rgb,
};
pub use scroll::{ScrollAmount, ScrollState};
pub use session::ShellSpec;

/// 主题层给本 crate 的取色入口。
///
/// 目前是 [`TerminalPalette`] 的直接使用；等 `aa_theme_iced` 建好后，
/// 它负责从主题 JSON / 色阶填充出这份 palette —— 但**填充逻辑不进这里**。
pub trait Palette {
    /// 供 [`resolve_color`] 使用的调色板。
    fn palette(&self) -> &TerminalPalette;
}

impl Palette for TerminalPalette {
    fn palette(&self) -> &TerminalPalette {
        self
    }
}
