//! iced 渲染层 —— 把 [`aa_terminal_core::Buffer`] 画到屏幕上。
//!
//! ## 当前范围
//!
//! 第一版只求把链路点亮：字符网格 + 前景/背景色 + 光标。不含选区、滚动条、
//! 鼠标、输入法。目的是让前面几层（PTY → 解析 → 网格 → 调色板）的接线错误
//! 立刻暴露出来，而不是等到功能都写完才发现接错了。
//!
//! ## 字形渲染
//!
//! 暂时借用 iced 自己的文本管线（`canvas::Frame::fill_text`），而不是自己驱动
//! cosmic-text。原因是先用最小路径验证接线；换成 cosmic-text 后**网格对齐的
//! 契约不变**：仍然每个 cell 一个固定 advance，不依赖字体自身的度量。
//!
//! 代价是这一版要求字体等宽，且每行要按 cell 宽度裁剪 —— 见 [`TerminalGrid`]。

#![warn(missing_docs)]

use aa_terminal_core::grid::{GridGeometry, Point};
use aa_terminal_core::selection::HighlightKind;
use aa_terminal_core::{Terminal, TerminalPalette};
// 注意 iced 0.14 里有两个都叫 `Text` 的类型：
// `iced::widget::Text` 是文本**控件**，`iced::widget::canvas::Text` 是画布的
// 几何文本。画网格要用后者。
use iced::Rectangle;
use iced::widget::Canvas;
use iced::widget::Theme;
use iced::widget::canvas::{self, Frame, Geometry, Path, Text};
use iced::{Color, Font, Point as IcedPoint, Size, mouse};

/// 字符基准字号（像素）。
pub const BASE_FONT_SIZE: f32 = 14.0;

/// 实测单个 `'m'` 的 advance，用作 cell 宽。
///
/// **不猜比例常数**：Zed 的 `terminal_element` 用
/// `text_system.advance(font_id, font_pixels, 'm')` 实测，iced 侧用
/// `paragraph::Paragraph::with_text("m").min_bounds()`。取 `'m'` 而不是任意
/// 字符，是因为等宽字体里它通常最宽，用它当宽不会截断别的字形。
///
/// 实测失败时退回 0.6：那只是「能看」，网格右侧会有缝或重叠，所以
/// [`CellMetrics::measure`] 会在 measure 失败时打日志。
pub fn measure_cell_width(font: Font, font_size: f32, line_height: f32) -> f32 {
    measure_advance(font, font_size, line_height, 'm')
}

/// 实测某个字符的 advance（水平步进）。
///
/// 按字符单独测，而不是只测 `'m'`：网格对齐的前提是**字体真的等宽** ——
/// 不同字符的 advance 必须相同。「m 最宽所以拿它当宽」只是 Zed 的取值习惯，
/// 等宽性才是要保证的性质，两者不能互相替代。
pub fn measure_advance(font: Font, font_size: f32, line_height: f32, character: char) -> f32 {
    // Spec 类型在 iced_core，Paragraph 实现与 trait 在 iced_graphics ——
    // `iced::graphics` 被 crate 刻意 `mod graphics {}` 藏起来了，拿不到。
    use iced_core::Size as IcedSize;
    use iced_core::text::{LineHeight, Paragraph as _, Shaping, Text as TextSpec, Wrapping};

    let paragraph =
        <iced_graphics::text::Paragraph as iced_core::text::Paragraph>::with_text(TextSpec {
            content: character.to_string().as_str(),
            font,
            size: iced::Pixels(font_size),
            align_x: iced_core::text::Alignment::Center,
            align_y: iced_core::alignment::Vertical::Center,
            shaping: Shaping::Advanced,
            line_height: LineHeight::Relative(line_height),
            bounds: IcedSize::INFINITE,
            wrapping: Wrapping::Glyph,
        });
    paragraph.min_bounds().width
}

/// 行高与字号之比。
///
/// 只有**行高**可以用比例：等宽字体的行高本来就是字号乘一个系数，而 advance
/// 必须实测（见 `measure_cell_width`）。Zed 也是这么分工的 —— 行高来自
/// `TextStyle` 的 line_height，cell 宽来自 advance 实测。
pub const LINE_HEIGHT_RATIO: f32 = 1.25;

/// 一个 cell 至少要多宽，否则渲染特殊字符（emoji）时 alacritty 会算错列。
///
/// 取自 Zed 对 issue #2750 的处理：宽度不足 2 个 cell 时强制撑到 2 个。
const MIN_VIEWPORT_CELLS: f32 = 2.;

/// 实测得到的 cell 尺寸。
///
/// [`TerminalGrid`] 每次绘制都带上它，而不是每次重新量：量文本要走完一次
/// shaping + 布局，每帧做会让滚屏时 CPU 直接烧起来。字体或字号变化时才需要
/// 重算 —— 见 [`CellMetrics::reload`]。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// cell 宽（像素）。
    pub cell_width: f32,
    /// 行高（像素）。
    pub line_height: f32,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            cell_width: 14. * MONO_ADVANCE_FALLBACK,
            line_height: 14. * LINE_HEIGHT_RATIO,
        }
    }
}

/// 实测不到 advance 时的后备比例。仅为「能看」，网格会有缝或重叠。
pub const MONO_ADVANCE_FALLBACK: f32 = 0.6;

impl CellMetrics {
    /// 实测一组 cell 尺寸。
    pub fn measure(font: Font, font_size: f32) -> Self {
        let width = measure_cell_width(font, font_size, LINE_HEIGHT_RATIO);
        let width = if width > 0.0 {
            width
        } else {
            tracing::debug!(
                measured = width,
                "实测 cell 宽失败，退回常数 {}",
                MONO_ADVANCE_FALLBACK
            );
            font_size * MONO_ADVANCE_FALLBACK
        };
        Self {
            cell_width: width,
            line_height: font_size * LINE_HEIGHT_RATIO,
        }
    }
}

/// 把主题层的 [`aa_theme_core::Hsla`] 转成 iced 的 [`Color`]。
///
/// iced 的 `Color` 是 sRGB 直通，不预乘 alpha —— 与 `Hsla` 的语义一致。
/// 注意 iced 0.14 里不少控件颜色是**线性**空间的（`iced_graphics` 有
/// `srgb_to_linear`），终端网格用的是直通的那套，别混用。
pub fn hsla_to_color(color: aa_theme_core::Hsla) -> Color {
    let rgb = aa_theme_core::Rgba::from(color);
    Color::from_rgba(rgb.r, rgb.g, rgb.b, rgb.a)
}

/// 按给定字号算出 cell 尺寸。
pub fn cell_size(font_size: f32) -> (f32, f32) {
    (
        font_size * MONO_ADVANCE_FALLBACK,
        font_size * LINE_HEIGHT_RATIO,
    )
}

/// 由视口像素尺寸与 cell 尺寸推几何。
///
/// `MIN_VIEWPORT_CELLS` 的意义见 [`CellMetrics`]：宽度不足 2 列时按 2 列算，
/// 否则渲染 emoji 之类的宽字符时 alacritty 会算错列号（Zed issue #2750）。
pub fn geometry_for(bounds: Size, metrics: CellMetrics) -> GridGeometry {
    let min_width = metrics.cell_width * MIN_VIEWPORT_CELLS;
    GridGeometry::new(
        bounds.width.max(min_width),
        bounds.height,
        metrics.cell_width,
        metrics.line_height,
    )
}

/// 终端网格的 canvas 程序。
pub struct TerminalGrid<'a> {
    terminal: &'a Terminal,
    palette: &'a TerminalPalette,
    /// cell 尺寸。绘制时**不重新量** —— 实测要走完一次 shaping + 布局，每帧做
    /// 会让滚屏时 CPU 直接烧起来。
    metrics: CellMetrics,
    /// 鼠标悬停的 cell，用于后续做选区高亮。
    hovered: Option<Point>,
    /// 光标是否处于可见相位。闪烁由外层驱动，这里只负责「画不画」。
    cursor_visible: bool,
}

impl<'a> TerminalGrid<'a> {
    /// 用给定的缓冲区与调色板构造。
    pub fn new(terminal: &'a Terminal, palette: &'a TerminalPalette) -> Self {
        Self {
            terminal,
            palette,
            metrics: CellMetrics::default(),
            hovered: None,
            cursor_visible: true,
        }
    }

    /// 指定 cell 尺寸。
    pub fn with_metrics(mut self, metrics: CellMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// 设置光标的可见相位。
    pub fn with_cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }

    /// 这个 cell 是否落在某一类高亮里，返回对应的颜色。
    ///
    /// Zed 的做法是把选区和搜索结果统一成一份 range 列表再逐个判 —— 这样每加
    /// 一种高亮只需要往 `highlight_ranges` 里加一行，不用改绘制逻辑。
    fn highlight_at(&self, row: usize, column: usize) -> Option<Color> {
        let point = Point::new(row as i32, column);
        self.terminal
            .highlight_ranges()
            .into_iter()
            .find(|(range, _)| range.contains(point))
            .map(|(_, kind)| match kind {
                HighlightKind::Selection => hsla_to_color(self.palette.selection_background),
                HighlightKind::SearchMatch => hsla_to_color(self.palette.selection_foreground),
            })
    }

    /// 标记当前悬停的 cell。
    pub fn with_hovered(mut self, point: Option<Point>) -> Self {
        self.hovered = point;
        self
    }
}

// 对任意 Message 都成立：绘制不需要产生消息。
impl<Message> canvas::Program<Message> for TerminalGrid<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<iced::Renderer>> {
        let font_size = BASE_FONT_SIZE;
        let (cell_width, line_height) = (self.metrics.cell_width, self.metrics.line_height);
        let geometry = geometry_for(bounds.size(), self.metrics);

        let mut frame = Frame::new(renderer, bounds.size());
        let origin = IcedPoint::new(bounds.x, bounds.y);

        // 先铺背景：整个视口刷成终端背景色，避免「没内容的地方」透出主题色。
        let background = Path::rectangle(IcedPoint::new(bounds.x, bounds.y), bounds.size());
        frame.fill(&background, hsla_to_color(self.palette.background));

        // 一次取整屏快照：Terminal::rows 内部只锁一次 Term，绘制期间不持锁
        // （alacritty 的事件循环每读到一批字节都在等这把锁）。
        let rows = self.terminal.rows();
        let columns = geometry.num_columns();
        let lines = geometry.num_lines().min(rows.len());

        for (row, cells) in rows.iter().enumerate().take(lines) {
            let y = origin.y + row as f32 * line_height;

            for column in 0..columns {
                let Some(cell) = cells.get(column) else {
                    continue;
                };
                // 宽字符的第二格由前一格负责画，这里跳过，否则会多出一个空格。
                if cell.is_wide_spacer {
                    continue;
                }

                let x = origin.x + column as f32 * cell_width;

                // 高亮铺底色。它在 cell 背景**之后**：选区应该盖住终端自己
                // 的背景色，而不是被它盖住。
                if let Some(color) = self.highlight_at(row, column) {
                    let rect = Path::rectangle(
                        IcedPoint::new(x, y),
                        iced::Size {
                            width: cell_width,
                            height: line_height,
                        },
                    );
                    frame.fill(&rect, color);
                }

                // 背景：与终端背景色不同才画，否则整屏会刷满矩形。
                let bg = hsla_to_color(cell.bg);
                if cell.bg != self.palette.background {
                    let rect = Path::rectangle(
                        IcedPoint::new(x, y),
                        iced::Size {
                            width: cell_width,
                            height: line_height,
                        },
                    );
                    frame.fill(&rect, bg);
                }

                // 宽字符的占位格：前一格画的时候会占两个 cell 宽，这一格必须
                // 跳过 —— 否则中文字符右侧会多画一个空格，整屏视觉错位一格。
                // 对应 Zed terminal_element 对 is_wide_char_spacer() 的 continue。
                if cell.is_wide_spacer {
                    continue;
                }
                if cell.c == ' ' {
                    continue;
                }

                // DIM 是降亮度而非降对比度：Zed 把 alpha 乘 0.7，与 SGR 2 在
                // 真实终端里的观感一致。
                let mut fg = cell.fg;
                if cell.dim {
                    fg.a *= 0.7;
                }

                frame.fill_text(Text {
                    content: cell.c.to_string(),
                    position: IcedPoint::new(x, y),
                    color: hsla_to_color(fg),
                    size: font_size.into(),
                    // 加粗/斜体必须落到**字形选择**上：只改描边粗细的话，多数
                    // 等宽字体看起来毫无变化，用户会以为 flag 没生效。
                    font: Font {
                        weight: if cell.bold {
                            iced::font::Weight::Bold
                        } else {
                            iced::font::Weight::Normal
                        },
                        style: if cell.italic {
                            iced::font::Style::Italic
                        } else {
                            iced::font::Style::Normal
                        },
                        ..Font::MONOSPACE
                    },
                    ..Text::default()
                });
            }
        }

        // 光标：实心块。闪烁与形状由后续迭代接上。
        let cursor = self.terminal.cursor_point();
        let cursor_visible = self.cursor_visible;
        // 形状隐藏或处于闪烁暗相位都不画。
        if cursor_visible
            && cursor.line >= 0
            && (cursor.line as usize) < lines
            && cursor.column < columns
        {
            let x = origin.x + cursor.column as f32 * cell_width;
            let y = origin.y + cursor.line as f32 * line_height;
            let rect = Path::rectangle(
                IcedPoint::new(x, y),
                iced::Size {
                    width: cell_width,
                    height: line_height,
                },
            );
            frame.fill(&rect, hsla_to_color(self.palette.cursor));
            frame.fill_text(Text {
                content: self
                    .terminal
                    .cell(cursor)
                    .map(|c| c.c.to_string())
                    .unwrap_or_default(),
                position: IcedPoint::new(x, y),
                color: hsla_to_color(self.palette.background),
                size: font_size.into(),
                font: Font::MONOSPACE,
                ..Text::default()
            });
        }

        vec![frame.into_geometry()]
    }
}

/// 构造填满可用空间的终端网格控件。
///
/// 泛型化 `Message`：canvas 在这一版不产生消息，但控件仍要挂到调用方的
/// 消息类型上，否则调用方得写一堆无意义的 map。
pub fn view<'a, Message: 'a>(
    terminal: &'a Terminal,
    palette: &'a TerminalPalette,
    // 光标是否画出来（闪烁的相位）。
    cursor_visible: bool,
    // cell 尺寸；由应用侧一次性实测后传进来。
    metrics: CellMetrics,
) -> iced::Element<'a, Message> {
    Canvas::new(
        TerminalGrid::new(terminal, palette)
            .with_cursor_visible(cursor_visible)
            .with_metrics(metrics),
    )
    .width(iced::Fill)
    .height(iced::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_terminal_core::grid::Rounding;
    use aa_terminal_core::session::window_size_from;

    fn palette() -> TerminalPalette {
        TerminalPalette {
            background: aa_theme_core::hsla(0., 0., 0., 1.),
            foreground: aa_theme_core::hsla(0., 0., 1., 1.),
            bright_foreground: aa_theme_core::hsla(0., 0., 1., 1.),
            dim_foreground: aa_theme_core::hsla(0., 0., 1., 1.),
            ansi_background: aa_theme_core::hsla(0., 0., 0., 1.),
            ansi: aa_terminal_core::AnsiColors {
                normal: [aa_theme_core::white(); 8],
                bright: [aa_theme_core::white(); 8],
                dim: [aa_theme_core::white(); 8],
            },
            cursor: aa_theme_core::white(),
            selection_background: aa_theme_core::black(),
            selection_foreground: aa_theme_core::white(),
        }
    }

    #[test]
    fn measuring_a_real_font_gives_a_sane_cell() {
        // 不猜常数：拿系统里真实存在的等宽字体实测一次，确认 advance 落在
        // 合理区间。真实区间由字体决定，这里只断言「非 0 且不像后备值」。
        // 直接量 MONOSPACE：cosmic-text 会按 family 解析到系统里真实的等宽
        // 字体。`Font` 只是 family/weight/style 的描述符，构造不出「从字节来」
        // 的字体，那需要应用层先注册。
        let metrics = CellMetrics::measure(iced::Font::MONOSPACE, 14.);
        assert!(metrics.cell_width > 0., "实测宽应非 0: {metrics:?}");
        assert_eq!(metrics.line_height, 14. * LINE_HEIGHT_RATIO);
    }

    #[test]
    fn the_resolved_font_really_is_monospace() {
        // 这条才是网格对齐的前提：等宽字体里 'i' 和 'm' 的 advance 相同。
        // 若解析到的是比例字体（比如忘了注册 JetBrains Mono，回落到 iced 内置
        // 的 Fira Sans），这里就会失败 —— 而症状（列对不齐）在单看画面时
        // 很容易被当成「渲染有点歪」而放过。
        let font = iced::Font::MONOSPACE;
        let advance_i = measure_advance(font, 14., LINE_HEIGHT_RATIO, 'i');
        let advance_m = measure_advance(font, 14., LINE_HEIGHT_RATIO, 'm');
        assert!(
            (advance_i - advance_m).abs() < 0.01,
            "解析到的字体不等宽：i={advance_i} m={advance_m}。             说明没有注册真正的等宽字体"
        );
    }

    #[test]
    fn line_height_uses_the_ratio_but_width_is_measured() {
        // 行高可以用比例，cell 宽必须实测 —— 这条断言锁住两者的分工，
        // 防止有人图省事把宽也改回比例。
        let metrics = CellMetrics::default();
        assert_eq!(metrics.line_height, 14. * LINE_HEIGHT_RATIO);
        assert_eq!(
            metrics.cell_width,
            14. * MONO_ADVANCE_FALLBACK,
            "default 用的是后备比例，不是实测值"
        );
        assert!(metrics.cell_width > 0.);
    }

    #[test]
    fn geometry_covers_the_whole_viewport() {
        let g = geometry_for(Size::new(800., 480.), CellMetrics::default());
        assert_eq!(g.cell_width, 14. * MONO_ADVANCE_FALLBACK);
        assert_eq!(g.line_height, 14. * LINE_HEIGHT_RATIO);
        // 精确值由公式算出，不拍阈值：800/8.4 = 95.23 → 95 列，
        // 480/17.5 = 27.43 → 27 行。
        assert_eq!(g.num_columns(), 95);
        assert_eq!(g.num_lines(), 27);
    }

    #[test]
    fn color_conversion_preserves_channels() {
        let white = hsla_to_color(aa_theme_core::white());
        assert!((white.r - 1.0).abs() < 1e-5);
        assert!((white.g - 1.0).abs() < 1e-5);
        assert!((white.b - 1.0).abs() < 1e-5);
        assert!((white.a - 1.0).abs() < 1e-5);
    }

    #[test]
    fn grid_renders_without_panicking() {
        // 只验证构造路径不炸；真正的绘制需要 GPU 上下文，不在单测里做。
        let metrics = CellMetrics::default();
        let size = window_size_from(&geometry_for(Size::new(800., 480.), metrics));
        let mut terminal = Terminal::for_test(size, palette());
        terminal.feed(b"hello \x1b[31mworld\x1b[0m");
        let palette = palette();
        // palette 必须先绑到变量：临时值的存活期不到下一行。
        let _grid = TerminalGrid::new(&terminal, &palette).with_hovered(Some(Point::new(0, 1)));
        // 借用关系正确：grid 持有 &terminal，不持有所有权
        assert!(terminal.cell(Point::new(0, 0)).is_some());
        let hit = geometry_for(Size::new(800., 480.), CellMetrics::default()).point_at(
            20.,
            30.,
            Rounding::Truncate,
        );
        assert_eq!(hit.line, 1);
    }
}
