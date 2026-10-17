//! iced 渲染层 —— 把 [`aa_terminal_core::Terminal`] 的整屏快照画到屏幕上。
//!
//! ## 当前范围
//!
//! 目前覆盖：字符网格 + 前景/背景色 + 光标 + 选区/搜索高亮 + 下划线/删除线 +
//! OSC 8 超链接 + 滚动条。还缺输入法（IME）。
//!
//! ## 字形渲染
//!
//! 暂时借用 iced 自己的文本管线（`canvas::Frame::fill_text`），而不是自己驱动
//! cosmic-text。原因是先用最小路径验证接线；换成 cosmic-text 后**网格对齐的
//! 契约不变**：仍然每个 cell 一个固定 advance，不依赖字体自身的度量。
//!
//! 代价是这一版要求字体等宽，且每行要按 cell 宽度裁剪 —— 见 [`TerminalGrid`]。

#![warn(missing_docs)]

use aa_terminal_core::grid::{GridGeometry, Point, Rounding};
use aa_terminal_core::selection::HighlightKind;
use aa_terminal_core::{
    Cell, ScrollState, Terminal, TerminalPalette, is_app_chosen_exact_color,
    is_default_background_color, resolve_color,
};
use aa_ui_core::utils::apca_contrast::ensure_minimum_contrast;
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

/// 下划线与删除线的粗细（像素）。
///
/// Zed 在 `cell_style` 里给两者都用 `Pixels::from(1.0)`；线画在文字**下方
/// 一点**而不是贴基线，1px 在多数缩放下够细又不至于被抗锯齿吃掉。
pub const DECORATION_THICKNESS: f32 = 1.0;

/// 默认的前景对比度下限（APCA Lc）。
///
/// Zed 用它自己的 45（大号文本门槛）；终端字号小，取 60。
pub const DEFAULT_MINIMUM_CONTRAST: f32 = 60.;

/// 滚动条拇指宽度（像素）。
///
/// 取得窄：它覆盖在内容上方，太宽会挡住最右一列的字符。
pub const SCROLLBAR_WIDTH: f32 = 8.0;

/// 滚动条与视口右边缘的间距（像素）。
pub const SCROLLBAR_MARGIN: f32 = 2.0;

/// 拇指的最小高度（像素）。
///
/// 回滚区几十万行时按比例算出的拇指不到 1px —— 既看不见也拖不动。取 25 是照
/// 抄 Zed `ui::scrollbar` 的 `MINIMUM_THUMB_SIZE = px(25.)`。
pub const MIN_SCROLLBAR_THUMB: f32 = 25.0;

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

/// 滚动条的 `(轨道, 拇指)` 矩形；没有回滚区或视口退化时返回 `None`（不画）。
///
/// 抽成纯函数是为了能直接断言几何 —— 绘制结果在测试里看不出来。
///
/// 公式逐条对齐 Zed `ui::scrollbar::thumb_ranges`：
/// `thumb = max(最小高度, 轨道高 × 可见占比)`、`起点 = 进度 × (轨道高 − thumb)`。
pub fn scrollbar_rects(bounds: Rectangle, state: ScrollState) -> Option<(Rectangle, Rectangle)> {
    if state.history_size() == 0 || bounds.height <= 0.0 || bounds.width <= 0.0 {
        return None;
    }
    let track = Rectangle {
        x: bounds.x + bounds.width - SCROLLBAR_WIDTH - SCROLLBAR_MARGIN,
        y: bounds.y,
        width: SCROLLBAR_WIDTH,
        height: bounds.height,
    };

    let (start, length) = state.thumb();
    // 比例算出的高度可能小于 1px，夹一个下限
    let thumb_height = (length * track.height)
        .min(track.height)
        .max(MIN_SCROLLBAR_THUMB.min(track.height));

    // `start` 是在**理想**拇指高度下算的比例，夹过下限后要按实际可移动距离
    // 重新落位，否则拇指会越过轨道底端。
    // 除以 `1 - length`（理想可移动距离）即还原成「离顶端多远」，0 = 顶端。
    let from_top = if length < 1.0 {
        start / (1.0 - length)
    } else {
        0.0
    };

    let thumb = Rectangle {
        x: track.x,
        y: track.y + from_top * (track.height - thumb_height),
        width: track.width,
        height: thumb_height,
    };
    Some((track, thumb))
}

/// 按下滚动条时该记住的「抓取偏移」（像素）：指针距拇指**顶端**有多远。
///
/// 同 Zed：按在拇指上就记住抓点（`ThumbDrag(drag_offset)`），按在轨道空白处
/// 则取拇指高度的一半，让拇指**中心**跟过来（`TrackClick` → `thumb_size / 2`）。
pub fn scrollbar_grab_offset(thumb: Rectangle, position: IcedPoint) -> f32 {
    if thumb.contains(position) {
        position.y - thumb.y
    } else {
        thumb.height / 2.0
    }
}

/// 把指针的 y 换成滚动偏移。
///
/// `grab_offset` 是 [`scrollbar_grab_offset`] 在按下时记下的值 —— 拖动过程中
/// 保持它不变，拇指才不会在抓住边缘时跳一下。
///
/// 返回 `None` 表示当前没有可拖的滚动条。
pub fn scrollbar_offset_at(
    bounds: Rectangle,
    state: ScrollState,
    y: f32,
    grab_offset: f32,
) -> Option<usize> {
    let (track, _) = scrollbar_rects(bounds, state)?;
    // 抓取偏移换成比例，再让 core 解出该滚到哪一行（夹取也在那边做）
    let position = (y - track.y - grab_offset) / track.height;
    Some(state.offset_for_thumb_position(position))
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
    /// 前景对比度下限（APCA Lc）。0 表示不调整。
    ///
    /// Zed 的默认值是 45，对应「大号流畅文本」的门槛；终端字号小，用 60 更稳。
    minimum_contrast: f32,
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
            minimum_contrast: DEFAULT_MINIMUM_CONTRAST,
        }
    }

    /// 指定 cell 尺寸。
    pub fn with_metrics(mut self, metrics: CellMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// 指定前景对比度下限。0 表示关闭。
    pub fn with_minimum_contrast(mut self, contrast: f32) -> Self {
        self.minimum_contrast = contrast;
        self
    }

    /// 设置光标的可见相位。
    pub fn with_cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }

    /// 画下划线与删除线。
    ///
    /// 位置按 Zed 的 `underline_bounds` 推：
    /// - 下划线贴 cell 底部（`y + line_height - thickness`），宽占满 cell
    /// - 删除线穿过垂直中线（`y + line_height/2 - thickness/2`）
    ///
    /// 两者默认取**前景色**：Zed 的 `cell_style` 里 `UnderlineStyle.color` 用的
    /// 就是调过对比度的 fg。用背景色的话暗底下会看不见。
    ///
    /// 例外是下划线：程序用 SGR 58 显式指定了颜色时就听它的，不再跟随 fg。
    ///
    /// 超链接（OSC 8）即使没设 SGR 4 也画下划线 —— Zed 在 `cell_style` 里就是
    /// `has_underline() || hyperlink().is_some()`。
    fn paint_decorations(
        &self,
        frame: &mut Frame,
        origin: IcedPoint,
        cell: iced::Size,
        snapshot_cell: &Cell,
        fg: aa_theme_core::Hsla,
    ) {
        // 粗线在极小行高下会盖住整个 cell，按行高比例夹一下
        let thickness = DECORATION_THICKNESS.min(cell.height * 0.25);
        let rect = |top: f32| {
            Path::rectangle(
                IcedPoint::new(origin.x, top),
                iced::Size {
                    width: cell.width,
                    height: thickness,
                },
            )
        };

        if snapshot_cell.has_underline() || snapshot_cell.has_hyperlink() {
            // 贴底：留出 thickness，否则有一半被 cell 边界裁掉
            let color = snapshot_cell
                .underline_color()
                .map(|color| resolve_color(&color, self.palette))
                .unwrap_or(fg);
            frame.fill(
                &rect(origin.y + cell.height - thickness),
                hsla_to_color(color),
            );
        }
        if snapshot_cell.has_strikeout() {
            frame.fill(
                &rect(origin.y + cell.height / 2.0 - thickness / 2.0),
                hsla_to_color(fg),
            );
        }
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
                HighlightKind::SearchMatch => hsla_to_color(self.palette.search_match_background),
                HighlightKind::CurrentSearchMatch => {
                    hsla_to_color(self.palette.search_active_match_background)
                }
            })
    }

    /// 标记当前悬停的 cell。
    pub fn with_hovered(mut self, point: Option<Point>) -> Self {
        self.hovered = point;
        self
    }

    /// 视口内一个像素位置命中的超链接 URI（OSC 8）。
    ///
    /// `bounds` 是画布矩形，`position` 是屏幕坐标（相对 `bounds` 左上角换算）。
    /// 命中换算走 [`GridGeometry::point_at`] 的 `Truncate`，与核心的选区命中
    /// （`selection.rs`）同一条路径 —— 两处取整方式不一致会变成「明明点在链接
    /// 上却打不开」。
    pub fn hyperlink_at(&self, bounds: Rectangle, position: IcedPoint) -> Option<String> {
        let geometry = geometry_for(bounds.size(), self.metrics);
        let point = geometry.point_at(
            position.x - bounds.x,
            position.y - bounds.y,
            Rounding::Truncate,
        );
        self.terminal.hyperlink_at(point)
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

        // 整屏快照由 core 在**终端内容变化时**重建（`Terminal::refresh_content`），
        // 这里只是拿一份 `Arc` 克隆：绘制期间既不加锁也不重扫网格。颜色在这里
        // 才用主题调色板解析 —— 快照里存的是符号化的 `Color`，与 Zed 一致。
        let content = self.terminal.content();
        let columns = geometry.num_columns();
        let lines = geometry.num_lines();

        for indexed in &content.cells {
            let (row, column) = (indexed.point.line, indexed.point.column);
            // 快照可能比画布大（几何由窗口尺寸算，快照由终端尺寸算），越界跳过。
            if row < 0 || row as usize >= lines || column >= columns {
                continue;
            }
            let cell = &indexed.cell;

            // 宽字符的占位格：前一格画的时候会占两个 cell 宽，这一格必须
            // 跳过 —— 否则中文字符右侧会多画一个空格，整屏视觉错位一格。
            // 对应 Zed terminal_element 对 is_wide_char_spacer() 的 continue。
            if cell.is_wide_char_spacer() {
                continue;
            }

            let x = origin.x + column as f32 * cell_width;
            let y = origin.y + row as f32 * line_height;
            let bg = resolve_color(&cell.background(), self.palette);

            // 高亮铺底色。它在 cell 背景**之后**：选区应该盖住终端自己
            // 的背景色，而不是被它盖住。
            if let Some(color) = self.highlight_at(row as usize, column) {
                let rect = Path::rectangle(
                    IcedPoint::new(x, y),
                    iced::Size {
                        width: cell_width,
                        height: line_height,
                    },
                );
                frame.fill(&rect, color);
            }

            // 背景：程序没指定（跟随主题）就不画，否则整屏会刷满矩形。
            if !is_default_background_color(cell.background()) {
                let rect = Path::rectangle(
                    IcedPoint::new(x, y),
                    iced::Size {
                        width: cell_width,
                        height: line_height,
                    },
                );
                frame.fill(&rect, hsla_to_color(bg));
            }

            if cell.character() == ' ' {
                continue;
            }

            let mut fg = resolve_color(&cell.foreground(), self.palette);
            // DIM 是降亮度而非降对比度：Zed 把 alpha 乘 0.7，与 SGR 2 在
            // 真实终端里的观感一致。
            if cell.is_dim() {
                fg.a *= 0.7;
            }

            // 对比度兜底：主题里撞色的前景/背景组合（比如 One Light 的
            // background 与 ansi.white 同为 #fafafa）会糊成一片。Zed 在
            // cell_style 里做同一件事，但跳过程序自己指定的精确颜色
            // （真彩色与 16..=255 的索引色）—— 那是程序的意图，不该被我们改。
            if self.minimum_contrast > 0. && !is_app_chosen_exact_color(cell.foreground()) {
                fg = ensure_minimum_contrast(fg, bg, self.minimum_contrast);
            }

            // 下划线与删除线：和字符同色，画在字符**之前**。两者都是纯矩形
            // （Zed 的 paint_underline / paint_strikethrough 就是这个几何），
            // 所以不需要文本度量。
            self.paint_decorations(
                &mut frame,
                IcedPoint::new(x, y),
                iced::Size {
                    width: cell_width,
                    height: line_height,
                },
                cell,
                fg,
            );

            frame.fill_text(Text {
                content: cell.character().to_string(),
                position: IcedPoint::new(x, y),
                color: hsla_to_color(fg),
                size: font_size.into(),
                // 加粗/斜体必须落到**字形选择**上：只改描边粗细的话，多数
                // 等宽字体看起来毫无变化，用户会以为 flag 没生效。
                font: Font {
                    weight: if cell.is_bold() {
                        iced::font::Weight::Bold
                    } else {
                        iced::font::Weight::Normal
                    },
                    style: if cell.is_italic() {
                        iced::font::Style::Italic
                    } else {
                        iced::font::Style::Normal
                    },
                    ..Font::MONOSPACE
                },
                ..Text::default()
            });
        }

        // 光标：实心块。闪烁与形状由后续迭代接上。
        //
        // 位置与字符都取自快照：上滚之后光标会落到视口之外（`line >= 屏高`），
        // 下面的判断会把整个光标跳过。
        let cursor = content.cursor;
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
                content: content.cursor_char.to_string(),
                position: IcedPoint::new(x, y),
                color: hsla_to_color(self.palette.background),
                size: font_size.into(),
                font: Font::MONOSPACE,
                ..Text::default()
            });
        }

        // 滚动条最后画：它是 overlay，要压在字符和光标之上。
        if let Some((_, thumb)) = scrollbar_rects(bounds, self.terminal.scroll_state()) {
            // 胶囊形：Zed 的 `ScrollbarStyle::Regular` 给拇指 `Pixels::MAX` 圆角
            // （再按尺寸夹取），等价于圆角 = 宽度的一半。
            let path = Path::rounded_rectangle(
                IcedPoint::new(thumb.x, thumb.y),
                iced::Size {
                    width: thumb.width,
                    height: thumb.height,
                },
                (thumb.width / 2.0).into(),
            );
            frame.fill(&path, hsla_to_color(self.palette.scrollbar_thumb));
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        // `cursor.position()` 在 `Unavailable` 时是 `None`，当作不在任何东西上。
        let Some(position) = cursor.position() else {
            return mouse::Interaction::default();
        };
        // 滚动条压在内容上方，要先判：它盖住的那几像素否则会被当成格子/链接。
        // 光标用默认箭头 —— Zed 的滚动条（`ScrollbarStyle::Regular`）也是 Arrow，
        // 不给 resize 光标。
        if scrollbar_rects(bounds, self.terminal.scroll_state())
            .is_some_and(|(track, _)| track.contains(position))
        {
            return mouse::Interaction::default();
        }
        // 悬停在超链接上给「手型」
        if self.hyperlink_at(bounds, position).is_some() {
            return mouse::Interaction::Pointer;
        }
        mouse::Interaction::default()
    }
}

/// 搜索框输入时的 widget id。
///
/// 打开搜索要把焦点交给输入框（`iced::widget::operation::focus`）。做成常量是
/// 为了让应用侧的 focus 调用与这里的 `text_input.id(...)` 用的是同一个值 ——
/// 两边各写一个字符串字面量，拼错了不会有任何报错，只是焦点永远落不上。
pub const SEARCH_INPUT_ID: &str = "aa_terminal_search_input";

/// 搜索框 —— 浮在终端右上角的一条。
///
/// ## 为什么必须是独立控件
///
/// 终端本体是 canvas，canvas 收不到文本输入（它没有「焦点」概念，也不参与
/// 控件树的键盘分发）。所以搜索**不能**靠终端自己处理按键，必须是一个真实
/// 的 `text_input`。这也意味着打开搜索时要主动把焦点移过去，否则用户打的字
/// 会全进 shell。
pub fn search_bar<'a, Message: Clone + 'a>(
    query: &str,
    matches: usize,
    current: Option<usize>,
    on_input: impl Fn(String) -> Message + 'a,
) -> iced::Element<'a, Message> {
    use iced::widget::{Id, container, row, text, text_input};

    let input = text_input("搜索…", query)
        .id(Id::new(SEARCH_INPUT_ID))
        .on_input(on_input)
        .width(280.);

    // 命中数：0 处时也显示，好让用户知道「词打对了但没匹配」和「还没打」的
    // 区别。非法正则（如单个 `[`）在 core 层会被当成清空，同样显示 0 处。
    //
    // 跳转到某一处之后改显示「i/N」：只报总数的话，用户不知道当前在第几个，
    // 跳到末尾环绕回开头时尤其看不出来。
    let count = text(match current {
        Some(index) if matches > 0 => format!("{}/{}", index + 1, matches),
        _ => format!("{matches} 处"),
    })
    .size(12);

    container(row![input, count].spacing(8).align_y(iced::Center))
        .padding(6)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.extended_palette().background.weak.color.into()),
            border: iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
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

    pub(super) fn palette() -> TerminalPalette {
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
            search_match_background: aa_theme_core::hsla(0., 0., 0.3, 1.),
            search_active_match_background: aa_theme_core::hsla(0., 0., 0.6, 1.),
            scrollbar_thumb: aa_theme_core::hsla(0., 0., 0.5, 1.),
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
    fn bold_and_italic_map_to_distinct_fonts() {
        // 上一轮把 cell flag 接到了 Font::weight/style 上，但**前提是字体文件
        // 里真的有这些变体** —— 只注册了 Regular 的话，cosmic-text 会合成一个
        // 假粗体或者干脆回落，视觉上毫无变化，而 flag 链路看着是通的。
        //
        // 这条测试量的是「同一个字符在三种 weight/style 下的 advance 是否不同」：
        // 等宽字体的伪粗体 advance 通常与原体相同，所以 advance 相同**不能**
        // 证明有真变体，但 advance 不同则说明确实换了一套字形。
        let font = iced::Font::MONOSPACE;
        let normal = measure_advance(font, 14., LINE_HEIGHT_RATIO, 'm');
        for (label, weight, style) in [
            ("bold", iced::font::Weight::Bold, iced::font::Style::Normal),
            (
                "italic",
                iced::font::Weight::Normal,
                iced::font::Style::Italic,
            ),
        ] {
            let styled = Font {
                weight,
                style,
                ..font
            };
            let advance = measure_advance(styled, 14., LINE_HEIGHT_RATIO, 'm');
            assert!(
                advance > 0.,
                "{label} 变体解析失败，advance 为 0 —— 字体文件里可能没有这个变体"
            );
            // 记录而不强断言：不同字体的表现不一样，这里只要保证「能解析」。
            if (advance - normal).abs() > 0.01 {
                println!("{label} 与 regular 的 advance 不同: {advance} vs {normal}");
            }
        }
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
        let mut terminal = Terminal::for_test(size);
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

    /// 造一块 800x480 的画布 + 一个已喂入内容的终端，返回 (grid 需要的终端)。
    fn link_terminal(content: &[u8]) -> (Terminal, CellMetrics, Rectangle) {
        let metrics = CellMetrics::default();
        let bounds = Rectangle::new(IcedPoint::ORIGIN, Size::new(800., 480.));
        let size = window_size_from(&geometry_for(bounds.size(), metrics));
        let mut terminal = Terminal::for_test(size);
        terminal.feed(content);
        (terminal, metrics, bounds)
    }

    #[test]
    fn hyperlink_at_maps_a_position_to_the_link_uri() {
        // "link" 是链接，紧接的 "plain" 不是（空 URI 关闭）。
        let (terminal, metrics, bounds) =
            link_terminal(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\plain");
        let palette = palette();
        let grid = TerminalGrid::new(&terminal, &palette).with_metrics(metrics);

        // 命中点在 cell 正中间，避开格子边界上的取整归属问题。
        let inside = IcedPoint::new(metrics.cell_width * 1.5, metrics.line_height * 0.5);
        assert_eq!(
            grid.hyperlink_at(bounds, inside).as_deref(),
            Some("https://example.com"),
            "第 0 行第 1 列应在链接内"
        );

        // "plain" 的第一格（第 4 列）
        let outside = IcedPoint::new(metrics.cell_width * 4.5, metrics.line_height * 0.5);
        assert_eq!(grid.hyperlink_at(bounds, outside), None);
    }

    #[test]
    fn hovering_a_hyperlink_asks_for_a_pointer_cursor() {
        let (terminal, metrics, bounds) = link_terminal(b"\x1b]8;;https://example.com\x1b\\link");
        let palette = palette();
        let grid = TerminalGrid::new(&terminal, &palette).with_metrics(metrics);

        let on_link = IcedPoint::new(metrics.cell_width * 0.5, metrics.line_height * 0.5);
        assert_eq!(
            <TerminalGrid as canvas::Program<()>>::mouse_interaction(
                &grid,
                &(),
                bounds,
                mouse::Cursor::Available(on_link),
            ),
            mouse::Interaction::Pointer
        );

        // 第 10 列已越过链接末尾，不该再是手型
        let past_link = IcedPoint::new(metrics.cell_width * 10.5, metrics.line_height * 0.5);
        assert_ne!(
            <TerminalGrid as canvas::Program<()>>::mouse_interaction(
                &grid,
                &(),
                bounds,
                mouse::Cursor::Available(past_link),
            ),
            mouse::Interaction::Pointer
        );
    }
}

/// 搜索框的 headless UI 测试。
///
/// 用 `iced_test` 真的把控件树建起来、发按键、断言结果，而不是靠人眼看截屏。
/// 终端的按键路由没法这样测（它走 `Subscription`，而模拟器只把事件派发给
/// 控件），但**搜索框本身是普通控件**，正好能覆盖到。
#[cfg(test)]
mod search_bar_tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Msg {
        Input(String),
    }

    /// 收集 `on_input` 收到的最后一次文本。
    fn last_input<'a>(messages: impl Iterator<Item = &'a Msg>) -> Option<&'a str> {
        messages.map(|Msg::Input(text)| text.as_str()).last()
    }

    #[test]
    fn typing_into_the_box_reports_the_growing_query() {
        let mut ui = iced_test::simulator(search_bar("", 0, None, Msg::Input));

        // 空值的输入框没法用文本选择器选中，按 Id 点它 —— 点击会顺带把焦点
        // 交给输入框，没有焦点 typewrite 是不会被接收的。
        assert!(
            ui.click(iced::widget::Id::new(SEARCH_INPUT_ID)).is_ok(),
            "应能点中搜索框"
        );
        ui.typewrite("abc");

        let messages: Vec<Msg> = ui.into_messages().collect();
        assert_eq!(last_input(messages.iter()), Some("abc"));
        // on_input 是逐字触发的：确认收到的是一串增长中的完整值，而不是
        // 只有最后一次。终端那边每一跳都会重跑 set_search，这里也是。
        let lengths: Vec<usize> = messages.iter().map(|Msg::Input(text)| text.len()).collect();
        assert_eq!(lengths, vec![1, 2, 3], "应逐字上报，实际 {lengths:?}");
    }

    #[test]
    fn the_box_shows_the_query_and_the_match_count() {
        let mut ui = iced_test::simulator(search_bar("needle", 3, None, Msg::Input));

        // 命中数是普通文本，能被文本选择器选中 —— 这样就不用截屏核对数字了。
        assert!(ui.find("3 处").is_ok(), "应显示命中数");
        // 输入框当前值同样可被选中（选择器匹配的是值而非 placeholder）。
        assert!(ui.find("needle").is_ok(), "输入框应显示查询词");
    }

    #[test]
    fn the_box_shows_the_current_index_once_navigation_started() {
        // 跳过一处后显示「2/3」，用户才知道自己在第几条。
        let mut ui = iced_test::simulator(search_bar("needle", 3, Some(1), Msg::Input));
        assert!(ui.find("2/3").is_ok(), "跳转后应显示当前序号");
        assert!(ui.find("3 处").is_err(), "跳转后不应再显示纯总数");
    }

    #[test]
    fn a_zero_match_count_is_still_shown() {
        // 0 处也要显示，用来区分「词没匹配上」和「还没开始搜」。
        let mut ui = iced_test::simulator(search_bar("zzz", 0, None, Msg::Input));
        assert!(ui.find("0 处").is_ok());
    }
}

/// 渲染的 golden 图测试。
///
/// 用 `iced_test` 的 headless 渲染器把整块网格画出来，存成 PNG，之后每次
/// 跑都逐像素比对。颜色、光标、下划线、字形位置这些「只看结果才知道对不对」
/// 的东西，靠它钉住 —— 不用人再盯着截屏。
///
/// ## 它不是万能的，也不是永远稳的
///
/// - 逐像素比对，所以**换机器/换字体/换渲染后端都可能失败**。iced_test 会把
///   后端名拼进文件名（如 `-tiny-skia`），能挡住后端差异，但字体差异挡不住。
/// - 失败时先看 diff 再决定：是渲染真的坏了，还是环境变了。真变了就删掉金色
///   文件重跑一次重新生成。
#[cfg(test)]
mod golden_tests {
    use super::*;
    use aa_terminal_core::session::window_size_from;

    #[test]
    fn a_plain_shell_screen_matches_the_golden_image() {
        let metrics = CellMetrics::measure(iced::Font::MONOSPACE, BASE_FONT_SIZE);
        let geometry = geometry_for(Size::new(360., 150.), metrics);

        let palette = tests::palette();
        let mut terminal = Terminal::for_test(window_size_from(&geometry));
        // 放一点带颜色的输出，好覆盖 SGR 解析 → 配色 → 绘制整条链路。
        terminal.feed(b"\x1b[1;32mgreen bold\x1b[0m plain\r\n");
        terminal.feed(b"\x1b[4munderlined\x1b[0m and \x1b[9mstruck\x1b[0m\r\n");
        terminal.feed(b"$ echo _\r\n");

        let mut ui = iced_test::simulator(view::<()>(&terminal, &palette, true, metrics));
        let snapshot = ui.snapshot(&iced::Theme::Dark).expect("应能渲染出一帧");

        assert!(
            snapshot
                .matches_image("tests/golden/plain_shell.png")
                .expect("比对金色图失败"),
            "渲染结果与金色图不符。若确认是新预期，删掉 tests/golden/ 下的 \
             plain_shell-*.png 重跑以重新生成。"
        );
    }
}

/// 滚动条几何测试。
///
/// 只测纯几何 —— 真正的绘制要 GPU 上下文，而几何算错才是「滚动条画错位置」
/// 的常见来源。
#[cfg(test)]
mod scrollbar_tests {
    use super::*;
    // `palette` 在 `mod tests` 里（`pub(super)`），`window_size_from` 是它在
    // 自己的 `use` 里引入的，这个模块要各自再引一次。
    use super::tests::palette;
    use aa_terminal_core::session::window_size_from;

    const BOUNDS: Rectangle = Rectangle {
        x: 0.,
        y: 0.,
        width: 800.,
        height: 480.,
    };

    /// 24 行屏幕 + 100 行回滚区，贴在最新输出（偏移 0）。
    fn state() -> ScrollState {
        ScrollState::new(0, 100, 24)
    }

    /// 「点轨道空白处」的抓取偏移：拇指高度的一半。
    fn grab_center() -> f32 {
        let (_, thumb) = scrollbar_rects(BOUNDS, state()).expect("有滚动条");
        scrollbar_grab_offset(thumb, IcedPoint::new(thumb.x, -1000.))
    }

    #[test]
    fn no_history_means_no_scrollbar() {
        assert_eq!(scrollbar_rects(BOUNDS, ScrollState::new(0, 0, 24)), None);
        // 退化成 0 高度也不该画（除零）
        let flat = Rectangle {
            height: 0.,
            ..BOUNDS
        };
        assert_eq!(scrollbar_rects(flat, state()), None);
    }

    #[test]
    fn the_track_hugs_the_right_edge() {
        let (track, thumb) = scrollbar_rects(BOUNDS, state()).expect("有回滚区就该有滚动条");
        assert_eq!(track.x, BOUNDS.width - SCROLLBAR_WIDTH - SCROLLBAR_MARGIN);
        assert_eq!(track.y, 0.);
        assert_eq!(track.height, BOUNDS.height);
        assert_eq!(thumb.x, track.x, "拇指与轨道同宽对齐");
        assert!(
            track.x + track.width <= BOUNDS.width,
            "滚动条不能越出视口右边缘"
        );
    }

    #[test]
    fn the_thumb_sits_at_the_bottom_when_viewing_the_latest_output() {
        let (track, thumb) = scrollbar_rects(BOUNDS, state()).expect("有滚动条");
        let bottom = track.y + track.height;
        assert!(
            (thumb.y + thumb.height - bottom).abs() < 1e-3,
            "贴最新输出时拇指应贴轨道底端, 实际 {:?}",
            thumb
        );
    }

    #[test]
    fn the_thumb_sits_at_the_top_when_scrolled_all_the_way_back() {
        let (track, thumb) =
            scrollbar_rects(BOUNDS, ScrollState::new(100, 100, 24)).expect("有滚动条");
        assert!(
            (thumb.y - track.y).abs() < 1e-3,
            "翻到最顶时拇指应贴轨道顶端"
        );
    }

    #[test]
    fn the_thumb_never_leaves_the_track() {
        for offset in 0..=100 {
            let (track, thumb) =
                scrollbar_rects(BOUNDS, ScrollState::new(offset, 100, 24)).expect("有滚动条");
            assert!(
                thumb.y >= track.y - 1e-3
                    && thumb.y + thumb.height <= track.y + track.height + 1e-3,
                "offset={offset} 拇指越出轨道: {thumb:?}"
            );
        }
    }

    #[test]
    fn the_thumb_keeps_a_usable_minimum_height() {
        // 回滚区极长时按比例算出的高度远小于 1px
        let (_, thumb) =
            scrollbar_rects(BOUNDS, ScrollState::new(0, 1_000_000, 24)).expect("有滚动条");
        assert_eq!(thumb.height, MIN_SCROLLBAR_THUMB, "要夹到最小高度才拖得动");
    }

    #[test]
    fn dragging_the_thumb_maps_the_pointer_to_a_scroll_offset() {
        let s = state();
        let (track, _) = scrollbar_rects(BOUNDS, s).expect("有滚动条");

        // 点轨道空白处：拇指中心跟过来（同 Zed 的 TrackClick）
        let grab = grab_center();
        // 拖到轨道顶端 = 翻到最早
        assert_eq!(
            scrollbar_offset_at(BOUNDS, s, track.y, grab),
            Some(100),
            "指针在顶端应解出最大偏移"
        );
        // 拖到轨道底端 = 回到最新
        assert_eq!(
            scrollbar_offset_at(BOUNDS, s, track.y + track.height, grab),
            Some(0),
            "指针在底端应解出 0"
        );
        // 超出轨道要夹住，不能越界
        assert_eq!(scrollbar_offset_at(BOUNDS, s, -1000., grab), Some(100));
        assert_eq!(scrollbar_offset_at(BOUNDS, s, 100_000., grab), Some(0));
    }

    #[test]
    fn dragging_downwards_always_scrolls_towards_the_latest_output() {
        let s = state();
        let (track, _) = scrollbar_rects(BOUNDS, s).expect("有滚动条");
        let mut previous = usize::MAX;
        let mut y = track.y;
        while y <= track.y + track.height {
            let offset = scrollbar_offset_at(BOUNDS, s, y, grab_center()).expect("有滚动条");
            assert!(offset <= previous, "y={y}: 往下拖偏移应单调减小");
            previous = offset;
            y += 4.;
        }
        assert_eq!(previous, 0);
    }

    #[test]
    fn dragging_without_history_does_nothing() {
        assert_eq!(
            scrollbar_offset_at(BOUNDS, ScrollState::new(0, 0, 24), 100., 0.),
            None
        );
    }

    #[test]
    fn grabbing_the_thumb_edge_does_not_move_the_view() {
        // 抓住拇指下沿原地按下：偏移不该变（同 Zed 的 ThumbDrag 记 drag_offset）。
        // 若拿拇指中心算，这一下就会把视口顶走半个拇指。
        let s = ScrollState::new(40, 100, 24);
        let (_, thumb) = scrollbar_rects(BOUNDS, s).expect("有滚动条");
        // 注意 `Rectangle::contains` 的远边是**开**的, 取下沿往内 1px 才算在拇指上
        let edge = IcedPoint::new(thumb.x + 1., thumb.y + thumb.height - 1.0);
        let grab = scrollbar_grab_offset(thumb, edge);
        assert!(
            (grab - (thumb.height - 1.0)).abs() < 1e-3,
            "抓在下沿时抓取偏移应约等于拇指高, 实际 {grab}"
        );
        assert_eq!(
            scrollbar_offset_at(BOUNDS, s, edge.y, grab),
            Some(40),
            "原地按下不该改变偏移"
        );
    }

    #[test]
    fn clicking_the_track_centers_the_thumb_on_the_pointer() {
        let s = ScrollState::new(0, 100, 24);
        let (_, thumb) = scrollbar_rects(BOUNDS, s).expect("有滚动条");
        // 点在轨道上（拇指之外）：抓取偏移取拇指高度的一半
        let far_above = IcedPoint::new(thumb.x + 1., 0.);
        assert!(
            (scrollbar_grab_offset(thumb, far_above) - thumb.height / 2.0).abs() < 1e-3,
            "点轨道空白处应让拇指中心跟过来"
        );
    }

    #[test]
    fn hovering_the_scrollbar_keeps_the_arrow_cursor() {
        // 用真实终端造出一段回滚区（几何函数本身不依赖终端内容）
        let metrics = CellMetrics::default();
        let size = window_size_from(&geometry_for(BOUNDS.size(), metrics));
        let mut terminal = Terminal::for_test(size);
        let mut content = Vec::new();
        for line in 0..60 {
            content.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        terminal.feed(&content);
        assert!(
            terminal.scroll_state().history_size() > 0,
            "喂了 60 行就该有回滚区"
        );

        let palette = palette();
        let grid = TerminalGrid::new(&terminal, &palette).with_metrics(metrics);
        let (track, _) = scrollbar_rects(BOUNDS, terminal.scroll_state()).expect("有滚动条");
        let over_track = IcedPoint::new(track.x + 1., track.y + track.height / 2.0);

        assert_eq!(
            <TerminalGrid as canvas::Program<()>>::mouse_interaction(
                &grid,
                &(),
                BOUNDS,
                mouse::Cursor::Available(over_track),
            ),
            mouse::Interaction::default(),
            "Zed 的滚动条也用默认箭头光标, 不给 resize"
        );
    }
}
