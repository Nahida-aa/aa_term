//! 整屏快照 —— 一次把可见屏幕取出来，之后绘制不再碰 `Term` 锁。
//!
//! 对应 Zed `terminal/src/alacritty.rs` 的 `make_content` + `Content`：在
//! **终端内容变化时**重建一份，渲染帧只是读这份快照。之前是每帧现取
//! （`rows()`），于是连光标闪烁这种与终端无关的重绘也要锁一次 `Term`、解析
//! 一整屏颜色；现在重建只发生在 `feed` / 滚动 / resize / PTY 输出到来时。

use alacritty_terminal::event::EventListener;
// `columns()` / `screen_lines()` 来自这个 trait，不引入就只能拿到 `grid()` 本身。
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::{Term, point_to_viewport};

use crate::grid::Point;
use crate::grid::content::cell::{Cell, IndexedCell};

/// 一屏可见内容。
///
/// `cells` 是**扁平**的（按视口顺序：先第一行从左到右，再第二行），与 Zed 的
/// `Content { cells: Vec<IndexedCell> }` 同形；每格自带坐标，所以渲染层既可以
/// 顺序画，也可以按 `point` 定位。
#[derive(Debug, Clone)]
pub struct Content {
    /// 可见的 cell，按视口顺序。
    pub cells: Vec<IndexedCell>,
    /// 列数。
    pub columns: usize,
    /// 屏幕行数。
    pub screen_lines: usize,
    /// 当前滚动偏移（从底部回退了多少行）。
    pub display_offset: usize,
    /// 光标位置（视口坐标）。
    ///
    /// 视口**上方**的光标（上滚之后）用 `line = -1` 表示 —— 与
    /// [`crate::Terminal::cursor_point`] 的返回值不同（那里是真实的负数），
    /// 但渲染层的判据是 `line < 0`，两者等价。视口下方的光标保留真实行号
    /// （≥ 屏高），渲染层同样按越界处理。
    pub cursor: Point,
    /// 光标所在格的字符。
    ///
    /// 与 Zed 的 `Content::cursor_char` 一样预先取好：画光标时不必再锁一次
    /// `Term` 去读那个 cell。
    pub cursor_char: char,
}

impl Content {
    /// 从 `Term` 取一份可见屏幕快照。
    ///
    /// 用 alacritty 的 `renderable_content().display_iter`：它从**视口顶行**开始
    /// 逐行给出可见 cell（正是渲染要的东西），顺序也是按行。注意它给的点是
    /// alacritty 的**绝对** `Line`（回滚区为负），我们用 `point_to_viewport`
    /// 换算成视口坐标，与 crate 内其它 API 的约定一致（见
    /// [`crate::grid::Point`]）。
    pub fn new<E: EventListener>(term: &Term<E>) -> Self {
        let renderable = term.renderable_content();
        let display_offset = renderable.display_offset;

        let mut cells = Vec::with_capacity(renderable.display_iter.size_hint().0);
        for indexed in renderable.display_iter {
            // `display_iter` 起于视口顶行，换算不会失败；真出现视口上方的行
            // （不该发生）时宁可少一格也不 panic —— 这是每次终端变化都要走的
            // 路径，为它引入 panic 不值得。
            let Some(point) = point_to_viewport(display_offset, indexed.point) else {
                continue;
            };
            cells.push(IndexedCell {
                point: Point::new(point.line as i32, point.column.0),
                cell: Cell::from_alacritty(indexed.cell),
            });
        }

        let cursor = point_to_viewport(display_offset, renderable.cursor.point)
            .map(|point| Point::new(point.line as i32, point.column.0))
            .unwrap_or_else(|| Point::new(-1, renderable.cursor.point.column.0));

        let grid = term.grid();
        Self {
            cells,
            columns: grid.columns(),
            screen_lines: grid.screen_lines(),
            display_offset,
            cursor,
            // 走 `Cell::character()` 而不是原始 `c`，让未写入的 '\0' 也变成空格
            // —— 画光标时那个字符是用**背景色**画在光标块上的，'\0' 会画出豆腐块。
            cursor_char: Cell::from_alacritty(&grid[renderable.cursor.point]).character(),
        }
    }
}
