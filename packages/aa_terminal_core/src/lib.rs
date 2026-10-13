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

pub mod cursor;
pub mod grid;
pub mod palette;
pub mod resolve;
pub mod scroll;

pub use cursor::{BlinkEpoch, BlinkState, Cursor, CursorShape};
pub use grid::{GridGeometry, Point, Range, Rounding, SelectionRange, SelectionSide};
pub use palette::{AnsiColors, TerminalPalette};
pub use resolve::{color_at_index, resolve_color, resolve_named, rgb};
pub use scroll::{Scroll, ScrollState};

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
