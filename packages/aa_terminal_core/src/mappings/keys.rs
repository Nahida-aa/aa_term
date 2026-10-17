//! 按键编码 —— 把 iced 的键盘事件变成要发给 shell 的字节。
//!
//! 对应 Zed `terminal/src/mappings/keys.rs`：那边输入 `gpui::Keystroke`，
//! 这边输入 iced 的 `Key` + `Modifiers`，映射表本身一致。
//!
//! 逻辑逐条对齐 AAgent/Zed 的 `terminal/src/mappings/keys.rs`（那份又是从
//! alacritty 源码读来的）。这里把 `gpui::Keystroke` 换成 iced 的
//! `keyboard::Key` + `Modifiers`，映射表本身不变。
//!
//! ## 分成三层
//!
//! 1. **手动映射表**：tab / enter / backspace / 方向键 / F1-F20 / insert 等
//!    特殊键，按 (键, 修饰键) 精确查表。
//! 2. **带修饰键的标准键**：`ESC [ 1 ; {modifier} ~`（xterm 的 PC-Style
//!    Function Keys 格式），`modifier` 由 shift/alt/ctrl 位运算得出 **+1**。
//! 3. **Caret notation**：`Ctrl+字母` → 控制字符 0x01..0x1A。
//!
//! ## 输入顺序
//!
//! 必须按这个顺序判。反过来会出错：例如 `Ctrl+I` 同时也是 tab（ASCII 0x09），
//! 若先判 caret notation 会发出 `\x1b[1;5I`，而程序期望的是制表符。

use iced_core::keyboard::key::Named;
use iced_core::keyboard::{Key, Modifiers};
use std::borrow::Cow;

/// 从 iced 的修饰键推出终端语义上的修饰组合。
///
/// `platform`（"logo" 键）不参与 —— 终端里它是快捷键，不该转发。
#[derive(Debug, PartialEq, Eq)]
pub enum TerminalModifiers {
    /// 无修饰。
    None,
    /// Alt。
    Alt,
    /// Ctrl。
    Ctrl,
    /// Shift。
    Shift,
    /// Ctrl+Shift。
    CtrlShift,
    /// 组合超过终端的表达能力（如 Ctrl+Alt+Shift），通常交给应用处理。
    Other,
}

impl TerminalModifiers {
    fn from(modifiers: &Modifiers) -> Self {
        match (modifiers.alt(), modifiers.control(), modifiers.shift()) {
            (false, false, false) => Self::None,
            (true, false, false) => Self::Alt,
            (false, true, false) => Self::Ctrl,
            (false, false, true) => Self::Shift,
            (false, true, true) => Self::CtrlShift,
            _ => Self::Other,
        }
    }

    fn any(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// `Ctrl+字母` 得到的控制字符。
///
/// Ctrl 会把字母减去 0x40：`a`(0x61) → 0x01，`z`(0x7a) → 0x1A。
fn control_code(character: char) -> Option<char> {
    let lower = character.to_ascii_lowercase();
    if !lower.is_ascii_lowercase() {
        return None;
    }
    char::from_u32((lower as u32) - 96)
}

/// 基础特殊键的编码。
///
/// 这些键在 iced 下有**两种**表示：`Key::Named(Named::Enter)` 与
/// `Key::Character("enter")`。Zed 的 gpui 只有后者（`Keystroke` 用字符串键名），
/// 直接照抄会漏掉 Named 形态 —— 症状是回车、退格、空格在真实窗口里完全失效，
/// 而单测只喂 `Character` 就发现不了。所以这里两种形态都接。
fn special_key_sequence(key: &Key, modifiers: &TerminalModifiers) -> Option<&'static str> {
    match (key.as_ref(), modifiers) {
        // 字符形态与命名键形态并列，任一命中即可。
        (Key::Character("enter") | Key::Named(Named::Enter), TerminalModifiers::None) => Some("\r"),
        // Shift+Enter 是换行：很多程序靠它区分「提交」和「换行」。
        (Key::Character("enter") | Key::Named(Named::Enter), TerminalModifiers::Shift) => {
            Some("\n")
        }
        (Key::Character("enter") | Key::Named(Named::Enter), TerminalModifiers::Alt) => {
            Some("\x1b\r")
        }
        (Key::Character("tab") | Key::Named(Named::Tab), TerminalModifiers::None) => Some("\t"),
        // Shift+Tab 是 backwards tab，很多 TUI 用它切到上一个。
        (Key::Character("tab") | Key::Named(Named::Tab), TerminalModifiers::Shift) => {
            Some("\x1b[Z")
        }
        (Key::Character("escape") | Key::Named(Named::Escape), TerminalModifiers::None) => {
            Some("\x1b")
        }
        (Key::Character("backspace") | Key::Named(Named::Backspace), TerminalModifiers::None) => {
            Some("\x7f")
        }
        (Key::Character("backspace") | Key::Named(Named::Backspace), TerminalModifiers::Ctrl) => {
            Some("\x08")
        }
        (Key::Character("backspace") | Key::Named(Named::Backspace), TerminalModifiers::Alt) => {
            Some("\x1b\x7f")
        }
        // 注意 iced 送来的是字符 " " 而不是字面量 "space"；Zed 的 gpui 用的是
        // "space"。两套命名不同，照抄字符串会漏掉这个组合。
        // 无修饰的空格就是空格本身；Ctrl+Space 才是 NUL。
        (Key::Character(" ") | Key::Named(Named::Space), TerminalModifiers::None) => Some(" "),
        (Key::Character(" ") | Key::Named(Named::Space), TerminalModifiers::Ctrl) => Some("\x00"),
        _ => None,
    }
}

/// xterm 的 modifier code：
///
/// ```text
/// 2 = Shift, 3 = Alt, 4 = Shift+Alt, 5 = Ctrl, 6 = Shift+Ctrl, 7 = Alt+Ctrl,
/// 8 = Shift+Alt+Ctrl
/// ```
///
/// 注意它是**位运算结果 +1**，所以「无修饰」得到 1。
fn modifier_code(modifiers: &Modifiers) -> u32 {
    let mut code = 0;
    if modifiers.shift() {
        code |= 1;
    }
    if modifiers.alt() {
        code |= 1 << 1;
    }
    if modifiers.control() {
        code |= 1 << 2;
    }
    code + 1
}

/// 把一次按键编码成要发给终端的字节。
///
/// `None` 表示这个按键不该转发给 shell —— 由应用层自行处理（通常是快捷键）。
pub fn encode(
    key: &Key,
    modifiers: &Modifiers,
    // terminal 是否处于 application cursor 模式（DECSET `DECCKM`）。
    // 该模式下方向键编码不同，判错的话 vim/less 会发出错误的方向序列。
    app_cursor: bool,
    // macOS 上 option 是否当 meta 用。非 macOS 恒为 true。
    option_as_meta: bool,
) -> Option<Cow<'static, str>> {
    let terminal_modifiers = TerminalModifiers::from(modifiers);

    // --- 第一层：手动查表 ------------------------------------------------
    if let Some(sequence) = special_key_sequence(key, &terminal_modifiers) {
        return Some(Cow::Borrowed(sequence));
    }

    if let Key::Named(named) = key {
        // 方向键在 application cursor 模式下用 `ESC O x` 而不是 `ESC [ x`，
        // 这是 DECCKM 的差别。
        let cursor = |normal: &'static str, application: &'static str| {
            if app_cursor { application } else { normal }
        };
        let manual: Option<&'static str> = match (named, &terminal_modifiers) {
            (Named::ArrowUp, TerminalModifiers::None) => Some(cursor("\x1b[A", "\x1bOA")),
            (Named::ArrowDown, TerminalModifiers::None) => Some(cursor("\x1b[B", "\x1bOB")),
            (Named::ArrowRight, TerminalModifiers::None) => Some(cursor("\x1b[C", "\x1bOC")),
            (Named::ArrowLeft, TerminalModifiers::None) => Some(cursor("\x1b[D", "\x1bOD")),
            (Named::Home, TerminalModifiers::None) => Some(cursor("\x1b[H", "\x1bOH")),
            (Named::End, TerminalModifiers::None) => Some(cursor("\x1b[F", "\x1bOF")),
            (Named::Insert, TerminalModifiers::None) => Some("\x1b[2~"),
            (Named::Delete, TerminalModifiers::None) => Some("\x1b[3~"),
            (Named::PageUp, TerminalModifiers::None) => Some("\x1b[5~"),
            (Named::PageDown, TerminalModifiers::None) => Some("\x1b[6~"),
            // Shift+Tab 是 backwards tab（ESC [ Z）。必须放在**命名键**分支，
            // 字符分支匹配不到 Key::Named。
            (Named::Tab, TerminalModifiers::Shift) => Some("\x1b[Z"),
            _ => None,
        };
        if let Some(sequence) = manual {
            return Some(Cow::Borrowed(sequence));
        }
    }

    // --- 第二层：带修饰键的标准键 ---------------------------------------
    if terminal_modifiers.any()
        && let Key::Named(named) = key
    {
        let code = modifier_code(modifiers);
        let encoded = match named {
            Named::Home => format!("\x1b[1;{code}H"),
            Named::End => format!("\x1b[1;{code}F"),
            Named::Insert => format!("\x1b[2;{code}~"),
            Named::Delete => format!("\x1b[3;{code}~"),
            Named::PageUp => format!("\x1b[5;{code}~"),
            Named::PageDown => format!("\x1b[6;{code}~"),
            Named::ArrowUp => format!("\x1b[1;{code}A"),
            Named::ArrowDown => format!("\x1b[1;{code}B"),
            Named::ArrowRight => format!("\x1b[1;{code}C"),
            Named::ArrowLeft => format!("\x1b[1;{code}D"),
            _ => String::new(),
        };
        if !encoded.is_empty() {
            return Some(Cow::Owned(encoded));
        }
    }

    // --- 第三层：Alt + 单字符 -------------------------------------------
    if key.as_ref() == Key::Unidentified {
        return None;
    }
    if let Key::Character(text) = key {
        let character = text.chars().next();
        if let Some(character) = character {
            // Alt 会给单字符加一个 ESC 前缀。macOS 上 option 未必是 meta，
            // 所以加一个开关。
            if (!cfg!(target_os = "macos") || option_as_meta)
                && modifiers.alt()
                && text.is_ascii()
                && text.len() == 1
            {
                if terminal_modifiers == TerminalModifiers::Alt {
                    return Some(Cow::Owned(format!("\x1b{character}")));
                } else if modifiers.shift() {
                    return Some(Cow::Owned(format!(
                        "\x1b{}",
                        character.to_ascii_uppercase()
                    )));
                } else if modifiers.control()
                    && let Some(code) = control_code(character)
                {
                    return Some(Cow::Owned(format!("\x1b{code}")));
                }
            }

            // Caret notation：Ctrl+字母 → 控制字符
            if modifiers.control()
                && !modifiers.alt()
                && text.len() == 1
                && let Some(code) = control_code(character)
            {
                return Some(Cow::Owned(code.to_string()));
            }

            // 普通字符：有修饰键时不发（交给应用），无修饰键时原样发。
            if !terminal_modifiers.any() {
                return Some(Cow::Owned(text.as_str().to_owned()));
            }
        }
    }

    None
}

/// 应用级快捷键 —— 必须在编码成终端字节**之前**拦掉。
///
/// 这些组合若当普通键编码，终端既会响铃/发控制码，用户又没达到目的
/// （比如 `Ctrl+Shift+C` 想复制，结果往 shell 发了 `\x03` 中断）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppShortcut {
    /// 复制选区到剪贴板。
    Copy,
    /// 从剪贴板粘贴。
    Paste,
    /// 打开（或重新聚焦）搜索框。
    OpenSearch,
    /// 视口上滚一行（Shift+↑）。
    ScrollLineUp,
    /// 视口下滚一行（Shift+↓）。
    ScrollLineDown,
    /// 视口上翻一屏（Shift+PageUp）。
    ScrollPageUp,
    /// 视口下翻一屏（Shift+PageDown）。
    ScrollPageDown,
    /// 翻到回滚区最顶端（Shift+Home）。
    ScrollToTop,
    /// 回到最新输出（Shift+End）。
    ScrollToBottom,
    /// 开关工作区侧边栏（Ctrl+Alt+J）。
    ///
    /// 名字与键位都照搬 Zed 的 `multi_workspace::ToggleWorkspaceSidebar`（绑在
    /// `assets/keymaps/default-linux.json`，`ctrl-alt-j`）。**位置不同**：上游那条
    /// 动作定义在 workspace/sidebar 那侧，这里跟其余应用级快捷键一样放在 core 的
    /// 判定表里 —— 一处判定、带测试，app 里不必再写一份（否则迟早不一致）。
    ToggleWorkspaceSidebar,
}

/// 判定一次按键是否应用级快捷键。
///
/// ## 为什么一律要求 Shift
///
/// 裸 `Ctrl+C` / `Ctrl+V` / `Ctrl+F` 是终端自己的：`Ctrl+C` 是 SIGINT，
/// `Ctrl+V` 在 readline 里是 quoted-insert，`Ctrl+F` 是光标前移。抢过来会
/// 破坏 shell 习惯。Zed 的 `pane::DeploySearch` 同样用 `ctrl-shift-f`。
///
/// 用 `eq_ignore_ascii_case` 是因为按下 Shift 时 iced 送来的字符是大写
/// （`Key::Character("C")`），大小写各写一个分支容易漏。
pub fn app_shortcut(key: &Key, modifiers: Modifiers) -> Option<AppShortcut> {
    // 视口翻页：与 Zed 的 `terminal::ScrollLineUp` / `ScrollPageUp` / `ScrollToTop`
    // 那组绑定一致，只需 Shift。必须要求 Shift —— 裸 `PageUp` / `↑` 是终端自己的，
    // 要编码成序列发给程序（这是方向键与翻页键的差别）。多带 Ctrl/Alt 的组合
    // 不抢，留给程序。
    if modifiers.shift()
        && !modifiers.control()
        && !modifiers.alt()
        && let Key::Named(named) = key
    {
        return match named {
            Named::PageUp => Some(AppShortcut::ScrollPageUp),
            Named::PageDown => Some(AppShortcut::ScrollPageDown),
            Named::Home => Some(AppShortcut::ScrollToTop),
            Named::End => Some(AppShortcut::ScrollToBottom),
            Named::ArrowUp => Some(AppShortcut::ScrollLineUp),
            Named::ArrowDown => Some(AppShortcut::ScrollLineDown),
            _ => None,
        };
    }
    // 侧边栏开关：Zed 绑在 `ctrl-alt-j`（无 Shift），照搬。
    //
    // 代价是裸 `Ctrl+Alt+J` 从此不再发给程序（xterm 那套会编码成 `ESC` + `\x0a`）。
    // 这正是 Zed 的选择：侧边栏是应用级面板，快捷键得先于终端。
    if modifiers.control()
        && modifiers.alt()
        && !modifiers.shift()
        && let Key::Character(character) = key
        && character.eq_ignore_ascii_case("j")
    {
        return Some(AppShortcut::ToggleWorkspaceSidebar);
    }
    if !(modifiers.control() && modifiers.shift()) {
        return None;
    }
    let Key::Character(c) = key else {
        return None;
    };
    if c.eq_ignore_ascii_case("c") {
        Some(AppShortcut::Copy)
    } else if c.eq_ignore_ascii_case("v") {
        Some(AppShortcut::Paste)
    } else if c.eq_ignore_ascii_case("f") {
        Some(AppShortcut::OpenSearch)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(key: &str) -> (Key, Modifiers) {
        (Key::Character(key.into()), Modifiers::default())
    }

    /// 按 shift/alt/ctrl 三个布尔值构造 `Modifiers`。
    ///
    /// iced_core 的 `Modifiers` 是 bitflags 封装的，没有公开的 `BitOr`，
    /// 组合修饰只能走这里；直接写 `Modifiers::CTRL | Modifiers::SHIFT` 编不过。
    fn mods(shift: bool, alt: bool, ctrl: bool) -> Modifiers {
        let mut modifiers = Modifiers::default();
        if shift {
            modifiers.insert(Modifiers::SHIFT);
        }
        if alt {
            modifiers.insert(Modifiers::ALT);
        }
        if ctrl {
            modifiers.insert(Modifiers::CTRL);
        }
        modifiers
    }

    fn encode_str(key: &Key, modifiers: &Modifiers) -> Option<String> {
        encode(key, modifiers, false, true).map(|s| s.to_string())
    }

    #[test]
    fn plain_characters_pass_through() {
        assert_eq!(
            encode_str(&plain("a").0, &plain("a").1).as_deref(),
            Some("a")
        );
        assert_eq!(
            encode_str(&plain("Z").0, &plain("Z").1).as_deref(),
            Some("Z")
        );
        // 多字符输入（如中文输入法）原样转发
        assert_eq!(
            encode_str(&plain("中").0, &plain("中").1).as_deref(),
            Some("中")
        );
    }

    #[test]
    fn app_shortcuts_require_shift() {
        // 大写是 Shift 按住时 iced 送来的形态，必须照样识别
        let ctrl_shift = mods(true, false, true);
        assert_eq!(
            app_shortcut(&Key::Character("C".into()), ctrl_shift),
            Some(AppShortcut::Copy)
        );
        assert_eq!(
            app_shortcut(&Key::Character("V".into()), ctrl_shift),
            Some(AppShortcut::Paste)
        );
        assert_eq!(
            app_shortcut(&Key::Character("F".into()), ctrl_shift),
            Some(AppShortcut::OpenSearch)
        );
    }

    #[test]
    fn the_sidebar_shortcut_is_ctrl_alt_j_without_shift() {
        // 与 Zed 的 `multi_workspace::ToggleWorkspaceSidebar` 同键位。
        assert_eq!(
            app_shortcut(&Key::Character("j".into()), mods(false, true, true)),
            Some(AppShortcut::ToggleWorkspaceSidebar)
        );
        assert_eq!(
            app_shortcut(&Key::Character("J".into()), mods(true, true, true)),
            None,
            "带 Shift 的不是这条快捷键（上游也没绑）"
        );
        assert_eq!(
            app_shortcut(&Key::Character("j".into()), mods(false, false, true)),
            None,
            "只有 Ctrl 时归终端（Ctrl+J 是换行）"
        );
        assert_eq!(
            app_shortcut(&Key::Character("j".into()), mods(false, true, false)),
            None,
            "只有 Alt 时归终端（Meta 前缀）"
        );
    }

    #[test]
    fn shifted_named_keys_are_scroll_shortcuts() {
        let shift = mods(true, false, false);
        for (key, expected) in [
            (Named::PageUp, AppShortcut::ScrollPageUp),
            (Named::PageDown, AppShortcut::ScrollPageDown),
            (Named::ArrowUp, AppShortcut::ScrollLineUp),
            (Named::ArrowDown, AppShortcut::ScrollLineDown),
            (Named::Home, AppShortcut::ScrollToTop),
            (Named::End, AppShortcut::ScrollToBottom),
        ] {
            assert_eq!(
                app_shortcut(&Key::Named(key), shift),
                Some(expected),
                "{key:?} + Shift"
            );
        }
    }

    #[test]
    fn bare_paging_keys_are_left_to_the_program() {
        // 裸 PageUp / ↑ / Home / End 都是程序要的键（less、vim 自己翻页），
        // 抢过来会让它们彻底收不到。
        for key in [
            Named::PageUp,
            Named::PageDown,
            Named::ArrowUp,
            Named::ArrowDown,
            Named::Home,
            Named::End,
        ] {
            assert_eq!(
                app_shortcut(&Key::Named(key), Modifiers::default()),
                None,
                "{key:?} 不带 Shift"
            );
        }
    }

    #[test]
    fn scroll_shortcuts_do_not_steal_control_combinations() {
        let ctrl_shift = mods(true, false, true);
        assert_eq!(
            app_shortcut(&Key::Named(Named::PageUp), ctrl_shift),
            None,
            "Ctrl+Shift+PageUp 不归终端管"
        );
        assert_eq!(
            app_shortcut(&Key::Named(Named::ArrowUp), mods(true, true, false)),
            None,
            "Alt+Shift+↑ 不归终端管"
        );
    }

    #[test]
    fn bare_control_letters_are_not_app_shortcuts() {
        // 裸 Ctrl+C 必须留给 shell 当 SIGINT —— 若被当成复制抢走，用户就
        // 没法用么 Ctrl+C 中断程序了。
        let ctrl = mods(false, false, true);
        assert_eq!(app_shortcut(&Key::Character("c".into()), ctrl), None);
        assert_eq!(app_shortcut(&Key::Character("v".into()), ctrl), None);
        assert_eq!(app_shortcut(&Key::Character("f".into()), ctrl), None);
    }

    #[test]
    fn other_shifted_combos_are_not_app_shortcuts() {
        let ctrl_shift = mods(true, false, true);
        // z 没有绑定，应原样落到 shell
        assert_eq!(app_shortcut(&Key::Character("z".into()), ctrl_shift), None);
        // 非字符键也不该命中
        assert_eq!(app_shortcut(&Key::Named(Named::Escape), ctrl_shift), None);
    }

    #[test]
    fn control_letters_become_control_codes() {
        let (key, modifiers) = (Key::Character("c".into()), Modifiers::CTRL);
        // Ctrl+C 是 SIGINT，是终端最常用的组合，必须精确
        assert_eq!(encode_str(&key, &modifiers).as_deref(), Some("\x03"));
    }

    #[test]
    fn every_letter_has_a_control_code() {
        // 穷举 26 个字母，验证公式 `c - 96` 覆盖整个 0x01..=0x1A 区间
        for (index, letter) in ('a'..='z').enumerate() {
            let key = Key::Character(letter.to_string().into());
            let expected = char::from_u32(index as u32 + 1).unwrap();
            assert_eq!(
                encode_str(&key, &Modifiers::CTRL).as_deref(),
                Some(expected.to_string().as_str())
            );
        }
    }

    #[test]
    fn control_with_uppercase_letter_still_works() {
        // Ctrl+Shift+C：Zed 也映射到 \x03。用户按到大写字母时不能没反应。
        let modifiers = mods(false, false, true);
        let key = Key::Character("C".into());
        assert_eq!(encode_str(&key, &modifiers).as_deref(), Some("\x03"));
    }

    #[test]
    fn control_space_is_nul() {
        // Ctrl+Space → \x00，很多程序用它清除选中
        let key = Key::Character(" ".into());
        assert_eq!(encode_str(&key, &Modifiers::CTRL).as_deref(), Some("\x00"));
    }

    #[test]
    fn special_keys_use_their_documented_sequences() {
        let cases = [
            (Named::Enter, Modifiers::default(), "\r"),
            (Named::Tab, Modifiers::default(), "\t"),
            (Named::Backspace, Modifiers::default(), "\x7f"),
            (Named::Escape, Modifiers::default(), "\x1b"),
            (Named::Space, Modifiers::default(), " "),
            (Named::ArrowUp, Modifiers::default(), "\x1b[A"),
            (Named::ArrowLeft, Modifiers::default(), "\x1b[D"),
            (Named::Home, Modifiers::default(), "\x1b[H"),
            (Named::End, Modifiers::default(), "\x1b[F"),
            (Named::Insert, Modifiers::default(), "\x1b[2~"),
            (Named::Delete, Modifiers::default(), "\x1b[3~"),
            (Named::PageUp, Modifiers::default(), "\x1b[5~"),
            (Named::PageDown, Modifiers::default(), "\x1b[6~"),
        ];
        for (named, modifiers, expected) in cases {
            let key = Key::Named(named);
            assert_eq!(
                encode_str(&key, &modifiers).as_deref(),
                Some(expected),
                "{named:?}"
            );
        }
    }

    #[test]
    fn app_cursor_mode_changes_arrow_encoding() {
        // DECCKM 模式下方向键是 ESC O x。判错会让 vim 收到错方向序列。
        let key = Key::Named(Named::ArrowUp);
        let normal = encode(&key, &Modifiers::default(), false, true);
        let application = encode(&key, &Modifiers::default(), true, true);
        assert_eq!(normal.map(|s| s.to_string()).as_deref(), Some("\x1b[A"));
        assert_eq!(
            application.map(|s| s.to_string()).as_deref(),
            Some("\x1bOA")
        );
    }

    #[test]
    fn shift_tab_is_backwards_tab() {
        // `ESC [ Z`：很多 TUI 用它做「切到上一个」。
        let key = Key::Named(Named::Tab);
        let sequence = encode(&key, &Modifiers::SHIFT, false, true);
        assert_eq!(sequence.map(|s| s.to_string()).as_deref(), Some("\x1b[Z"));
    }

    #[test]
    fn modified_navigation_keys_use_the_xterm_format() {
        let cases = [
            (Named::Home, "\x1b[1;2H"),      // Shift+Home
            (Named::End, "\x1b[1;3F"),       // Alt+End
            (Named::ArrowUp, "\x1b[1;5A"),   // Ctrl+Up
            (Named::ArrowDown, "\x1b[1;6B"), // Ctrl+Shift+Down
        ];
        for (named, expected) in cases {
            let modifiers = match expected {
                "\x1b[1;2H" => mods(true, false, false),
                "\x1b[1;3F" => mods(false, true, false),
                "\x1b[1;5A" => mods(false, false, true),
                _ => mods(true, false, true),
            };
            let key = Key::Named(named);
            assert_eq!(
                encode_str(&key, &modifiers).as_deref(),
                Some(expected),
                "{named:?}"
            );
        }
    }

    #[test]
    fn modifier_code_follows_the_xterm_table() {
        // 1 = 无修饰，2 = Shift，3 = Alt，4 = Shift+Alt，5 = Ctrl，6 = Shift+Ctrl，
        // 7 = Alt+Ctrl，8 = Shift+Alt+Ctrl。逐位验证，别靠记忆。
        assert_eq!(modifier_code(&Modifiers::default()), 1);
        assert_eq!(modifier_code(&Modifiers::SHIFT), 2);
        assert_eq!(modifier_code(&Modifiers::ALT), 3);
        assert_eq!(modifier_code(&mods(true, true, false)), 4);
        assert_eq!(modifier_code(&Modifiers::CTRL), 5);
        assert_eq!(modifier_code(&mods(true, false, true)), 6);
        assert_eq!(modifier_code(&mods(false, true, true)), 7);
        assert_eq!(modifier_code(&mods(true, true, true)), 8);
    }

    #[test]
    fn alt_prefixes_single_ascii_characters() {
        let key = Key::Character("x".into());
        assert_eq!(encode_str(&key, &Modifiers::ALT).as_deref(), Some("\x1bx"));
    }

    #[test]
    fn alt_shift_letter_is_uppercased() {
        let key = Key::Character("x".into());
        assert_eq!(
            encode_str(&key, &(Modifiers::ALT | Modifiers::SHIFT)).as_deref(),
            Some("\x1bX")
        );
    }

    #[test]
    fn unidentified_and_modifier_only_keys_are_not_forwarded() {
        // 应用要保留 Ctrl+Shift+P 这类快捷键 —— 返回 None 由它处理。
        assert_eq!(encode_str(&Key::Unidentified, &Modifiers::default()), None);
        assert_eq!(
            encode_str(&plain("a").0, &Modifiers::CTRL),
            Some("\x01".to_string())
        );
    }

    #[test]
    fn ctrl_alt_letters_match_the_reference() {
        // ESC + 控制字符。Zed 的 keys.rs 就是这么做（`control && is_ascii_lowercase`
        // 那一支），bash/zle 也按这个解析。别自作聪明改成 None。
        let key = Key::Character("a".into());
        assert_eq!(
            encode_str(&key, &mods(false, true, true)).as_deref(),
            Some("\x1b\x01")
        );
    }
}
