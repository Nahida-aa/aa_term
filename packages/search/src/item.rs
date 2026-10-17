//! 「可被查找的视图」契约 —— 查找栏只认这个 trait。

use crate::options::SearchOptions;

/// 查找栏能驱动的视图（终端、编辑器……）。
///
/// 对应 Zed 的 `workspace::SearchableItem`，但只留查找栏真正要用的那几件：
/// Zed 那个 trait 还背着工具栏、面包屑、替换、`select_matches` 等一整摊，等
/// `workspace` 真开工再往上搬 —— 现在照抄只会留下一堆没有消费方的空方法。
///
/// 为什么是「相对跳转」（[`next_match`](Self::next_match)）而不是 Zed 的
/// `activate_match(index)`：Zed 的查找栏自己保存命中列表，所以能按绝对下标跳；
/// 我们把命中留在被搜索的视图里（终端那边是一份加锁的列表），按绝对下标跳就得
/// 让视图暴露「跳到第 N 处」，而栏上只有上/下一处两个按钮 —— 相对跳转够用，也
/// 不用把命中列表抄一份给 UI。哪天要「跳到第 N 处」，在 `SearchableItem` 上
/// 加一个方法即可。
///
/// **方法命名对照**（Zed 的 `workspace::SearchableItem` ↔ 这里）：
///
/// | 这里 | Zed | |
/// |---|---|---|
/// | [`supported_options`](Self::supported_options) | `supported_options` | 同名 |
/// | [`active_match_index`](Self::active_match_index) | `active_match_index` | 同名（那边要 `window, cx`，这边只要 `&self`） |
/// | [`set_query`](Self::set_query) | `find_matches` + `update_matches` | 无同名：那边把「扫」与「写回」拆成两步（扫是异步 `Task`），这边一步做完 |
/// | [`next_match`](Self::next_match) / [`prev_match`](Self::prev_match) | `activate_match(index)` + `match_index_for_direction` | 无同名：那边按绝对下标跳，方向由调用方算 |
/// | [`match_count`](Self::match_count) | 无 | 那边栏里直接读命中列表的长度 |
pub trait SearchableItem {
    /// 换查询词，返回命中数。
    ///
    /// `options` 是栏上当前的选项开关。不支持某项的视图可以忽略它（终端底层
    /// 只吃正则，就只认 [`SearchOption::Regex`](crate::SearchOption::Regex)）。
    /// 空查询表示清空高亮。
    fn set_query(&mut self, query: &str, options: SearchOptions) -> usize;

    /// 跳到下一处命中；到末尾环绕回第一处。
    fn next_match(&mut self);

    /// 跳到上一处命中；到开头环绕回最后一处。
    fn prev_match(&mut self);

    /// 命中总数。
    fn match_count(&self) -> usize;

    /// 当前命中下标（从 0 起）；还没跳转过为 `None`。
    ///
    /// 栏据此显示「i/N」—— 别在 UI 侧自己推算下标，环绕逻辑一改就对不上。
    fn active_match_index(&self) -> Option<usize>;

    /// 这个视图支持哪些选项。
    ///
    /// 不支持的不画开关：画出来点了没反应，比没有更糟。
    fn supported_options(&self) -> SearchOptions;
}
