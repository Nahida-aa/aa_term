//! 终端调色板 —— 把「颜色语义」与「颜色取值」分开。
//!
//! 本类型只有字段、没有默认值。这是刻意的：终端层回答的是「某条 ANSI 颜色该
//! 显示成什么颜色」，而不是「默认该是什么颜色」。后者属于主题层。
//!
//! 现状（AaAgent 侧）的默认值是这么来的：
//!
//! ```text
//! terminal_background: neutral().light().step_1()
//! terminal_ansi_red:   red().light().step_11()
//! ```
//!
//! 即由 `default_color_scales` 那套色阶推导出来的，牵扯几百行定义。把它抄进
//! 终端层会造成两份色阶、日后必然漂移。因此 `TerminalPalette` 在本 crate 里
//! 是个纯数据结构，等 iced 侧的主题 crate（`aa_theme_iced`）建好后，由它从
//! 主题 JSON / 色阶填充。

use aa_theme_core::Hsla;

/// 8 种 ANSI 基础色及其亮/暗变体。
///
/// 按数组下标对应 `NamedColor::{Black..White}` 与 `{BrightBlack..BrightWhite}`，
/// 使解析代码退化成一次索引，避免 28 个 `match` 分支。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnsiColors {
    /// Black, Red, Green, Yellow, Blue, Magenta, Cyan, White（index 0..8）
    pub normal: [Hsla; 8],
    /// BrightBlack..BrightWhite（index 0..8）
    pub bright: [Hsla; 8],
    /// DimBlack..DimWhite（index 0..8）
    pub dim: [Hsla; 8],
}

impl AnsiColors {
    /// 取 `NamedColor`（Black=0 .. BrightWhite=15）在 16 色表里的下标。
    ///
    /// `NamedColor` 的定义刻意把 `Black = 0` / `Foreground = 256` 排在一起，
    /// 因此 0..16 连续、256 之后是前景/背景等非 ANSI 语义 —— 越界的一律返回
    /// `None`，由调用方回退到 foreground/background，而不是猜。
    pub fn index_of(named: vte::ansi::NamedColor) -> Option<usize> {
        use vte::ansi::NamedColor as N;
        Some(match named {
            N::Black => 0,
            N::Red => 1,
            N::Green => 2,
            N::Yellow => 3,
            N::Blue => 4,
            N::Magenta => 5,
            N::Cyan => 6,
            N::White => 7,
            N::BrightBlack => 8,
            N::BrightRed => 9,
            N::BrightGreen => 10,
            N::BrightYellow => 11,
            N::BrightBlue => 12,
            N::BrightMagenta => 13,
            N::BrightCyan => 14,
            N::BrightWhite => 15,
            N::DimBlack => 16,
            N::DimRed => 17,
            N::DimGreen => 18,
            N::DimYellow => 19,
            N::DimBlue => 20,
            N::DimMagenta => 21,
            N::DimCyan => 22,
            N::DimWhite => 23,
            _ => return None,
        })
    }

    /// 按 [`AnsiColors::index_of`] 给出的槽位取色。
    pub fn get(&self, slot: usize) -> Hsla {
        match slot {
            0..=7 => self.normal[slot],
            8..=15 => self.bright[slot - 8],
            16..=23 => self.dim[slot - 16],
            // 只有 index_of 返回的槽位才会走到这里，理论上不可达；
            // 兜底成黑色而不是 panic —— 颜色解析出意外值时不该让整个终端崩掉。
            _ => aa_theme_core::black(),
        }
    }
}

/// 渲染一个终端所需的全部颜色。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalPalette {
    /// 终端底色。
    pub background: Hsla,
    /// 默认前景色（`NamedColor::Foreground`）。
    pub foreground: Hsla,
    /// 亮色前景（`NamedColor::BrightForeground`）。
    pub bright_foreground: Hsla,
    /// 暗色前景（`NamedColor::DimForeground`）。
    pub dim_foreground: Hsla,
    /// 默认背景色（`NamedColor::Background`）。
    pub ansi_background: Hsla,
    /// 16 色 ANSI 表 + 8 个 dim 变体。
    pub ansi: AnsiColors,
    /// 光标颜色（`NamedColor::Cursor`）。
    pub cursor: Hsla,
    /// 选区底色。
    pub selection_background: Hsla,
    /// 选区前景。
    pub selection_foreground: Hsla,
    /// 搜索命中底色。
    pub search_match_background: Hsla,
    /// 当前跳转到的搜索命中的底色。要与普通命中**明确不同**，否则用户看不出
    /// 自己跳到哪一处了。
    pub search_active_match_background: Hsla,
}
