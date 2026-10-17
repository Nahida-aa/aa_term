//! 终端的搜索：命中列表、当前命中，以及上下跳转。
//!
//! 对应 alacritty 的 `term/search.rs`，但**不每次重扫**：alacritty 的
//! `search_next` 从 origin 出发重跑一遍正则，而我们把全部命中一次性收集存下来，
//! 跳转只是移动下标 —— 用户每敲一次回车都重扫整块 scrollback 太亏。
//!
//! 命中坐标存的是 alacritty 的**绝对坐标**（不随滚动变）；对外给渲染和跳转的
//! 一律换算成**视口坐标**（0 = 视口顶，越往下越大），换算口只有 [`range_of`]
//! 一处，别在别的地方再写一遍加减偏移。
//!
//! **命名对照**（这里 ↔ AAgent `packages/terminal`，逐条核对过）：
//!
//! | 这里 | AAgent | |
//! |---|---|---|
//! | `SearchState` | 无此类型 | 那边只有 `Terminal::matches` 公开字段（`src/pty/mod.rs:58`）；「当前第几处」根本不进终端，在搜索 UI 上 |
//! | `SearchState::matches` | `Terminal::matches` | 同名沿用 |
//! | `set_search` | `find_matches`（`src/grid/content/search.rs:13`）+ `update_matches` | 无同名方法：那边后台线程扫（`cx.background_spawn`）再把结果写回字段，我们同步扫、自己存 |
//! | `search_matches` | `alacritty::search_matches`（`src/alacritty/search.rs:33`） | 同名，语义不同：那边是**执行**搜索，这里是**读**已存的命中 |
//! | `search_next` / `search_prev` | `activate_match(index)`（`src/input/selection.rs:56`） | 无同名：那边只收下标、环绕由调用方算；名字取自 alacritty 的 `term/search.rs` |
//! | `search_match_count` / `search_current_index` | 无 | 那边直接读字段的 `len()` / 在搜索 UI 上 |
//! | `escape_pattern` | 无 | Zed 侧是 `terminal_view::regex_search_for_query` 里的 `regex::escape` |

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Point as AlacPoint};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};

use crate::buffer::Terminal;
use crate::grid::{Point, Range};

/// 搜索状态：全部命中 + 当前跳转到的下标。
///
/// 上游（Zed / AAgent）**没有**这个类型 —— 那是 `Terminal` 上一个公开的
/// `matches` 字段，加上直接读写它的方法。我们把它收成一个私有结构、藏在
/// `Terminal::search_state` 的锁后面：aa_term 的终端要跨线程共享（iced 这边没有
/// gpui 的 `Entity` 可以挂状态）。字段名沿用上游的 `matches`。
#[derive(Debug, Default)]
pub(crate) struct SearchState {
    /// 全部命中，alacritty 绝对坐标。
    matches: Vec<Match>,
    /// 当前命中在 `matches` 里的下标。
    ///
    /// `None` 表示**还没有开始跳转**（刚设完搜索词，或删空了词）。这样第一次
    /// 按回车会落在第一个命中上，而不是「第一个已经当前、于是被跳过」。
    current: Option<usize>,
}

/// 把一条 alacritty 绝对坐标的命中换算成视口范围。
///
/// `offset` 是当前 `display_offset`，由调用方一次性取出后传进来 —— 换算口只有
/// 这一处，且这个函数**不碰任何锁**。
fn range_of(hit: &Match, offset: i32) -> Range {
    // alacritty 的 `Match` 是 `RangeInclusive<Point>`，字段私有，取端点要用
    // start()/end()。
    let (start, end) = (hit.start(), hit.end());
    Range::new(
        Point::new(start.line.0 + offset, start.column.0),
        Point::new(end.line.0 + offset, end.column.0),
    )
}

impl Terminal {
    /// 设置搜索词，返回命中数量。
    ///
    /// 搜索范围是**整块可滚动内容**（`topmost_line..=bottommost_line`），不只是
    /// 可见屏 —— 这正是用户预期：往上翻也能看到命中高亮。范围取自 Zed 的
    /// `terminal/src/alacritty.rs` 的 `all_search_matches`。
    ///
    /// 设置会**重置跳转位置**（`current = None`）：换词之后还停在旧词的下标上
    /// 毫无意义。
    ///
    /// **与上游的差别**：AAgent 的对应物是 `find_matches`（返回 `Task`，扫在
    /// `cx.background_spawn` 里）+ `update_matches`（宿主把结果写回 `matches`
    /// 字段）。我们同步扫完就存好 —— 查找栏是逐字符触发的，等 scrollback 长到
    /// 扫描能感觉出来那一步，再把 `scan` 挪到后台。
    pub fn set_search(&self, query: &str) -> usize {
        let matches = self.scan(query);
        let count = matches.len();
        let mut state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        state.matches = matches;
        state.current = None;
        count
    }

    /// 清空搜索高亮。
    pub fn clear_search(&self) {
        let mut state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        state.matches.clear();
        state.current = None;
    }

    /// 全部命中（视口坐标）。
    ///
    /// 名字和 AAgent 的 `alacritty::search_matches` 撞了但语义不同：那个是**执行**
    /// 搜索（`src/alacritty/search.rs:33`），这里是**读**已存的命中。上游把结果放在
    /// `Terminal::matches` 公开字段里直接读，我们给了个 getter。
    pub fn search_matches(&self) -> Vec<Range> {
        let offset = self.scroll_offset() as i32;
        let state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .matches
            .iter()
            .map(|hit| range_of(hit, offset))
            .collect()
    }

    /// 命中总数。
    ///
    /// 查找栏每帧都要显示它，所以单开一个取值口：走 [`Self::search_matches`]
    /// 的话每帧都要把全部命中换算成视口坐标再收集一次（命中几千处时白白的
    /// 分配），而这里只要读一个长度。
    ///
    /// 上游无同名方法（那边读 `matches` 字段的 `len()`）。
    pub fn search_match_count(&self) -> usize {
        self.search_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .matches
            .len()
    }

    /// 当前跳转到的命中下标；还没开始跳转时为 `None`。
    ///
    /// 上游无同名方法：AAgent / Zed 的这个数不在终端上（搜索 UI 自己算，见
    /// [`SearchState`] 的说明）。
    pub fn search_current_index(&self) -> Option<usize> {
        self.search_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
    }

    /// 跳到下一处命中（环绕），并把它滚进视口。
    ///
    /// 名字取自 alacritty 的 `term/search.rs`（那边的 `search_next` 每次都从 origin
    /// 重跑正则）。上游无同名：AAgent 只有 `activate_match(index)`，方向与环绕由调用
    /// 方算。
    pub fn search_next(&self) {
        self.search_step(1);
    }

    /// 跳到上一处命中（环绕），并把它滚进视口。
    pub fn search_prev(&self) {
        self.search_step(-1);
    }

    /// 移动当前下标并按方向环绕，再把它滚进视口。
    fn search_step(&self, direction: isize) {
        let Some(index) = self.advance_current(direction) else {
            return;
        };
        self.scroll_to(index);
    }

    /// 移动当前命中下标并按方向环绕，返回新下标。
    fn advance_current(&self, direction: isize) -> Option<usize> {
        let mut state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
        let len = state.matches.len();
        if len == 0 {
            state.current = None;
            return None;
        }
        let next = match state.current {
            None if direction > 0 => 0,
            None => len - 1,
            // rem_euclid 处理负数：prev 从 0 出发要绕到 len-1。
            Some(index) => (index as isize + direction).rem_euclid(len as isize) as usize,
        };
        state.current = Some(next);
        Some(next)
    }

    /// 把第 `index` 个命中滚进视口。
    fn scroll_to(&self, index: usize) {
        // 先取行号、放掉搜索锁，再进 `scroll_to_line` 拿 term 锁 —— 全程不同时
        // 持两把锁，避免与其它路径形成加锁顺序环。
        let line = {
            let state = self.search_state.lock().unwrap_or_else(|e| e.into_inner());
            match state.matches.get(index) {
                Some(hit) => hit.start().line.0,
                None => return,
            }
        };
        self.scroll_to_line(line);
    }

    /// 让某个 alacritty 绝对行滚进视口。
    ///
    /// 换算：视口行号 = `line + offset`。`line` 越小越靠上，所以
    /// - 落在视口上方（行号 < 0）→ 抬到视口顶：`offset = -line`
    /// - 落在视口下方（行号 ≥ 屏高）→ 压到视口底：`offset = 屏高 - 1 - line`
    ///
    /// 最后夹到 `[0, history_size]`；已经在视口内、或夹完仍在边界（内容还没
    /// 生成）时不动。
    fn scroll_to_line(&self, line: i32) {
        // 走 `mutate_grid`：滚动与「重建可见内容快照」必须是一件事，否则跳到
        // 下一个命中之后画面还停在原处。
        self.mutate_grid(|term| {
            // 先把需要的数据拷出来，再改 offset —— 借用 `grid()` 期间不能改。
            let (screen, history, offset) = {
                let grid = term.grid();
                (
                    grid.screen_lines() as i32,
                    grid.history_size() as i32,
                    grid.display_offset() as i32,
                )
            };
            let row = line + offset;
            let desired = if row < 0 {
                -line
            } else if row >= screen {
                screen - 1 - line
            } else {
                // 已经在视口里，不必动。
                return;
            }
            .clamp(0, history);
            if desired != offset {
                term.grid_mut()
                    .scroll_display(Scroll::Delta(desired - offset));
            }
        });
    }

    /// 扫出全部命中（alacritty 绝对坐标）。
    ///
    /// 非法正则**不 panic**：用户敲键盘的每一步都可能构成一个临时非法的式子
    /// （例如刚输完 `[`），此时按「没有命中」处理。
    fn scan(&self, query: &str) -> Vec<Match> {
        if query.is_empty() {
            return Vec::new();
        }
        let Ok(mut regex) = RegexSearch::new(query) else {
            return Vec::new();
        };

        let term = self.term.lock();
        let start = AlacPoint::new(term.grid().topmost_line(), Column(0));
        let end = AlacPoint::new(term.grid().bottommost_line(), term.grid().last_column());
        // RegexIter 要 &mut（内部推进游标）；Item 就是 `Match`。
        RegexIter::new(start, end, Direction::Right, &term, &mut regex).collect()
    }
}

/// 正则的元字符。
///
/// 照抄 `regex-syntax` 的 `is_meta_character`（0.8 的 `lib.rs:262`）—— 它只转义
/// 这些字符，跟它一致省得两边对「什么该转义」的理解不同。注意 `#`/`&`/`-`/`~`
/// 也在内（`#` 在 `(?x)` 注释模式里有意义）。
fn is_regex_meta_character(character: char) -> bool {
    matches!(
        character,
        '\\' | '.'
            | '+'
            | '*'
            | '?'
            | '('
            | ')'
            | '|'
            | '['
            | ']'
            | '{'
            | '}'
            | '^'
            | '$'
            | '#'
            | '&'
            | '-'
            | '~'
    )
}

/// 把查询词转义成「按字面量匹配」的正则。
///
/// 关掉查找栏的「正则」开关时用它：底层（alacritty 的 `RegexSearch`）只吃正则
/// 字符串，所以字面量搜索得自己转义。Zed 的
/// `terminal_view::regex_search_for_query` 也是这一手（它用 `regex::escape`）。
///
/// 上游无同名函数：Zed 那处转义写在 `terminal_view` 里（不是 core），AAgent 的
/// `packages/terminal` 里没有。放在 core 是因为「字面量 vs 正则」是搜索语义，跟
/// 哪个前端无关。
pub fn escape_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if is_regex_meta_character(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::GridGeometry;
    use crate::selection::HighlightKind;
    use crate::session::window_size_from;

    /// 24 行 × 80 列的终端（800/10 × 480/20）。
    fn terminal() -> Terminal {
        let size = window_size_from(&GridGeometry::new(800., 480., 10., 20.));
        Terminal::for_test(size)
    }

    #[test]
    fn a_fresh_search_has_no_current_until_the_first_jump() {
        let mut t = terminal();
        t.feed(b"abc abc abc");
        assert_eq!(t.set_search("abc"), 3);
        // 关键：第一次跳转要落在**第一处**，而不是「第一处已当前、于是跳过它」。
        assert_eq!(t.search_current_index(), None);
    }

    #[test]
    fn next_walks_forward_and_wraps_around() {
        let mut t = terminal();
        t.feed(b"abc xx abc xx abc");
        t.set_search("abc");

        for expected in [0, 1, 2, 0] {
            t.search_next();
            assert_eq!(t.search_current_index(), Some(expected));
        }
    }

    #[test]
    fn prev_from_a_fresh_search_lands_on_the_last_match() {
        let mut t = terminal();
        t.feed(b"abc xx abc xx abc");
        t.set_search("abc");

        t.search_prev();
        assert_eq!(t.search_current_index(), Some(2), "prev 应绕到最后一处");
        t.search_prev();
        assert_eq!(t.search_current_index(), Some(1));
        t.search_prev();
        assert_eq!(t.search_current_index(), Some(0));
        t.search_prev();
        assert_eq!(t.search_current_index(), Some(2), "再 prev 应从 0 绕回末尾");
    }

    #[test]
    fn navigation_without_matches_stays_at_none() {
        let mut t = terminal();
        t.feed(b"abc");
        assert_eq!(t.set_search("zzz"), 0);

        t.search_next();
        assert_eq!(t.search_current_index(), None);
        t.search_prev();
        assert_eq!(t.search_current_index(), None);
    }

    /// 快照首行的文本（`Content::cells` 是行优先、每行 `columns` 格）。
    fn first_snapshot_row(t: &Terminal) -> String {
        let content = t.content();
        content
            .cells
            .iter()
            .take(content.columns)
            .map(|cell| cell.character())
            .collect()
    }

    #[test]
    fn jumping_scrolls_the_match_into_view() {
        let mut t = terminal();
        // 命中放在最上面，随后灌满一屏多的输出把它顶到视口上方。
        t.feed(b"needle\r\n");
        for i in 0..40 {
            t.feed(format!("line-{i:02}\r\n").as_bytes());
        }
        assert_eq!(t.scroll_offset(), 0, "刚写完应停在最新输出处");

        t.set_search("needle");
        // 命中此刻在视口上方。
        assert!(
            t.search_matches()[0].start().line < 0,
            "命中应先在视口上方: {:?}",
            t.search_matches()[0]
        );
        assert!(
            !first_snapshot_row(&t).starts_with("needle"),
            "跳转前快照首行不该是命中行"
        );

        t.search_next();
        assert_eq!(t.search_current_index(), Some(0));
        assert!(t.scroll_offset() > 0, "跳转应把视口向上滚");
        // 快照要跟着跳转走。它由 `mutate_grid` 重建，跳转路径绕开它的话
        // 画面会停在最新输出处 —— 上面读的都是网格，只有这条读快照。
        assert!(
            first_snapshot_row(&t).starts_with("needle"),
            "跳转后快照首行应是命中行，实际 {:?}",
            first_snapshot_row(&t)
        );
        let row = t.search_matches()[0].start().line;
        assert!(
            (0..24).contains(&row),
            "跳转后命中应落在视口内，实际行 {row}"
        );
    }

    #[test]
    fn the_current_match_is_highlighted_apart_from_the_others() {
        let mut t = terminal();
        t.feed(b"abc xx abc");
        t.set_search("abc");

        let before: Vec<_> = t
            .highlight_ranges()
            .into_iter()
            .map(|(_, kind)| kind)
            .collect();
        assert_eq!(
            before,
            vec![HighlightKind::SearchMatch; 2],
            "未跳转前都是普通命中"
        );

        t.search_next();
        let after: Vec<_> = t
            .highlight_ranges()
            .into_iter()
            .map(|(_, kind)| kind)
            .collect();
        assert_eq!(
            after,
            vec![
                HighlightKind::CurrentSearchMatch,
                HighlightKind::SearchMatch
            ],
            "跳转后当前命中应单独着色"
        );
    }

    #[test]
    fn match_count_is_readable_without_taking_the_matches() {
        // 查找栏每帧都要显示命中数，而这个取值口不该顺带换算坐标/分配 Vec。
        let mut t = terminal();
        t.feed(b"abc xx abc");
        assert_eq!(t.set_search("abc"), 2);
        assert_eq!(t.search_match_count(), 2);

        t.set_search("zzz");
        assert_eq!(t.search_match_count(), 0, "换词后应归零");
    }

    #[test]
    fn escaping_a_query_turns_it_into_a_literal_search() {
        // 关掉「正则」开关时，`a.c` 必须只命中字面量 `a.c`，不能命中 `abc` ——
        // 这就是那个开关的全部意义。
        assert_eq!(escape_pattern("a.c"), "a\\.c");
        assert_eq!(escape_pattern("a+b*c?"), "a\\+b\\*c\\?");
        // 元字符表照 `regex-syntax`：字母数字与下划线不转义，`#`/`&`/`-`/`~` 转。
        assert_eq!(escape_pattern("snake_case"), "snake_case");
        assert_eq!(escape_pattern("#1 - ~2"), "\\#1 \\- \\~2");
        // 中文等多字节字符逐 `char` 处理，不该被拆坏。
        assert_eq!(escape_pattern("中文(1)"), "中文\\(1\\)");
    }

    #[test]
    fn an_escaped_query_matches_the_literal_text_only() {
        // 上面测的是字符串形状；这里真的喂给引擎，确认转义后的模式能被解析、
        // 且语义就是「字面量」—— 转义写错（多转/少转）只有在这一层才暴露。
        let mut t = terminal();
        t.feed(b"abc\r\na.c\r\n");
        assert_eq!(t.set_search("a.c"), 2, "不转义时 `.` 通配，两处都命中");
        assert_eq!(
            t.set_search(&escape_pattern("a.c")),
            1,
            "转义后应只命中字面量 `a.c`"
        );
    }
}
