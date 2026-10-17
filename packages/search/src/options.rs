//! 查找栏上的选项开关。
//!
//! 对应 Zed `crates/search/src/search.rs` 的 `SearchOption` / `SearchOptions`
//! （那边用 `bitflags!` 声明）。这里手写位图：只有 3 个位，为此多引一个依赖不
//! 划算，语义与那边一致。

/// 查找栏上可以切换的选项。
///
/// **与上游的命名对照**：名字沿用 Zed 的同名枚举，但只搬了终端用得上的 3 个 ——
/// Zed 有 6 个（`WholeWord` / `CaseSensitive` / `IncludeIgnored` / `Regex` /
/// `OneMatchPerLine` / `Backwards`），其余都属于编辑器/项目级查找。而且**变体顺序
/// 与位序都和那边不同**（那边 `WholeWord = 0`、`CaseSensitive = 1`、`Regex = 3`）。
/// 位只在本 crate 内用、既不外传也不落盘，所以这里按「栏上从左到右」排。
///
/// 顺序即栏上按钮从左到右的顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchOption {
    /// 区分大小写。
    CaseSensitive,
    /// 全词匹配。
    WholeWord,
    /// 按正则表达式解释查询词。
    Regex,
}

impl SearchOption {
    /// 全部选项，按栏上的显示顺序。
    pub const ALL: [SearchOption; 3] = [Self::CaseSensitive, Self::WholeWord, Self::Regex];

    /// 按钮上的短标签。
    ///
    /// 用约定俗成的符号而不是文字（Zed 用图标，含义相同）：`Aa` 大小写、
    /// `ab` 全词、`.*` 正则，一条按钮放得下。
    ///
    /// **与上游同名、语义不同**：Zed 的 `SearchOption::label()` 是**悬停说明**文字
    /// （"Match Whole Words"），按钮面画的是图标（它另有 `icon() -> ui::IconName`）。
    /// 我们没有图标资源，`label()` 就是按钮面文字，悬停说明在
    /// [`description`](Self::description)。
    pub fn label(self) -> &'static str {
        match self {
            SearchOption::CaseSensitive => "Aa",
            SearchOption::WholeWord => "ab",
            SearchOption::Regex => ".*",
        }
    }

    /// 悬停提示用的说明文字。
    ///
    /// 对应 Zed 的 `label()`（那边没有单独的 description）。
    pub fn description(self) -> &'static str {
        match self {
            SearchOption::CaseSensitive => "区分大小写",
            SearchOption::WholeWord => "全词匹配",
            SearchOption::Regex => "正则表达式",
        }
    }

    /// 这一项在位图里占的位。
    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// 选项集合 —— 哪些开关开着。
///
/// 命名与语义沿用 Zed 的同名类型（那边是 `bitflags!` 声明，常量名
/// `CASE_SENSITIVE` / `WHOLE_WORD` / `REGEX`、`NONE`）。我们手写位图：
/// `with` / `without` 对应 bitflags 的 `insert` / `remove`，`contains` / `toggle`
/// / `is_empty` 与那边同名。
///
/// 位图而非 `Vec`：判定「这项开没开」是渲染路径上的热操作（每个选项每帧一
/// 次），而且集合本身要能 `Copy` 地在消息里传。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct SearchOptions(u8);

impl SearchOptions {
    /// 一项都没开。
    pub const NONE: SearchOptions = SearchOptions(0);

    /// 这一项开着吗。
    pub fn contains(self, option: SearchOption) -> bool {
        self.0 & option.bit() != 0
    }

    /// 打开这一项。
    pub fn with(self, option: SearchOption) -> SearchOptions {
        SearchOptions(self.0 | option.bit())
    }

    /// 关掉这一项。
    pub fn without(self, option: SearchOption) -> SearchOptions {
        SearchOptions(self.0 & !option.bit())
    }

    /// 反转这一项。
    pub fn toggle(self, option: SearchOption) -> SearchOptions {
        if self.contains(option) {
            self.without(option)
        } else {
            self.with(option)
        }
    }

    /// 一项都没开。
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl FromIterator<SearchOption> for SearchOptions {
    fn from_iter<T: IntoIterator<Item = SearchOption>>(options: T) -> SearchOptions {
        options
            .into_iter()
            .fold(SearchOptions::NONE, SearchOptions::with)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_labels_are_unique_and_non_empty() {
        // 按钮文字重复的话，用户分不清哪个开关是哪个（`ab` 与 `Aa` 也不会被
        // 误认成彼此 —— 这正是选符号时要注意的）。
        let labels: Vec<&str> = SearchOption::ALL.iter().map(|o| o.label()).collect();
        assert!(labels.iter().all(|label| !label.is_empty()));
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "标签重复: {labels:?}");
    }

    #[test]
    fn toggling_is_its_own_inverse() {
        // 点两下开关必须回到原状，否则栏上的高亮会和实际选项漂移。
        let mut options = SearchOptions::NONE;
        for option in SearchOption::ALL {
            options = options.toggle(option);
            assert!(options.contains(option));
            options = options.toggle(option);
            assert!(!options.contains(option), "{option:?} 关不干净");
        }
        assert_eq!(options, SearchOptions::NONE);
    }

    #[test]
    fn options_are_independent_bits() {
        // 位撞车会让「开正则」顺带打开大小写，且只在特定组合下暴露。
        let mut options = SearchOptions::NONE;
        for option in SearchOption::ALL {
            options = options.with(option);
        }
        for option in SearchOption::ALL {
            assert!(options.contains(option));
            // 去掉一项，其余三项不受影响。
            let without = options.without(option);
            assert!(!without.contains(option));
            for other in SearchOption::ALL.iter().filter(|other| **other != option) {
                assert!(without.contains(*other), "{other:?} 被误关");
            }
        }
    }

    #[test]
    fn none_is_empty_and_collect_fills_from_iterators() {
        // 「视图支持哪些选项」就是用它收集出来的（见 `TerminalSearch`）。
        assert!(SearchOptions::NONE.is_empty());
        let supported: SearchOptions = [SearchOption::Regex].into_iter().collect();
        assert!(supported.contains(SearchOption::Regex));
        assert!(!supported.is_empty());
        assert!(!supported.contains(SearchOption::WholeWord));
    }
}
