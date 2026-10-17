//! 通用的查找栏：状态、消息、渲染。
//!
//! 栏只做三件事：收查询词、切选项、上/下一处。它**不**认识被搜索的视图 ——
//! 宿主把 [`SearchMessage`] 映射进自己的消息类型，再转手喂给
//! [`SearchableItem`](crate::SearchableItem)。

use std::rc::Rc;

use iced::widget::{Id, button, container, row, text, text_input};
use iced::{Element, Theme};

use crate::options::{SearchOption, SearchOptions};

/// 查找栏输入框的 widget id。
///
/// 打开查找栏要把焦点交给输入框（`iced::widget::operation::focus`）。做成常量
/// 是为了让宿主侧的 focus 调用与栏里的 `text_input.id(...)` 用同一个值 —— 两边
/// 各写一个字符串字面量，拼错了不报错，只是焦点永远落不上。
pub const SEARCH_INPUT_ID: &str = "aa_search_input";

/// 栏发出的消息。
///
/// 宿主把它映射成自己的 `Message`（iced 里就是 `Message::Search(...)`），
/// 再交给 [`SearchableItem`](crate::SearchableItem) 执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchMessage {
    /// 查询词变化（逐字触发，与 `text_input` 的 `on_input` 一致）。
    Query(String),
    /// 切换一个选项开关。
    Toggle(SearchOption),
    /// 跳到下一处命中。
    Next,
    /// 跳到上一处命中。
    Prev,
    /// 关闭查找栏。
    Close,
}

/// 栏自己持有的状态。
///
/// 只有查询词与选项开关。命中数与当前下标属于被搜索的视图，渲染时从
/// [`SearchStatus`] 现取 —— 抄一份进栏里就会有两处真相，环绕逻辑一改就漂移
/// （Zed 的划分也一样：查询词在栏里，命中在 `SearchableItem` 那边）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchBarState {
    query: String,
    options: SearchOptions,
}

impl SearchBarState {
    /// 打开查找栏时的初始状态。
    ///
    /// `options` 通常传被搜索视图的
    /// [`supported_options`](crate::SearchableItem::supported_options)：视图支持
    /// 的默认全开（Zed 的终端也是「正则」强制在开，但允许用户关掉）。
    pub fn new(query: impl Into<String>, options: SearchOptions) -> SearchBarState {
        SearchBarState {
            query: query.into(),
            options,
        }
    }

    /// 当前查询词。
    pub fn query(&self) -> &str {
        &self.query
    }

    /// 当前选项开关。
    pub fn options(&self) -> SearchOptions {
        self.options
    }

    /// 换查询词。
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
    }

    /// 反转一个选项开关。
    pub fn toggle(&mut self, option: SearchOption) {
        self.options = self.options.toggle(option);
    }
}

/// 被搜索视图回报的命中情况。
///
/// 宿主每次渲染前从 [`SearchableItem`](crate::SearchableItem) 现取。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchStatus {
    /// 命中总数。
    pub matches: usize,
    /// 当前命中下标；还没跳转过为 `None`。
    pub current: Option<usize>,
}

impl SearchStatus {
    /// 命中数文案。
    ///
    /// - 还没跳转过：`N 处`
    /// - 跳转过：`i/N`（只报总数的话，用户不知道自己在第几个，从末尾环绕回
    ///   开头时尤其看不出来）
    ///
    /// 0 处也照显示：这样用户能区分「词打对了但没匹配」和「还没开始搜」。
    /// 非法正则（例如刚敲下的单个 `[`）在底层会被当成清空，同样显示 0 处。
    pub fn label(&self) -> String {
        match self.current {
            Some(index) if self.matches > 0 => format!("{}/{}", index + 1, self.matches),
            _ => format!("{} 处", self.matches),
        }
    }
}

/// 渲染查找栏。
///
/// `on_message` 把栏的消息映射成宿主的消息类型，宿主只需要在 `Message` 里加
/// 一个 `Search(SearchMessage)` 变体。
///
/// `supported` 决定画哪几个选项开关 —— 传
/// [`supported_options`](crate::SearchableItem::supported_options) 的结果。
pub fn view<'a, Message>(
    state: &'a SearchBarState,
    status: SearchStatus,
    supported: SearchOptions,
    on_message: impl Fn(SearchMessage) -> Message + 'a,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    // 每个按钮都要一份闭包，而 `Fn` 闭包不一定 `Clone` —— 用 `Rc` 共享同一份，
    // 省得给调用方加 `Clone` 约束。
    let on_message: Rc<dyn Fn(SearchMessage) -> Message + 'a> = Rc::new(on_message);

    let input = text_input("搜索…", state.query())
        .id(Id::new(SEARCH_INPUT_ID))
        .on_input({
            let on_message = Rc::clone(&on_message);
            move |query| on_message(SearchMessage::Query(query))
        })
        .width(240.);

    let mut bar = row![input, text(status.label()).size(12)]
        .spacing(8)
        .align_y(iced::Center);

    // 只画视图支持的开关（Zed 的 `supported_options`）：画出来点了没反应的
    // 开关比没有更糟。
    for option in SearchOption::ALL {
        if supported.contains(option) {
            bar = bar.push(toggle_button(
                option,
                state.options().contains(option),
                Rc::clone(&on_message),
            ));
        }
    }

    bar = bar
        .push(action_button(
            "↑",
            SearchMessage::Prev,
            Rc::clone(&on_message),
        ))
        .push(action_button(
            "↓",
            SearchMessage::Next,
            Rc::clone(&on_message),
        ))
        .push(action_button("✕", SearchMessage::Close, on_message));

    container(bar)
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

/// 一个选项开关：开着时用主色底，关着时是纯文字按钮。
fn toggle_button<'a, Message>(
    option: SearchOption,
    active: bool,
    on_message: Rc<dyn Fn(SearchMessage) -> Message + 'a>,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    button(text(option.label()).size(12))
        .padding([2, 6])
        .style(move |theme: &Theme, status| {
            if active {
                button::primary(theme, status)
            } else {
                button::text(theme, status)
            }
        })
        .on_press(on_message(SearchMessage::Toggle(option)))
        .into()
}

/// 一个动作按钮（上/下一处、关闭）。
fn action_button<'a, Message>(
    label: &'a str,
    message: SearchMessage,
    on_message: Rc<dyn Fn(SearchMessage) -> Message + 'a>,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    button(text(label).size(12))
        .padding([2, 6])
        .style(button::text)
        .on_press(on_message(message))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Msg {
        Search(SearchMessage),
    }

    fn bar<'a>(
        state: &'a SearchBarState,
        status: SearchStatus,
        supported: SearchOptions,
    ) -> Element<'a, Msg> {
        view(state, status, supported, Msg::Search)
    }

    #[test]
    fn label_shows_the_total_before_and_the_index_after_navigating() {
        assert_eq!(
            SearchStatus {
                matches: 3,
                current: None
            }
            .label(),
            "3 处"
        );
        // 下标从 0 起，但给用户看的是从 1 起 —— 差一位正是这里最容易写错的。
        assert_eq!(
            SearchStatus {
                matches: 3,
                current: Some(0)
            }
            .label(),
            "1/3"
        );
        assert_eq!(
            SearchStatus {
                matches: 3,
                current: Some(2)
            }
            .label(),
            "3/3"
        );
    }

    #[test]
    fn zero_matches_still_shows_a_count() {
        // 区分「没匹配上」与「还没搜」。
        assert_eq!(SearchStatus::default().label(), "0 处");
        // 命中清空后下标可能还是旧值，这时不该显示 `4/0` 这种越界序号。
        assert_eq!(
            SearchStatus {
                matches: 0,
                current: Some(3)
            }
            .label(),
            "0 处"
        );
    }

    #[test]
    fn typing_into_the_box_reports_the_growing_query() {
        let state = SearchBarState::default();
        let mut ui =
            iced_test::simulator(bar(&state, SearchStatus::default(), SearchOptions::NONE));

        // 空值的输入框没法用文本选择器选中，按 Id 点它 —— 点击会顺带把焦点
        // 交给输入框，没有焦点 typewrite 不会被接收。
        assert!(ui.click(Id::new(SEARCH_INPUT_ID)).is_ok(), "应能点中查找框");
        ui.typewrite("abc");

        let lengths: Vec<usize> = ui
            .into_messages()
            .filter_map(|message| match message {
                Msg::Search(SearchMessage::Query(query)) => Some(query.len()),
                _ => None,
            })
            .collect();
        // on_input 逐字触发：terminal 那边每一跳都会重跑一次搜索，这里确认收到
        // 的是一串增长中的完整值而不是只有最后一次。
        assert_eq!(lengths, vec![1, 2, 3], "应逐字上报，实际 {lengths:?}");
    }

    #[test]
    fn the_box_shows_the_query_and_the_match_count() {
        let state = SearchBarState::new("needle", SearchOptions::NONE);
        let mut ui = iced_test::simulator(bar(
            &state,
            SearchStatus {
                matches: 3,
                current: None,
            },
            SearchOptions::NONE,
        ));

        // 命中数是普通文本，能被文本选择器选中 —— 这样就不用截屏核对数字。
        assert!(ui.find("3 处").is_ok(), "应显示命中数");
        // 输入框当前值同样可被选中（选择器匹配的是值而非 placeholder）。
        assert!(ui.find("needle").is_ok(), "输入框应显示查询词");
    }

    #[test]
    fn only_supported_options_get_a_toggle() {
        // 终端只支持正则：大小写与全词开关画出来点了没反应，不能出现。
        let state = SearchBarState::new("x", SearchOptions::NONE);
        let mut ui = iced_test::simulator(bar(
            &state,
            SearchStatus::default(),
            [SearchOption::Regex].into_iter().collect(),
        ));

        assert!(
            ui.find(SearchOption::Regex.label()).is_ok(),
            "支持的选项应有开关"
        );
        for unsupported in [SearchOption::CaseSensitive, SearchOption::WholeWord] {
            assert!(
                ui.find(unsupported.label()).is_err(),
                "{unsupported:?} 不该有开关"
            );
        }
    }

    #[test]
    fn clicking_a_toggle_reports_that_option() {
        let state = SearchBarState::new("x", SearchOptions::NONE);
        let mut ui = iced_test::simulator(bar(
            &state,
            SearchStatus::default(),
            [SearchOption::Regex].into_iter().collect(),
        ));

        assert!(
            ui.click(SearchOption::Regex.label()).is_ok(),
            "应能点中开关"
        );
        let messages: Vec<Msg> = ui.into_messages().collect();
        assert_eq!(
            messages,
            vec![Msg::Search(SearchMessage::Toggle(SearchOption::Regex))]
        );
    }

    #[test]
    fn the_action_buttons_report_next_prev_and_close() {
        let state = SearchBarState::new("x", SearchOptions::NONE);
        let mut ui =
            iced_test::simulator(bar(&state, SearchStatus::default(), SearchOptions::NONE));

        // 三个按钮各点一次，按点击顺序记录消息 —— 顺序写反（上/下互换）在这里
        // 就会露出来。
        for label in ["↑", "↓", "✕"] {
            assert!(ui.click(label).is_ok(), "应能点中 {label}");
        }
        let messages: Vec<Msg> = ui.into_messages().collect();
        assert_eq!(
            messages,
            vec![
                Msg::Search(SearchMessage::Prev),
                Msg::Search(SearchMessage::Next),
                Msg::Search(SearchMessage::Close),
            ]
        );
    }
}
