//! 键位表 —— 对应 Zed 的 `crates/keymap` + `assets/keymaps/*.json`。
//!
//! Zed 的做法：动作是 `Action` 结构体（`multi_workspace::ToggleWorkspaceSidebar`），
//! 键位写在 `assets/keymaps/default-linux.json` 里，运行时由 `Keymap` 解析成
//! 「按键序列 → 动作」再按焦点派发。**没有**中心化的快捷键枚举这种东西。
//!
//! 我们的对应物：动作词汇仍是一个枚举 [`AppShortcut`]（iced 没有 action 派发，
//! 枚举 + `match` 就是最省的等价物），但**键位从代码里搬进了文件**：
//! `packages/aa_term/assets/keymaps/default-linux.json`。这样「改个绑定」不用改
//! 代码，格式也跟上游一个样子（数组 + `bindings` 映射），照抄 zed 的键位时可以直接
//! 粘。
//!
//! 与上游的差别（有意为之）：
//!
//! - **没有 context / 焦点作用域**。zed 的绑定可以带 `context`（如
//!   `"context": "Workspace"`），由焦点决定谁生效；我们只有单个终端，没有分发层。
//! - **绑定值只支持字符串**（动作名）。zed 还允许字符串数组、`null`（禁用某个
//!   绑定的覆盖写法）与对象形式。
//! - **修饰键按等价匹配**，不做「多按一个修饰键也算命中」。zed 那边也是这样。
//! - 键位串只认我们在用的那些名字（见 [`parse_keystroke`]），不是 zed 那套完整的
//!   键名表。

use std::collections::BTreeMap;
use std::fmt;

use iced_core::keyboard::{Key, Modifiers, key::Named};
use serde::Deserialize;

use super::keys::AppShortcut;

/// 键位文件里的一个层级。
///
/// zed 的键位文件是**数组**（多个层级按顺序叠加，后面的覆盖前面的），每个元素形如
/// `{"bindings": {...}}`。我们只用到一层，但保持数组形状，将来加平台差异
/// （`default-macos.json` 之类）时不用换格式。
#[derive(Debug, Clone, Deserialize)]
struct KeymapFile {
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

/// 一条绑定。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyBinding {
    /// 原始键位串（如 `ctrl-shift-f`）—— 原样留着，报错与调试都用得上。
    pub keystroke: String,
    /// 解析出来的修饰键。
    pub modifiers: Modifiers,
    /// 解析出来的键。
    pub key: Key,
    /// 命中的动作。
    pub action: AppShortcut,
}

/// 应用级键位表。
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<KeyBinding>,
}

impl Keymap {
    /// 从键位文件的文本解析。
    pub fn from_json(json: &str) -> Result<Keymap, KeymapError> {
        let files: Vec<KeymapFile> = serde_json::from_str(json).map_err(KeymapError::Json)?;

        let mut bindings = Vec::new();
        for file in files {
            for (keystroke, action) in file.bindings {
                let (modifiers, key) = parse_keystroke(&keystroke)?;
                let action = AppShortcut::from_name(&action).ok_or(KeymapError::UnknownAction {
                    action: action.clone(),
                })?;
                bindings.push(KeyBinding {
                    keystroke,
                    modifiers,
                    key,
                    action,
                });
            }
        }
        // `BTreeMap` 的遍历顺序只保证按字符串排，而绑定之间**不该有重叠**
        // （有的话 `action_for` 取第一条，谁赢取决于字符串序 —— 靠不住）。
        // 这里把重叠当成配置错误直接报出来，别让它变成「偶尔按不出来」。
        if let Some((first, second)) = find_conflict(&bindings) {
            return Err(KeymapError::ConflictingBindings {
                first: first.clone(),
                second: second.clone(),
            });
        }
        Ok(Keymap { bindings })
    }

    /// 这次按键命中的动作；没有绑定命中就是 `None`。
    ///
    /// 修饰键要**完全一致**才命中：`ctrl-shift-c` 不会因为多按了 Alt 也命中。
    pub fn action_for(&self, key: &Key, modifiers: Modifiers) -> Option<AppShortcut> {
        self.bindings
            .iter()
            .find(|binding| {
                same_modifiers(binding.modifiers, modifiers) && same_key(&binding.key, key)
            })
            .map(|binding| binding.action)
    }

    /// 表里所有的绑定（顺序按键位串排）。
    pub fn bindings(&self) -> &[KeyBinding] {
        &self.bindings
    }
}

/// 修饰键是否完全相同。
///
/// iced 的 `Modifiers` 是位集合，这里逐位比 —— 用 `contains` 会让「多按一个修饰键
/// 也算命中」，那样 `ctrl-shift-z` 会被 `ctrl-shift` 之类的绑定误抢。
fn same_modifiers(left: Modifiers, right: Modifiers) -> bool {
    left.control() == right.control()
        && left.alt() == right.alt()
        && left.shift() == right.shift()
        && left.logo() == right.logo()
}

/// 两次按键是不是同一个键。
///
/// 字符键忽略大小写：按住 Shift 时 iced 送来的是大写（`Key::Character("F")`），
/// 而键位文件里写的是 `ctrl-shift-f`。大小写各写一条绑定容易漏。
fn same_key(left: &Key, right: &Key) -> bool {
    match (left, right) {
        (Key::Character(left), Key::Character(right)) => left.eq_ignore_ascii_case(right),
        (Key::Named(left), Key::Named(right)) => left == right,
        _ => false,
    }
}

/// 找出第一对互相冲突（同一个键 + 同一组修饰键）的绑定。
fn find_conflict(bindings: &[KeyBinding]) -> Option<(&String, &String)> {
    for (index, binding) in bindings.iter().enumerate() {
        for other in &bindings[index + 1..] {
            if same_modifiers(binding.modifiers, other.modifiers)
                && same_key(&binding.key, &other.key)
            {
                return Some((&binding.keystroke, &other.keystroke));
            }
        }
    }
    None
}

/// 解析一个键位串（如 `ctrl-shift-f`、`shift-pageup`）。
///
/// 修饰键写在前面，用 `-` 分隔，最后一段是键本身。名字与 Zed 的键位文件一致：
/// `ctrl` / `alt` / `shift` / `cmd`（也接受 `super`、`win`），键名用
/// `up` / `down` / `left` / `right` / `pageup` / `pagedown` / `home` / `end` /
/// `escape` / `enter` / `tab` / `backspace` / `space`，其余单个字符按字面量。
///
/// **不支持**把 `-` 本身当键（zed 写作 `minus`）：我们没这个绑定，先不认。
pub fn parse_keystroke(keystroke: &str) -> Result<(Modifiers, Key), KeymapError> {
    let unknown = || KeymapError::UnknownKeystroke {
        keystroke: keystroke.to_owned(),
    };

    // `rsplit_once` 而不是 `split`：键名里不会出现 `-`，从右边切一段最省心。
    let (modifiers, key) = match keystroke.rsplit_once('-') {
        Some((modifiers, key)) => (modifiers, key),
        None => ("", keystroke),
    };

    let mut parsed = Modifiers::default();
    for modifier in modifiers.split('-').filter(|part| !part.is_empty()) {
        match modifier {
            "ctrl" | "control" => parsed.insert(Modifiers::CTRL),
            "alt" | "option" => parsed.insert(Modifiers::ALT),
            "shift" => parsed.insert(Modifiers::SHIFT),
            "cmd" | "super" | "win" | "logo" => parsed.insert(Modifiers::LOGO),
            _ => return Err(unknown()),
        }
    }

    let key = match key {
        "up" => Key::Named(Named::ArrowUp),
        "down" => Key::Named(Named::ArrowDown),
        "left" => Key::Named(Named::ArrowLeft),
        "right" => Key::Named(Named::ArrowRight),
        "pageup" => Key::Named(Named::PageUp),
        "pagedown" => Key::Named(Named::PageDown),
        "home" => Key::Named(Named::Home),
        "end" => Key::Named(Named::End),
        "escape" => Key::Named(Named::Escape),
        "enter" => Key::Named(Named::Enter),
        "tab" => Key::Named(Named::Tab),
        "backspace" => Key::Named(Named::Backspace),
        "space" => Key::Named(Named::Space),
        // 单个字符（含 `F` 这样带 Shift 时 iced 会送大写的情况）按字面量收下，
        // 匹配时忽略大小写。
        character if character.chars().count() == 1 => Key::Character(character.to_owned().into()),
        _ => return Err(unknown()),
    };

    Ok((parsed, key))
}

/// 键位表的问题。
#[derive(Debug)]
pub enum KeymapError {
    /// JSON 本身不合法，或结构不是「数组 + `bindings` 映射」。
    Json(serde_json::Error),
    /// 键位串不认识（`ctrl-nope`、`ctrl-`、多字符键名等）。
    UnknownKeystroke {
        /// 出问题的键位串。
        keystroke: String,
    },
    /// 动作名不在 [`AppShortcut`] 的名单里。
    UnknownAction {
        /// 出问题的动作名。
        action: String,
    },
    /// 两条绑定落在同一个键 + 同一组修饰键上。
    ConflictingBindings {
        /// 先出现的那条。
        first: String,
        /// 撞上它的那条。
        second: String,
    },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::Json(error) => write!(f, "键位表不是合法 JSON: {error}"),
            KeymapError::UnknownKeystroke { keystroke } => {
                write!(f, "不认识的键位: {keystroke}")
            }
            KeymapError::UnknownAction { action } => write!(f, "不认识的动作: {action}"),
            KeymapError::ConflictingBindings { first, second } => {
                write!(f, "键位冲突: {first} 与 {second} 绑到同一个键")
            }
        }
    }
}

impl std::error::Error for KeymapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KeymapError::Json(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Keymap {
        Keymap::from_json(json).expect("测试用的键位表应当能解析")
    }

    /// 三处取键的写法（文件串、iced 事件）之间别打架。
    #[test]
    fn a_keystroke_parses_into_modifiers_and_a_key() {
        let (modifiers, key) = parse_keystroke("ctrl-shift-f").expect("应能解析");
        assert!(modifiers.control() && modifiers.shift() && !modifiers.alt());
        assert_eq!(key, Key::Character("f".into()));

        let (modifiers, key) = parse_keystroke("shift-pageup").expect("应能解析");
        assert!(modifiers.shift() && !modifiers.control());
        assert_eq!(key, Key::Named(Named::PageUp));

        // 没有修饰键的写法
        let (modifiers, key) = parse_keystroke("escape").expect("应能解析");
        assert_eq!(modifiers, Modifiers::default());
        assert_eq!(key, Key::Named(Named::Escape));
    }

    #[test]
    fn a_bad_keystroke_is_rejected() {
        for keystroke in ["ctrl-", "ctrl-nope", "", "ctrl-hyper"] {
            assert!(
                matches!(
                    parse_keystroke(keystroke),
                    Err(KeymapError::UnknownKeystroke { .. })
                ),
                "{keystroke:?} 应当被拒"
            );
        }
    }

    /// 大写字母必须也命中 —— 按住 Shift 时 iced 送的就是大写。
    #[test]
    fn character_keys_ignore_case() {
        let keymap = parse(r#"[{"bindings": {"ctrl-shift-c": "aa_term::Copy"}}]"#);
        let mut modifiers = Modifiers::default();
        modifiers.insert(Modifiers::CTRL);
        modifiers.insert(Modifiers::SHIFT);

        assert_eq!(
            keymap.action_for(&Key::Character("C".into()), modifiers),
            Some(AppShortcut::Copy)
        );
        assert_eq!(
            keymap.action_for(&Key::Character("c".into()), modifiers),
            Some(AppShortcut::Copy)
        );
    }

    /// 修饰键要完全一致：多一个、少一个都不算命中。
    #[test]
    fn modifiers_must_match_exactly() {
        let keymap = parse(r#"[{"bindings": {"ctrl-alt-j": "aa_term::ToggleWorkspaceSidebar"}}]"#);

        let mut ctrl_alt = Modifiers::default();
        ctrl_alt.insert(Modifiers::CTRL);
        ctrl_alt.insert(Modifiers::ALT);
        assert_eq!(
            keymap.action_for(&Key::Character("j".into()), ctrl_alt),
            Some(AppShortcut::ToggleWorkspaceSidebar)
        );

        let mut ctrl_alt_shift = ctrl_alt;
        ctrl_alt_shift.insert(Modifiers::SHIFT);
        assert_eq!(
            keymap.action_for(&Key::Character("j".into()), ctrl_alt_shift),
            None,
            "多按一个 Shift 不该命中"
        );

        let mut ctrl = Modifiers::default();
        ctrl.insert(Modifiers::CTRL);
        assert_eq!(
            keymap.action_for(&Key::Character("j".into()), ctrl),
            None,
            "少一个 Alt 不该命中"
        );
    }

    #[test]
    fn several_layers_are_merged() {
        let keymap = parse(
            r#"[
                {"bindings": {"ctrl-shift-c": "aa_term::Copy"}},
                {"bindings": {"ctrl-shift-v": "aa_term::Paste"}}
            ]"#,
        );
        assert_eq!(keymap.bindings().len(), 2);
    }

    /// 同一个键绑两个动作是配置错误，解析时就报出来 —— 否则谁赢取决于字符串序。
    #[test]
    fn conflicting_bindings_are_rejected() {
        let error = Keymap::from_json(
            r#"[
                {"bindings": {"ctrl-shift-c": "aa_term::Copy"}},
                {"bindings": {"shift-ctrl-c": "aa_term::Paste"}}
            ]"#,
        )
        .expect_err("重复键位应当报错");
        assert!(matches!(error, KeymapError::ConflictingBindings { .. }));
    }

    #[test]
    fn unknown_actions_and_broken_json_are_rejected() {
        assert!(matches!(
            Keymap::from_json(r#"[{"bindings": {"ctrl-shift-c": "aa_term::Nope"}}]"#),
            Err(KeymapError::UnknownAction { .. })
        ));
        assert!(matches!(
            Keymap::from_json("不是 json"),
            Err(KeymapError::Json(_))
        ));
    }

    /// 动作名要能反查回枚举 —— 键位文件里写的就是这些名字。
    #[test]
    fn every_action_name_round_trips() {
        for action in [
            AppShortcut::Copy,
            AppShortcut::Paste,
            AppShortcut::OpenSearch,
            AppShortcut::ScrollLineUp,
            AppShortcut::ScrollLineDown,
            AppShortcut::ScrollPageUp,
            AppShortcut::ScrollPageDown,
            AppShortcut::ScrollToTop,
            AppShortcut::ScrollToBottom,
            AppShortcut::ToggleWorkspaceSidebar,
        ] {
            assert_eq!(
                AppShortcut::from_name(action.name()),
                Some(action),
                "{} 反查不回来",
                action.name()
            );
        }
        assert_eq!(AppShortcut::from_name("aa_term::Nope"), None);
    }
}
