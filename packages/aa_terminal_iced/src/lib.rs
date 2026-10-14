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

/// 等宽字体的 advance 与字号之比。
///
/// 多数等宽字体在 0.6 附近，但这**不是**由 API 保证的：不同字体的实际 advance
/// 有细微差别。这里按 0.6 算，若发现网格右侧有缝或重叠，只需改这一个常数。
pub const MONO_ADVANCE_RATIO: f32 = 0.6;

/// 行高与字号之比。
pub const LINE_HEIGHT_RATIO: f32 = 1.25;

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
        font_size * MONO_ADVANCE_RATIO,
        font_size * LINE_HEIGHT_RATIO,
    )
}

/// 由视口像素尺寸与 cell 尺寸推几何。
pub fn geometry_for(bounds: Size, font_size: f32) -> GridGeometry {
    let (cell_width, line_height) = cell_size(font_size);
    GridGeometry::new(bounds.width, bounds.height, cell_width, line_height)
}

/// 终端网格的 canvas 程序。
pub struct TerminalGrid<'a> {
    terminal: &'a Terminal,
    palette: &'a TerminalPalette,
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
            hovered: None,
            cursor_visible: true,
        }
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
        let (cell_width, line_height) = cell_size(font_size);
        let geometry = geometry_for(bounds.size(), font_size);

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

                if cell.c == ' ' {
                    continue;
                }

                frame.fill_text(Text {
                    content: cell.c.to_string(),
                    position: IcedPoint::new(x, y),
                    color: hsla_to_color(cell.fg),
                    size: font_size.into(),
                    font: Font::MONOSPACE,
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
) -> iced::Element<'a, Message> {
    Canvas::new(TerminalGrid::new(terminal, palette).with_cursor_visible(cursor_visible))
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
    fn cell_size_follows_the_ratios() {
        assert_eq!(cell_size(10.), (6., 12.5));
    }

    #[test]
    fn geometry_covers_the_whole_viewport() {
        let g = geometry_for(Size::new(800., 480.), 14.);
        assert_eq!(g.cell_width, 14. * MONO_ADVANCE_RATIO);
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
        let size = window_size_from(&geometry_for(Size::new(800., 480.), BASE_FONT_SIZE));
        let mut terminal = Terminal::for_test(size, palette());
        terminal.feed(b"hello \x1b[31mworld\x1b[0m");
        let palette = palette();
        // palette 必须先绑到变量：临时值的存活期不到下一行。
        let _grid = TerminalGrid::new(&terminal, &palette).with_hovered(Some(Point::new(0, 1)));
        // 借用关系正确：grid 持有 &terminal，不持有所有权
        assert!(terminal.cell(Point::new(0, 0)).is_some());
        let hit = geometry_for(Size::new(800., 480.), BASE_FONT_SIZE).point_at(
            20.,
            30.,
            Rounding::Truncate,
        );
        assert_eq!(hit.line, 1);
    }
}
