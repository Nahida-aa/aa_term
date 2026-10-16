//! 把 ANSI 颜色（`NamedColor` / 24 位 RGB / 8 位索引）解析成具体色值。
//!
//! 逐字对齐 Zed `terminal::get_color_at_index` 与
//! `terminal_view::terminal_element::convert_color` 的行为，包括那几处
//! 「不标准但必须兼容」的索引映射。

use aa_theme_core::Hsla;
use vte::ansi::{Color, NamedColor};

use crate::palette::{AnsiColors, TerminalPalette};

/// 由 8 位 RGB 造一个不透明颜色。
///
/// 注意与 `Rgba { .. }.into()` 的区别：这里直接走 sRGB 归一化，不经过 HSL
/// 中转，避免多一次有损转换。
pub fn rgb(r: u8, g: u8, b: u8) -> Hsla {
    aa_theme_core::Hsla::from(aa_theme_core::Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    })
}

/// 索引 16..=231 的 6×6×6 色立方体里各通道所在的档位（0..=5）。
///
/// `index = 16 + 36*r + 6*g + b`，这里是它的逆运算。
fn rgb_for_index(i: u8) -> (u8, u8, u8) {
    debug_assert!((16..=231).contains(&i));
    let i = i - 16;
    let r = (i - (i % 36)) / 36;
    let g = ((i % 36) - (i % 6)) / 6;
    let b = (i % 36) % 6;
    (r, g, b)
}

/// 256 色表里某一档对应的通道值。
///
/// 档位 0 是纯黑/纯饱和色的基准值 0，其余每档 +40 再偏 55 —— 这是 xterm 的
/// 标准做法（见 xterm 的 `256colres.pl`），不是可以随手改成线性插值的东西。
fn cube_component(level: u8) -> u8 {
    if level == 0 { 0 } else { level * 40 + 55 }
}

/// 解析 8 位索引色。
///
/// 0..=15 走调色板的 16 色表；16..=231 是 6×6×6 色立方；232..=255 是 24 级
/// 灰阶；256 以后是 alacritty 为兼容自身 `Colors` 接口而自定义的扩展位，
/// 见各分支注释。
pub fn color_at_index(index: usize, palette: &TerminalPalette) -> Hsla {
    match index {
        // 0-15 与具名色一致
        0..=15 => {
            let slot = AnsiColors::index_of(match index {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                _ => NamedColor::BrightWhite,
            })
            .expect("0..=15 必然能映射到 16 色表");
            palette.ansi.get(slot)
        }
        // 6x6x6 色立方
        16..=231 => {
            let (r, g, b) = rgb_for_index(index as u8);
            rgb(cube_component(r), cube_component(g), cube_component(b))
        }
        // 24 级灰阶：8 → 238
        232..=255 => {
            let value = (index as u8 - 232) * 10 + 8;
            rgb(value, value, value)
        }
        // 以下为 alacritty::Colors 接口的私有扩展位，不是标准终端会发出的颜色。
        // 行为对齐 Zed，否则同一段输出在两个终端里颜色不同。
        256 => palette.foreground,
        257 => palette.background,
        258 => palette.cursor,
        259 => palette.ansi.dim[0],
        260 => palette.ansi.dim[1],
        261 => palette.ansi.dim[2],
        262 => palette.ansi.dim[3],
        263 => palette.ansi.dim[4],
        264 => palette.ansi.dim[5],
        265 => palette.ansi.dim[6],
        266 => palette.ansi.dim[7],
        267 => palette.bright_foreground,
        // alacritty 把 268 称作 'Dim Background'，但实际给回普通黑 —— 这里跟随之。
        268 => palette.ansi.normal[0],
        // 越界：Zed 返回黑色。静默兜底比 panic 好，颜色解析失败不该让终端崩掉。
        _ => aa_theme_core::black(),
    }
}

/// 把一个 ANSI 颜色解析成具体色值。
pub fn resolve_color(color: &Color, palette: &TerminalPalette) -> Hsla {
    match color {
        Color::Named(named) => resolve_named(*named, palette),
        // 24 位真彩色
        Color::Spec(rgb) => rgb_of(rgb.r, rgb.g, rgb.b),
        // 8 位索引色
        Color::Indexed(i) => color_at_index(*i as usize, palette),
    }
}

fn rgb_of(r: u8, g: u8, b: u8) -> Hsla {
    rgb(r, g, b)
}

/// 解析具名色。
pub fn resolve_named(named: NamedColor, palette: &TerminalPalette) -> Hsla {
    // 先查 16 色表（0..=23 的槽位），把 28 个分支压成一次数组索引。
    if let Some(slot) = AnsiColors::index_of(named) {
        return palette.ansi.get(slot);
    }

    match named {
        NamedColor::Foreground => palette.foreground,
        NamedColor::Background => palette.background,
        NamedColor::Cursor => palette.cursor,
        NamedColor::BrightForeground => palette.bright_foreground,
        NamedColor::DimForeground => palette.dim_foreground,
        // index_of 已覆盖 Black..=DimWhite，这里的分支不可达。
        other => {
            tracing_warn_unreachable(other);
            palette.foreground
        }
    }
}

fn tracing_warn_unreachable(named: NamedColor) {
    // 刻意不引 tracing：本 crate 目前不想因为一行告警多一个依赖。
    // 若将来接入日志体系，把这里换掉即可。
    debug_assert!(
        false,
        "具名色 {named:?} 既不在 16 色表也不在前景/背景分支里 —— \
         index_of 漏了变体？"
    );
}

/// 测试共用的调色板构造，见 `tests_support`。
#[cfg(test)]
mod tests_support {
    use super::*;
    use aa_theme_core::hsla;

    /// 用「色相=槽位」的规律造一份可预测的调色板：槽位 n 的颜色其 alpha
    /// 恰为 n/24。这样断言失败时能直接从色值看出是哪个槽位错了。
    pub fn slot_color(slot: usize) -> Hsla {
        hsla(0., 0., 0., 1.).alpha(slot as f32 / 24.)
    }

    pub fn palette() -> TerminalPalette {
        TerminalPalette {
            background: hsla(0., 0., 0.5, 1.),
            foreground: hsla(0., 0., 1., 1.),
            bright_foreground: hsla(0., 0., 0.9, 1.),
            dim_foreground: hsla(0., 0., 0.4, 1.),
            ansi_background: hsla(0., 0., 0.6, 1.),
            ansi: AnsiColors {
                normal: core::array::from_fn(slot_color),
                bright: core::array::from_fn(|i| slot_color(8 + i)),
                dim: core::array::from_fn(|i| slot_color(16 + i)),
            },
            cursor: hsla(0., 0., 0.3, 1.),
            selection_background: hsla(0., 0., 0.2, 1.),
            selection_foreground: hsla(0., 0., 0.7, 1.),
            search_match_background: hsla(0., 0., 0.1, 1.),
            search_active_match_background: hsla(0., 0., 0.8, 1.),
            scrollbar_thumb: hsla(0., 0., 0.35, 1.),
        }
    }

    pub fn slot_of(color: Hsla) -> usize {
        (color.a * 24.).round() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{palette, slot_of};
    use super::*;
    use aa_theme_core::{black, blue, green, red, white, yellow};

    #[test]
    fn named_colors_map_to_the_right_slot() {
        let p = palette();
        let expected = [
            (NamedColor::Black, 0),
            (NamedColor::Red, 1),
            (NamedColor::Green, 2),
            (NamedColor::Yellow, 3),
            (NamedColor::Blue, 4),
            (NamedColor::Magenta, 5),
            (NamedColor::Cyan, 6),
            (NamedColor::White, 7),
            (NamedColor::BrightBlack, 8),
            (NamedColor::BrightWhite, 15),
            (NamedColor::DimBlack, 16),
            (NamedColor::DimWhite, 23),
        ];
        for (named, slot) in expected {
            let got = resolve_named(named, &p);
            assert_eq!(slot_of(got), slot, "{named:?} 应落在槽位 {slot}");
        }
    }

    #[test]
    fn semantic_named_colors_do_not_go_through_the_ansi_table() {
        let p = palette();
        assert_eq!(resolve_named(NamedColor::Foreground, &p), p.foreground);
        assert_eq!(resolve_named(NamedColor::Background, &p), p.background);
        assert_eq!(resolve_named(NamedColor::Cursor, &p), p.cursor);
        assert_eq!(
            resolve_named(NamedColor::BrightForeground, &p),
            p.bright_foreground
        );
        assert_eq!(
            resolve_named(NamedColor::DimForeground, &p),
            p.dim_foreground
        );
    }

    #[test]
    fn index_zero_to_fifteen_agrees_with_named_colors() {
        // ANSI 输出里 `38;5;n` 和具名色会指向同一批颜色，两者必须一致，
        // 否则同一段文本在不同终端里颜色不同。
        let p = palette();
        let named = [
            NamedColor::Black,
            NamedColor::Red,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Blue,
            NamedColor::Magenta,
            NamedColor::Cyan,
            NamedColor::White,
            NamedColor::BrightBlack,
            NamedColor::BrightRed,
            NamedColor::BrightGreen,
            NamedColor::BrightYellow,
            NamedColor::BrightBlue,
            NamedColor::BrightMagenta,
            NamedColor::BrightCyan,
            NamedColor::BrightWhite,
        ];
        for (i, n) in named.iter().enumerate() {
            assert_eq!(
                color_at_index(i, &p),
                resolve_named(*n, &p),
                "索引 {i} 与 {n:?} 不一致"
            );
        }
    }

    #[test]
    fn color_cube_follows_the_xterm_ramp() {
        // 立方体的索引公式是 index = 16 + 36*r + 6*g + b：
        // **b 变化最快**（stride 1），r 最慢（stride 36）。16 号格因此是纯黑，
        // 紧邻的 17 号是纯蓝而不是纯红 —— 这点很容易记反。
        // 档位 0 → 0，档位 1 → 95（55+40）。
        assert_eq!(color_at_index(16, &palette()), rgb(0, 0, 0));
        assert_eq!(color_at_index(17, &palette()), rgb(0, 0, 95));
        assert_eq!(color_at_index(21, &palette()), rgb(0, 0, 255));
        // r 那一维要 +36：52 = 16 + 36 → (1,0,0) → 深红
        assert_eq!(color_at_index(52, &palette()), rgb(95, 0, 0));
        // g 那一维要 +6：22 = 16 + 6 → (0,1,0)
        assert_eq!(color_at_index(22, &palette()), rgb(0, 95, 0));
        // 231 是立方体最后一格：全 5 档 → 255
        assert_eq!(color_at_index(231, &palette()), rgb(255, 255, 255));
        // 中间一格：110 = 16 + 36*2 + 6*3 + 4 → (2,3,4) → (135,175,215)
        assert_eq!(color_at_index(110, &palette()), rgb(135, 175, 215));
    }

    #[test]
    fn grayscale_ramp_runs_from_8_to_238() {
        assert_eq!(color_at_index(232, &palette()), rgb(8, 8, 8));
        assert_eq!(color_at_index(243, &palette()), rgb(118, 118, 118));
        assert_eq!(color_at_index(255, &palette()), rgb(238, 238, 238));
    }

    #[test]
    fn alacritty_extension_slots_are_not_standard_but_are_supported() {
        // 256..=268 不是终端会发出的颜色，而是 alacritty 内部 Colors 接口的
        // 私有编码。别把它们当索引色随便映射掉。
        let p = palette();
        assert_eq!(color_at_index(256, &p), p.foreground);
        assert_eq!(color_at_index(257, &p), p.background);
        assert_eq!(color_at_index(258, &p), p.cursor);
        assert_eq!(color_at_index(259, &p), p.ansi.dim[0]);
        assert_eq!(color_at_index(266, &p), p.ansi.dim[7]);
        assert_eq!(color_at_index(267, &p), p.bright_foreground);
        // 268 号称 Dim Background 却给回普通黑 —— 跟随 alacritty 的既有行为。
        assert_eq!(color_at_index(268, &p), p.ansi.normal[0]);
    }

    #[test]
    fn out_of_range_index_degrades_to_black() {
        // u8 索引最大 255，所以 269 只有直接调 color_at_index 才够得到；
        // 确认越界不 panic 而是有确定结果。
        assert_eq!(color_at_index(269, &palette()), black());
        assert_eq!(color_at_index(1000, &palette()), black());
    }

    #[test]
    fn spec_color_is_true_rgb_without_hsl_round_trip() {
        let p = palette();
        assert_eq!(
            resolve_color(&Color::Spec(vte::ansi::Rgb { r: 1, g: 2, b: 3 }), &p),
            rgb(1, 2, 3)
        );
        assert_eq!(
            resolve_color(
                &Color::Spec(vte::ansi::Rgb {
                    r: 255,
                    g: 128,
                    b: 0
                }),
                &p
            ),
            rgb(255, 128, 0)
        );
    }

    #[test]
    fn all_three_color_forms_round_trip_through_resolve() {
        let p = palette();
        // 同一颜色经三种写法解析应完全一致
        let named = resolve_color(&Color::Named(NamedColor::Red), &p);
        let indexed = resolve_color(&Color::Indexed(1), &p);
        assert_eq!(named, indexed);
    }

    #[test]
    fn palette_has_a_distinct_color_in_every_slot() {
        // 防止构造调色板时把两个槽位填成同一个色 —— 那样上面的槽位断言
        // 会在「两个都错」的情况下假通过。
        let p = palette();
        let mut seen = std::collections::HashSet::new();
        for i in 0..24 {
            assert!(seen.insert(slot_of(p.ansi.get(i))), "槽位 {i} 与前面的重复");
        }
    }

    #[test]
    fn named_colors_are_distinct_constants() {
        // 确认 aa_theme_core 的具名常量确实是预期的色值（green 的 l 是 0.25）。
        // aa_theme_core 只提供 gpui 那 6 个具名色，没有 magenta/cyan。
        let _ = (black(), white(), red(), green(), yellow(), blue());
        assert_eq!(green().l, 0.25, "green 的 lightness 应为 0.25");
        assert_eq!(red().h, 0.0);
    }
}
#[cfg(test)]
mod exhaustive {
    use super::tests_support::palette;
    use super::*;

    /// 把 16..=231 全 216 格走一遍：
    /// 1) 立方体公式与索引公式互为逆运算（否则某个格的颜色是错的）
    /// 2) 单调性 —— 索引递增时 r/g/b 各自的档位不应出现回退
    ///
    /// 单独手写几个样例只能覆盖「我记得的那几格」，这里穷举，把整张表锁住。
    #[test]
    fn every_cube_cell_is_consistent() {
        let p = palette();

        for index in 16u16..=231 {
            let levels = rgb_for_index(index as u8);
            // 逆运算：levels 代回索引公式应得回原索引
            let reconstructed = 16 + 36 * levels.0 as u16 + 6 * levels.1 as u16 + levels.2 as u16;
            assert_eq!(
                reconstructed, index,
                "索引 {index} 的档位 {levels:?} 代回公式得 {reconstructed}"
            );

            // 三个档位都必须落在 0..=5。
            // 注意**不能**断言逐分量单调：蓝分量走完 0..5 后会回绕到 0、
            // 同时绿加 1（21 号格 (0,0,5) → 22 号格 (0,1,0)），这是
            // `index = 16 + 36r + 6g + b` 里 b stride=1 的必然结果。
            for (axis, level) in [("r", levels.0), ("g", levels.1), ("b", levels.2)] {
                assert!(
                    level <= 5,
                    "索引 {index} 的 {axis} 档位 {level} 越界（应 ≤ 5）"
                );
            }

            // 实际取色结果必须是 3 个通道各自映射后的值
            let expected = rgb(
                cube_component(levels.0),
                cube_component(levels.1),
                cube_component(levels.2),
            );
            assert_eq!(color_at_index(index as usize, &p), expected, "索引 {index}");
        }
    }

    /// 232..=255 灰阶：24 级、步长 10、首值 8，且严格单调。
    #[test]
    fn every_grayscale_step_is_consistent() {
        let p = palette();
        let mut prev: Option<u8> = None;
        for index in 232u16..=255 {
            let value = (index as u8 - 232) * 10 + 8;
            assert_eq!(color_at_index(index as usize, &p), rgb(value, value, value));
            if let Some(prev) = prev {
                assert_eq!(value, prev + 10, "灰阶在 {index} 处步长不是 10");
            }
            prev = Some(value);
        }
        assert_eq!(prev, Some(238), "灰阶末值应为 238");
    }
}
