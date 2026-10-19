//! 主题令牌 → 终端调色板。
//!
//! zed 做这件事的地方是 `TerminalPalette::new`（`crates/terminal`）：底色、前景、
//! ANSI 16 色（含亮/暗变体）取自 `ThemeColors::terminal_*`，光标与选区取自
//! `players().local()`，查找命中与滚动条拇指**直接复用** `ThemeColors` 的同名
//! 令牌。这里保持一致，只是「主题」换成 iced 侧的 [`aa_theme_iced::Theme`]。
//!
//! 于是切深浅主题时终端会跟着变 —— 不再有硬编码的第二套配色。

use aa_terminal_core::palette::{AnsiColors, TerminalPalette};
use aa_theme_core::{Hsla, Rgba};
use aa_theme_iced::Theme;

/// 主题令牌 → [`TerminalPalette`]。
pub fn from_theme(theme: &Theme) -> TerminalPalette {
    let colors = theme.colors();
    let terminal = colors.terminal;
    TerminalPalette {
        background: hsla(terminal.background),
        foreground: hsla(terminal.foreground),
        bright_foreground: hsla(terminal.bright_foreground),
        dim_foreground: hsla(terminal.dim_foreground),
        // zed 里 `terminal_ansi_background` 与 `terminal_background` 是同一个值
        // （都取 base），主题里只存一份。
        ansi_background: hsla(terminal.background),
        ansi: AnsiColors {
            normal: terminal.ansi_normal.map(hsla),
            bright: terminal.ansi_bright.map(hsla),
            dim: terminal.ansi_dim.map(hsla),
        },
        cursor: hsla(terminal.cursor),
        selection_background: hsla(terminal.selection_background),
        selection_foreground: hsla(terminal.selection_foreground),
        // 三个「复用型」令牌：zed 的终端调色板用的是界面同名令牌，不是另存的色。
        search_match_background: hsla(colors.search_match_background),
        search_active_match_background: hsla(colors.search_active_match_background),
        scrollbar_thumb: hsla(colors.scrollbar_thumb_background),
    }
}

/// `iced_core::Color` → gpui 的 `Hsla`。
///
/// 直接搬同一套 sRGB 分量（都是 0..=1 的 f32），不经过 8 位中转 —— 与
/// `aa_terminal_core::resolve::rgb` 的注释同理，能少一次有损转换。
fn hsla(color: iced_core::Color) -> Hsla {
    Hsla::from(Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_terminal_core::resolve::rgb;

    /// 终端底色来自主题，而不是以前那份硬编码的 Catppuccin Mocha
    /// （`0x1e1e2e`）—— 写这条是为了钉住那次改动。
    #[test]
    fn the_background_comes_from_the_theme_not_the_old_hardcoded_palette() {
        let dark = from_theme(&Theme::dark());
        assert_eq!(dark.background, rgb(0x24, 0x27, 0x3a), "Macchiato base");
        assert_ne!(dark.background, rgb(0x1e, 0x1e, 0x2e), "不再是 Mocha base");

        let light = from_theme(&Theme::light());
        assert_eq!(light.background, rgb(0xef, 0xf1, 0xf5), "Latte base");
    }

    /// 前景 / 亮前景 / 暗前景分别对应主题的三个令牌（以前亮前景被写成
    /// rosewater，与主题里的 text 不一致）。
    #[test]
    fn the_foregrounds_come_from_the_three_theme_tokens() {
        for theme in [Theme::dark(), Theme::light()] {
            let palette = from_theme(&theme);
            let terminal = theme.colors().terminal;
            assert_eq!(palette.foreground, hsla(terminal.foreground));
            assert_eq!(palette.bright_foreground, hsla(terminal.bright_foreground));
            assert_eq!(palette.dim_foreground, hsla(terminal.dim_foreground));
            assert_eq!(palette.ansi_background, palette.background);
        }
    }

    /// ANSI 16 色逐槽位取自主题令牌（槽位写错在这里会立刻暴露）。
    #[test]
    fn the_ansi_slots_come_from_the_theme_tokens() {
        for theme in [Theme::dark(), Theme::light()] {
            let palette = from_theme(&theme);
            let terminal = theme.colors().terminal;
            for (slot, expected) in terminal.ansi_normal.iter().enumerate() {
                assert_eq!(palette.ansi.normal[slot], hsla(*expected), "normal[{slot}]");
            }
            for (slot, expected) in terminal.ansi_bright.iter().enumerate() {
                assert_eq!(palette.ansi.bright[slot], hsla(*expected), "bright[{slot}]");
            }
            for (slot, expected) in terminal.ansi_dim.iter().enumerate() {
                assert_eq!(palette.ansi.dim[slot], hsla(*expected), "dim[{slot}]");
            }
        }
    }

    /// `ansi.dim` 以前是八个黑色占位值（SGR 2 的文字会变黑），现在是主题令牌
    /// （ctp 规范里 dim == normal）。
    #[test]
    fn the_dim_slots_are_not_placeholders_anymore() {
        let palette = from_theme(&Theme::dark());
        assert_ne!(palette.ansi.dim, [rgb(0, 0, 0); 8], "不再是一排黑");
        assert_eq!(palette.ansi.dim, palette.ansi.normal, "dim 与 normal 同值");
    }

    /// 光标与选区取自主题里的玩家色，深浅两套要真的不同。
    #[test]
    fn the_cursor_and_selection_come_from_the_theme() {
        let dark = from_theme(&Theme::dark());
        let light = from_theme(&Theme::light());
        let dark_tokens = Theme::dark();
        let dark_tokens = dark_tokens.colors().terminal;
        assert_eq!(dark.cursor, hsla(dark_tokens.cursor));
        assert_eq!(
            dark.selection_background,
            hsla(dark_tokens.selection_background)
        );
        assert_eq!(
            dark.selection_foreground,
            hsla(dark_tokens.selection_foreground)
        );
        assert_ne!(dark.cursor, light.cursor);
        assert_ne!(dark.selection_background, light.selection_background);
    }

    /// 查找命中 / 当前命中 / 滚动条拇指复用界面令牌（zed 也是这么做的）。
    #[test]
    fn the_reused_tokens_match_the_ui_tokens() {
        for theme in [Theme::dark(), Theme::light()] {
            let palette = from_theme(&theme);
            let colors = theme.colors();
            assert_eq!(
                palette.search_match_background,
                hsla(colors.search_match_background)
            );
            assert_eq!(
                palette.search_active_match_background,
                hsla(colors.search_active_match_background)
            );
            assert_eq!(
                palette.scrollbar_thumb,
                hsla(colors.scrollbar_thumb_background)
            );
        }
    }

    /// 深浅两套配色的底色/前景必须分得开：主题传错（或 from_theme 里漏传主题
    /// 字段、用了固定值）都会让两边一模一样。
    #[test]
    fn dark_and_light_palettes_differ_in_more_than_the_background() {
        let dark = from_theme(&Theme::dark());
        let light = from_theme(&Theme::light());
        assert_ne!(dark.foreground, light.foreground);
        assert_ne!(dark.ansi.normal, light.ansi.normal);
        assert_ne!(dark.ansi.bright, light.ansi.bright);
    }
}
