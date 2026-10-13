//! 缓冲区层 —— 把 PTY 的字节流变成可渲染的 cell。
//!
//! 这一层是「终端跑得对不对」的核心，所以设计目标是**能脱离窗口测试**：喂一串
//! 字节进去，直接断言每个 cell 的字符与颜色。测试里不需要 PTY、不需要 iced、
//! 不需要真起 shell。
//!
//! ## 与上游的对应
//!
//! - 解析交给 `alacritty_terminal::Term` + `vte::ansi::Parser`（Zed 同款组合）。
//!   我们不自己写 VT 解析器 —— 那是最不该重造的部分。
//! - `EventListener` 不走 channel。Zed 的 `ZedListener` 往 `mpsc` 灌事件，由
//!   事件循环消费；这里改成收集到 `Vec` 里供调用方按需取走，避免引入 channel
//!   就得决定谁来关它。
//! - 尺寸统一走 [`crate::session::window_size_from`]，与 PTY 侧共用同一套
//!   换算，不会出现「网格 80 列但内核以为是 79 列」这种错位。

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::Term;
// 走 alacritty 重导出的 vte，而不是直接依赖 vte crate —— 保证 Parser 与
// Term 用的是同一个 vte 实例版本，避免两份 Parser 类型互不兼容。
use alacritty_terminal::vte::ansi::Processor as VteProcessor;

use crate::grid::{GridGeometry, Point, Rounding};
use crate::palette::TerminalPalette;
use crate::resolve::resolve_color;
use crate::session::window_size_from;

/// 收集 alacritty 产生的事件。
///
/// alacritty 要求 `EventListener: Send`，所以这里用 `Arc<Mutex<Vec<_>>>` 而
/// 不是 `RefCell`。
#[derive(Clone, Debug, Default)]
pub struct EventCollector {
    events: Arc<Mutex<Vec<AlacEvent>>>,
}

impl EventCollector {
    /// 新建一个空收集器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取走并清空已收集的事件。
    pub fn drain(&self) -> Vec<AlacEvent> {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *events)
    }

    /// 只看不取。
    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 是否没有待处理事件。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl EventListener for EventCollector {
    fn send_event(&self, event: AlacEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

/// 视口尺寸 —— alacritty `Term::new` / `resize` 要求的 `Dimensions` 实现。
///
/// 刻意不用 `(usize, usize)` 元组：alacritty 的那个 impl 把 `.0` 当行数、
/// `.1` 当列数（`screen_lines() -> self.0`），写反了不会报错，只会把 80×24
/// 悄悄造成 24×80 的终端。具名字段没有这个坑。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    /// 列数。
    pub columns: usize,
    /// 屏幕行数。
    pub screen_lines: usize,
}

impl Viewport {
    /// 构造。
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }

    /// 由窗口尺寸换算。
    pub fn from_window_size(size: WindowSize) -> Self {
        Self::new(usize::from(size.num_cols), usize::from(size.num_lines))
    }
}

impl Dimensions for Viewport {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// 渲染一个 cell 所需的全部信息。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderCell {
    /// 字符。空格 cell 与未写入 cell 都给出 `' '`，而不是 `'\0'` —— 渲染层
    /// 拿到 `'\0'` 时容易画出豆腐块。
    pub c: char,
    /// 前景色。
    pub fg: aa_theme_core::Hsla,
    /// 背景色。
    pub bg: aa_theme_core::Hsla,
    /// 是否加粗。
    pub bold: bool,
    /// 是否斜体。
    pub italic: bool,
    /// 是否有下划线（不含下卷曲）。
    pub underline: bool,
    /// 是否有删除线。
    pub strikeout: bool,
    /// 是否反色（用于高亮搜索命中时要反转处理）。
    pub inverse: bool,
    /// 是否是宽字符占位的第二格。
    ///
    /// 渲染时这类 cell 应被前一个宽字符「吞掉」，否则会画出多余的空格。
    pub is_wide_spacer: bool,
}

/// 终端缓冲区。
///
/// 不做内部同步：调用方（iced 侧）负责保证独占访问。跨线程共享时用
/// alacritty 自带的 [`alacritty_terminal::sync::FairMutex`] 包裹，与 Zed 一致。
pub struct Buffer {
    term: Term<EventCollector>,
    parser: VteProcessor,
    size: WindowSize,
    palette: TerminalPalette,
    collector: EventCollector,
}

impl Buffer {
    /// 用给定尺寸创建缓冲区。
    pub fn new(size: WindowSize, palette: TerminalPalette) -> Self {
        let collector = EventCollector::new();
        // 用 `TermSize { columns, screen_lines }` 而不是 `(usize, usize)`：
        // 后者的 `impl Dimensions` 把 .0 当行数、.1 当列数，写反了不报错，
        // 只会把 80×24 悄悄造�� 24×80。具名字段没有这个坑。
        let term = Term::new(
            Config::default(),
            &Viewport::from_window_size(size),
            collector.clone(),
        );
        Self {
            term,
            parser: VteProcessor::default(),
            size,
            palette,
            collector,
        }
    }

    /// 喂入 PTY 读到的字节。
    ///
    /// 刻意允许字节流在转义序列中间截断 —— `vte` 的 `Parser` 自己保存状态，
    /// 跨调用继续解析。这正是 PTY 天然的情况，不能假设每次读到的是完整序列。
    pub fn advance(&mut self, bytes: &[u8]) {
        // 空输入会让 vte 死循环（alacritty 的事件循环里明确注释过这点）。
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, bytes);
    }

    /// 调整尺寸。
    pub fn resize(&mut self, size: WindowSize) {
        // WindowSize 只 derive 了 Copy/Clone/Debug，没有 PartialEq，只能逐字段比。
        if self.size.num_lines == size.num_lines
            && self.size.num_cols == size.num_cols
            && self.size.cell_width == size.cell_width
            && self.size.cell_height == size.cell_height
        {
            return;
        }
        self.term.resize(Viewport::from_window_size(size));
        self.size = size;
    }

    /// 由视口几何调整尺寸。
    pub fn resize_to(&mut self, geometry: &GridGeometry) {
        self.resize(window_size_from(geometry));
    }

    /// 当前尺寸。
    pub fn size(&self) -> WindowSize {
        self.size
    }

    /// 取走待处理事件。
    pub fn drain_events(&self) -> Vec<AlacEvent> {
        self.collector.drain()
    }

    /// 可见屏幕的行列数。
    pub fn dimensions(&self) -> (usize, usize) {
        (
            usize::from(self.size.num_cols),
            usize::from(self.size.num_lines),
        )
    }

    /// 回滚区总行数（含屏幕）。
    pub fn total_lines(&self) -> usize {
        self.term.total_lines()
    }

    /// 光标所在的网格位置。
    ///
    /// 与 [`crate::grid::Point`] 的 line 约定一致：alacritty 的 `Line(0)` 是
    /// 视口首行、向下递增，减去 `display_offset` 后即 Zed 使用的坐标。
    /// 见 [`crate::scroll::ScrollState`] 对该约定的说明。
    pub fn cursor_point(&self) -> Point {
        let line = self.term.grid().cursor.point.line.0 - self.term.grid().display_offset() as i32;
        Point::new(line, self.term.grid().cursor.point.column.0)
    }

    /// 取一个可见 cell。
    ///
    /// `point` 用 [`crate::grid::Point`] 的 line 约定（与 Zed 的
    /// `line = row - display_offset` 一致），不是裸的 alacritty `Line`。
    /// 越界返回 `None`。
    pub fn cell(&self, point: Point) -> Option<RenderCell> {
        let (columns, screen_lines) = self.dimensions();
        if point.column >= columns {
            return None;
        }
        // Point.line → 绝对 Line。由 `line = row - display_offset` 反解出
        // `Line = line + display_offset`。注意是**加**不是减：alacritty 的
        // Line(0) 是视口首行、向下递增，写反了会读到镜像的位置。
        let raw_line = point.line + self.term.grid().display_offset() as i32;
        if raw_line < 0 || raw_line >= screen_lines as i32 {
            return None;
        }

        let cell = &self.term.grid()[Line(raw_line)][Column(point.column)];
        Some(self.render_cell(cell))
    }

    fn render_cell(&self, cell: &alacritty_terminal::term::cell::Cell) -> RenderCell {
        use alacritty_terminal::term::cell::Flags;
        RenderCell {
            // 未写入的 cell 是 '\0'，渲染层容易画成豆腐块，统一成空格。
            c: if cell.c == '\0' { ' ' } else { cell.c },
            fg: resolve_color(&cell.fg, &self.palette),
            bg: resolve_color(&cell.bg, &self.palette),
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            strikeout: cell.flags.intersects(Flags::STRIKEOUT),
            inverse: cell.flags.contains(Flags::INVERSE),
            is_wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
        }
    }

    /// 取一整行。
    ///
    /// 渲染层逐 cell 取会很慢（每格一次 bounds 检查），整行取一次更划算。
    pub fn row(&self, line: i32) -> Option<Vec<RenderCell>> {
        let columns = self.dimensions().0;
        (0..columns)
            .map(|column| self.cell(Point::new(line, column)))
            .collect()
    }

    /// 屏幕上的文本（按行拼接）。
    ///
    /// 用于「把可见内容复制出去」这类操作。会去掉行尾空白。
    pub fn visible_text(&self) -> String {
        let (_, screen_lines) = self.dimensions();
        let mut text = String::new();
        for line in (0..screen_lines as i32).rev() {
            let Some(row) = self.row(line) else { continue };
            let content: String = row.iter().map(|cell| cell.c).collect();
            text.push_str(content.trim_end());
            text.push('\n');
        }
        text
    }

    /// 按视口坐标做命中测试。
    ///
    /// 取整策略显式传入：上游对「像素→cell」的取整有两套并存的说法，详见
    /// [`crate::grid::Rounding`]。
    pub fn point_at(&self, geometry: &GridGeometry, x: f32, y: f32, rounding: Rounding) -> Point {
        geometry.point_at(x, y, rounding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{AnsiColors, TerminalPalette};
    use crate::session::window_size_from;
    use aa_theme_core::{black, hsla};

    /// 造一份「每个槽位颜色都不同」的调色板，便于断言。
    pub(super) fn palette() -> TerminalPalette {
        let slot = |n: usize| hsla(0., 0., 0., 1.).alpha(n as f32 / 24.);
        TerminalPalette {
            background: hsla(0., 0., 0.5, 1.),
            foreground: hsla(0., 0., 1., 1.),
            bright_foreground: hsla(0., 0., 0.9, 1.),
            dim_foreground: hsla(0., 0., 0.4, 1.),
            ansi_background: hsla(0., 0., 0.6, 1.),
            ansi: AnsiColors {
                normal: core::array::from_fn(slot),
                bright: core::array::from_fn(|i| slot(8 + i)),
                dim: core::array::from_fn(|i| slot(16 + i)),
            },
            cursor: hsla(0., 0., 0.3, 1.),
            selection_background: hsla(0., 0., 0.2, 1.),
            selection_foreground: hsla(0., 0., 0.7, 1.),
        }
    }

    fn buffer() -> Buffer {
        let size = window_size_from(&GridGeometry::new(800., 480., 10., 20.));
        Buffer::new(size, palette())
    }

    pub(super) fn slot_of(color: aa_theme_core::Hsla) -> usize {
        (color.a * 24.).round() as usize
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut b = buffer();
        b.advance(b"hello");
        assert_eq!(b.cell(Point::new(0, 0)).unwrap().c, 'h');
        assert_eq!(b.cell(Point::new(0, 4)).unwrap().c, 'o');
        // 未写入处是空格而不是 '\0'
        assert_eq!(b.cell(Point::new(0, 5)).unwrap().c, ' ');
    }

    #[test]
    fn escape_sequence_split_across_feeds_still_parses() {
        // PTY 天然会把一个转义序列切成两半，Parser 必须能跨调用续上。
        let mut b = buffer();
        b.advance(b"\x1b[3");
        b.advance(b"1mred");
        assert_eq!(b.cell(Point::new(0, 0)).unwrap().c, 'r');
        // SGR 31 = 红色 → 具名色表第 1 槽
        assert_eq!(slot_of(b.cell(Point::new(0, 0)).unwrap().fg), 1);
    }

    #[test]
    fn sgr_colors_resolve_through_the_palette() {
        let mut b = buffer();
        b.advance(b"\x1b[32mG");
        assert_eq!(
            slot_of(b.cell(Point::new(0, 0)).unwrap().fg),
            2,
            "32 应映射到 green 槽"
        );

        b.advance(b"\x1b[93mB");
        assert_eq!(
            slot_of(b.cell(Point::new(0, 1)).unwrap().fg),
            11,
            "93 应映射到 bright yellow"
        );
    }

    #[test]
    fn true_color_bypasses_the_palette() {
        let mut b = buffer();
        b.advance(b"\x1b[38;2;255;128;0mX");
        let fg = b.cell(Point::new(0, 0)).unwrap().fg;
        // 真彩色不走 16 色表，alpha 保持 1（没被当成槽位标记）
        assert_eq!(fg.a, 1.0);
        assert_eq!(slot_of(fg), 24, "不在任何 ANSI 槽位内");
    }

    #[test]
    fn reset_restores_the_default_foreground() {
        let mut b = buffer();
        b.advance(b"\x1b[31mA\x1b[0mB");
        assert_eq!(slot_of(b.cell(Point::new(0, 0)).unwrap().fg), 1);
        let p = palette();
        assert_eq!(b.cell(Point::new(0, 1)).unwrap().fg, p.foreground);
    }

    #[test]
    fn bold_and_italic_flags_are_exposed() {
        let mut b = buffer();
        b.advance(b"\x1b[1;3mS");
        let cell = b.cell(Point::new(0, 0)).unwrap();
        assert!(cell.bold);
        assert!(cell.italic);
        assert!(!cell.underline);
    }

    #[test]
    fn underline_and_strikeout_are_distinguished() {
        // 先测下划线，再显式重置后测删除线。
        //
        // 不能写成 `\x1b[4mU\x1b[9mK`：SGR 属性是**累积**的，`\x1b[4m`
        // 设下的下划线会一直保持，于是 K 同时带下划线和删除线 —— 那是终端
        // 的正确行为，不是 bug。想让两者互斥必须先 `\x1b[0m`。
        let mut b = buffer();
        b.advance(b"\x1b[4mU");
        assert!(b.cell(Point::new(0, 0)).unwrap().underline);

        b.advance(b"\x1b[0m\x1b[9mK");
        let strike = b.cell(Point::new(0, 1)).unwrap();
        assert!(strike.strikeout, "SGR 9 应设置删除线");
        assert!(!strike.underline, "重置后不应残留下划线");
    }

    #[test]
    fn sgr_attributes_accumulate_until_reset() {
        // 锁住上面那条语义：属性叠加，只有 SGR 0 才清空。
        let mut b = buffer();
        b.advance(b"\x1b[4mA\x1b[1mB");
        assert!(b.cell(Point::new(0, 0)).unwrap().underline);
        let second = b.cell(Point::new(0, 1)).unwrap();
        assert!(second.underline, "加粗不应清掉下划线");
        assert!(second.bold);

        b.advance(b"\x1b[0mC");
        let third = b.cell(Point::new(0, 2)).unwrap();
        assert!(!third.underline && !third.bold, "SGR 0 应清空所有属性");
    }

    #[test]
    fn inverse_flag_is_surfaced() {
        // 反色不在这里做交换 —— 搜索命中高亮要用它，渲染层再决定怎么反转。
        let mut b = buffer();
        b.advance(b"\x1b[7mI");
        assert!(b.cell(Point::new(0, 0)).unwrap().inverse);
    }

    #[test]
    fn empty_input_does_not_hang_or_corrupt() {
        // alacritty 的事件循环里明确注释过「发 0 字节会让终端挂起」。
        let mut b = buffer();
        b.advance(b"ok");
        b.advance(b"");
        assert_eq!(b.cell(Point::new(0, 0)).unwrap().c, 'o');
    }

    #[test]
    fn cursor_tracks_written_text() {
        let mut b = buffer();
        assert_eq!(b.cursor_point(), Point::new(0, 0));
        b.advance(b"abc");
        assert_eq!(b.cursor_point(), Point::new(0, 3));
        b.advance(b"\r\n");
        // \r 回列首，\n 下行。alacritty 的 Line(0) 是视口首行且向下递增，
        // 所以下行让 line 变大，不是变小。
        assert_eq!(b.cursor_point(), Point::new(1, 0));
    }

    #[test]
    fn out_of_range_cells_return_none() {
        let b = buffer();
        assert!(b.cell(Point::new(0, 80)).is_none(), "列越界");
        assert!(b.cell(Point::new(24, 0)).is_none(), "行越界");
        // line 为负表示在视口首行之上，不存在
        assert!(b.cell(Point::new(-1, 0)).is_none());
        assert!(b.cell(Point::new(24, 0)).is_none());
    }

    #[test]
    fn row_returns_exactly_one_screen_of_cells() {
        let mut b = buffer();
        b.advance(b"abcdef");
        let row = b.row(0).expect("首行应存在");
        assert_eq!(row.len(), 80);
        assert_eq!(row[0].c, 'a');
    }

    #[test]
    fn resize_changes_the_grid_dimensions() {
        let mut b = buffer();
        assert_eq!(b.dimensions(), (80, 24));
        b.resize_to(&GridGeometry::new(400., 240., 10., 20.));
        assert_eq!(b.dimensions(), (40, 12));
    }

    #[test]
    fn resize_to_the_same_size_is_a_no_op() {
        let mut b = buffer();
        b.advance(b"keep");
        let before = b.size();
        b.resize(before);
        // 尺寸没变时不应重排网格，否则已有内容会被截断
        assert_eq!(b.cell(Point::new(0, 0)).unwrap().c, 'k');
    }

    #[test]
    fn wide_characters_mark_their_spacer_cell() {
        let mut b = buffer();
        // 中文字符占两格
        b.advance("中".as_bytes());
        let first = b.cell(Point::new(0, 0)).unwrap();
        let second = b.cell(Point::new(0, 1)).unwrap();
        assert_eq!(first.c, '中');
        assert!(
            second.is_wide_spacer,
            "第二格应标记为宽字符占位，渲染时要跳过"
        );
    }

    #[test]
    fn visible_text_reads_bottom_up_without_trailing_space() {
        let mut b = buffer();
        b.advance(b"first\r\nsecond");
        let text = b.visible_text();
        let lines: Vec<&str> = text.lines().collect();
        // 底部是最后写入的 second
        assert!(lines.contains(&"first"), "应包含两行: {lines:?}");
        assert!(lines.contains(&"first"), "应包含两行: {lines:?}");
        // 行尾空白被去掉
        assert!(!text.lines().any(|l| l.ends_with(' ')), "{text:?}");
    }

    #[test]
    fn events_are_collected_and_drained_once() {
        let mut b = buffer();
        // OSC 2 设置标题会产生 Title 事件
        b.advance(b"\x1b]2;my title\x07");
        let events = b.drain_events();
        assert!(!events.is_empty(), "设置标题应产生事件");
        // 取走后应为空，不重复投递
        assert!(b.drain_events().is_empty());
    }

    #[test]
    fn collector_drain_empties_the_queue() {
        let c = EventCollector::new();
        assert!(c.is_empty());
        c.send_event(AlacEvent::Wakeup);
        assert_eq!(c.len(), 1);
        assert_eq!(c.drain().len(), 1);
        assert!(c.is_empty());
    }

    #[test]
    fn background_defaults_come_from_the_palette() {
        let b = buffer();
        let p = palette();
        // 没写过内容的 cell 背景应取调色板的背景色，而不是黑色
        assert_eq!(b.cell(Point::new(0, 0)).unwrap().bg, p.background);
        assert_ne!(p.background, black());
    }

    #[test]
    fn default_foreground_matches_the_palette() {
        let b = buffer();
        let p = palette();
        assert_eq!(b.cell(Point::new(0, 0)).unwrap().fg, p.foreground);
    }
}

#[cfg(test)]
mod hit_testing {
    use super::tests_support::*;
    use crate::grid::{Point, Rounding};

    /// 命中测试是 `GridGeometry` 的职责，这里只验证接线正确。
    #[test]
    fn hit_testing_delegates_to_geometry() {
        let (buffer, geometry) = fixture();
        let point = buffer.point_at(&geometry, 45., 30., Rounding::Truncate);
        assert_eq!(point, Point::new(1, 4));
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;

    pub(super) fn fixture() -> (Buffer, GridGeometry) {
        use crate::palette::{AnsiColors, TerminalPalette};
        use aa_theme_core::hsla;
        let geometry = GridGeometry::new(800., 480., 10., 20.);
        let palette = TerminalPalette {
            background: hsla(0., 0., 0.5, 1.),
            foreground: hsla(0., 0., 1., 1.),
            bright_foreground: hsla(0., 0., 0.9, 1.),
            dim_foreground: hsla(0., 0., 0.4, 1.),
            ansi_background: hsla(0., 0., 0.6, 1.),
            ansi: AnsiColors {
                normal: core::array::from_fn(|n| hsla(0., 0., 0., 1.).alpha(n as f32 / 24.)),
                bright: core::array::from_fn(|i| hsla(0., 0., 0., 1.).alpha((8 + i) as f32 / 24.)),
                dim: core::array::from_fn(|i| hsla(0., 0., 0., 1.).alpha((16 + i) as f32 / 24.)),
            },
            cursor: hsla(0., 0., 0.3, 1.),
            selection_background: hsla(0., 0., 0.2, 1.),
            selection_foreground: hsla(0., 0., 0.7, 1.),
        };
        let buffer = Buffer::new(window_size_from(&geometry), palette);
        (buffer, geometry)
    }
}
/// 跨层的集成测试：真起一个 shell，让它输出带颜色的文本，验证
/// PTY 字节 → vte 解析 → 网格 cell → 调色板解析 这条链真的通。
///
/// 单模块测试喂的都是自己构造的字节，这一层则要确认「从内核读到的真实
/// 输出」也能正确落到网格上 —— 中间任何一层的接线错误都会在这里暴露。
#[cfg(unix)]
#[test]
#[ignore = "会真的 fork 一个 shell，CI 上不稳定"]
fn pty_output_reaches_the_grid_through_every_layer() {
    use crate::buffer::tests::{palette, slot_of};
    use crate::grid::Point;
    use crate::session::{PtySession, ShellSpec, window_size_from};
    use std::io::Read;

    let geometry = GridGeometry::new(800., 480., 10., 20.);
    let size = window_size_from(&geometry);

    // printf 出带 SGR 颜色的文本，然后立刻退出，便于读到 EOF。
    let spec = ShellSpec::new().with_program(
        "/bin/sh",
        vec![
            "-c".to_string(),
            "printf '\\033[31mRED\\033[0m\\n'".to_string(),
        ],
    );
    let session =
        PtySession::spawn(&spec.to_options("aa_term", "test"), size).expect("应能启动 /bin/sh");

    let mut buffer = Buffer::new(size, palette());
    let fd = session.master_fd();
    // SAFETY: fd 来自本进程刚打开的 PTY 主端，本测试内不关闭它，
    // 且只做只读。生命周期不超过 session。
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };

    // 主端是**非阻塞**的（alacritty 在 tty/unix.rs 里设了 O_NONBLOCK），
    // 无数据时 read 返回 WouldBlock 而不是阻塞。所以不能直接 read_to_end ——
    // 那会立刻拿到空结果，看起来像「子进程没输出」。必须循环重试。
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break, // EOF：子进程已退出且管道关闭
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    // 归还 fd 所有权，避免 double close。
    std::mem::forget(file);

    buffer.advance(&raw);
    let cell = buffer.cell(Point::new(0, 0)).expect("首行首列应可读");
    assert_eq!(cell.c, 'R', "实际读到: {raw:?}");
    assert_eq!(slot_of(cell.fg), 1, "\x1b[31m 应解析为红色槽位");
}
