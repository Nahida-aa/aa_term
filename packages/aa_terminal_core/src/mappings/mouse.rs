//! 鼠标映射：像素坐标 → 终端语义，以及滚轮的方向键回退。
//!
//! 对应 Zed 的 `terminal/src/mappings/mouse.rs`。那边还包含鼠标事件上报
//! （`mouse_moved_report` / `mouse_button_report` / X10/SGR 协议）—— 那是
//! 「让终端里的 vim 响应鼠标」用的，我们这版还没接，先只做本地交互
//! （拖选 + 滚动）。
//!
//! ## 一个必须记住的约定
//!
//! iced 给的是**窗口内**像素坐标，而所有命中测试要的是**终端画布内**的坐标。
//! 应用层需要先减去画布原点再进来；少了这一步，鼠标点上方实际会命中下方
//! 对应位置，症状是「点哪都不对」。

use crate::buffer::Terminal;
use crate::grid::{GridGeometry, Point, Rounding, SelectionSide};
use iced_core::keyboard::Modifiers;
use iced_core::mouse::Button;

impl Terminal {
    /// 像素坐标 → 视口网格点。
    ///
    /// 截断而不是四舍五入：命中应该落在 cell 内，四舍五入会让点击半个 cell 时
    /// 跳到下一格去。这一点 Zed 与我们一致，但 AAEnt 下游还有另一套用
    /// `round()` 的实现，别混用。
    pub fn hit_test(&self, x: f32, y: f32, geometry: &GridGeometry) -> Point {
        geometry.point_at(x, y, Rounding::Truncate)
    }

    /// 命中的位置在 cell 左半边还是右半边。
    ///
    /// 块选区用得到：右半边的命中应该让选区多含一列。
    pub fn hit_test_side(&self, x: f32, geometry: &GridGeometry) -> SelectionSide {
        if geometry.side_at(x) == SelectionSide::Right {
            SelectionSide::Right
        } else {
            SelectionSide::Left
        }
    }
}

// ---------------------------------------------------------------------------
// 鼠标事件上报：让终端里的 vim / tmux / htop 能响应点击和滚轮
// ---------------------------------------------------------------------------

/// 终端用哪种鼠标协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseFormat {
    /// SGR（`CSI < ... M/m`）。现代终端默认，坐标无上限。
    Sgr,
    /// X10/普通协议，`utf8` 表示是否用 UTF-8 扩展坐标。
    Normal(bool),
}

impl MouseFormat {
    /// 由 terminal 的模式推断用哪套。
    ///
    /// 顺序不能反：SGR 与 UTF8 都被开启时 SGR 优先（Zed 同款判断）。
    pub fn from_mode(mode: MouseMode) -> Self {
        if mode.sgr {
            MouseFormat::Sgr
        } else if mode.utf8 {
            MouseFormat::Normal(true)
        } else {
            MouseFormat::Normal(false)
        }
    }
}

/// terminal 当前开启的鼠标相关模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMode {
    /// 是否开启鼠标上报（1000/1002/1003 任一）。
    pub enabled: bool,
    /// 是否需要移动事件也上报（1002/1003）。
    pub motion: bool,
    /// 是否只在按住拖动时上报（1002）。
    pub drag_only: bool,
    /// 是否用 SGR 编码（1006）。
    pub sgr: bool,
    /// 是否用 UTF-8 扩展坐标（1005）。
    pub utf8: bool,
}

/// 鼠标按键/滚轮在协议里的编码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButtonCode {
    /// 左键 0。
    Left = 0,
    /// 中键 1。
    Middle = 1,
    /// 右键 2。
    Right = 2,
    /// 左键拖动 32。
    LeftMove = 32,
    /// 中键拖动 33。
    MiddleMove = 33,
    /// 右键拖动 34。
    RightMove = 34,
    /// 无键移动 35。
    NoneMove = 35,
    /// 滚轮上 64。
    ScrollUp = 64,
    /// 滚轮下 65。
    ScrollDown = 65,
    /// 不认识的键（如侧键）。Zed 会直接丢弃。
    Other = 99,
}

impl MouseButtonCode {
    /// 由 iced 的按键推断。
    pub fn from_iced(button: iced_core::mouse::Button) -> Self {
        use iced_core::mouse::Button;
        match button {
            Button::Left => Self::Left,
            Button::Middle => Self::Middle,
            Button::Right => Self::Right,
            _ => Self::Other,
        }
    }

    /// 拖动时按「哪个键在按着」推断编码。
    pub fn from_dragging(button: Option<iced_core::mouse::Button>) -> Self {
        match button {
            Some(Button::Left) => Self::LeftMove,
            Some(Button::Middle) => Self::MiddleMove,
            Some(Button::Right) => Self::RightMove,
            Some(_) => Self::Other,
            None => Self::NoneMove,
        }
    }

    /// 是否该丢弃。
    pub fn is_other(&self) -> bool {
        matches!(self, Self::Other)
    }
}

/// 滚轮方向 → 编码。
///
/// 正 delta 是「往上滚」，对应看更早的内容。
pub fn scroll_code(delta: iced_core::mouse::ScrollDelta) -> MouseButtonCode {
    use iced_core::mouse::ScrollDelta;
    let up = match delta {
        ScrollDelta::Lines { y, .. } => y > 0.,
        ScrollDelta::Pixels { y, .. } => y > 0.,
    };
    if up {
        MouseButtonCode::ScrollUp
    } else {
        MouseButtonCode::ScrollDown
    }
}

/// 修饰键在协议里的位标记。
///
/// 这是 xterm 的固定编码，不是我们能改的：shift=4、alt=8、ctrl=16。
pub fn modifier_bits(shift: bool, alt: bool, control: bool) -> u8 {
    let mut bits = 0;
    if shift {
        bits += 4;
    }
    if alt {
        bits += 8;
    }
    if control {
        bits += 16;
    }
    bits
}

/// 生成一次鼠标事件上报的字节。
///
/// 返回 `None` 表示不该上报 —— 调用方（`Session::scroll` 那条路径）据此决定
/// 是自己消费滚动还是转给程序。
pub fn mouse_report(
    point: crate::grid::Point,
    button: MouseButtonCode,
    // 按下还是松开。SGR 的 M/m 靠它区分。
    pressed: bool,
    modifiers: Modifiers,
    format: MouseFormat,
) -> Option<Vec<u8>> {
    // 负行在视口上方，发出去程序也定位不到。
    if point.line < 0 {
        return None;
    }

    let mods = modifier_bits(modifiers.shift(), modifiers.alt(), modifiers.control());

    match format {
        MouseFormat::Sgr => Some(sgr_report(point, button as u8 + mods, pressed)),
        MouseFormat::Normal(utf8) => {
            // 松开时按键码固定为 3（「释放」），Zed 与 alacritty 一致。
            let button = if pressed {
                button as u8 + mods
            } else {
                3 + mods
            };
            normal_report(point, button, utf8)
        }
    }
}

/// SGR 编码：`CSI < button ; col ; row [Mm]`。
///
/// 坐标是**1-based**，与 X10 的 1-based 偏移不同。
fn sgr_report(point: crate::grid::Point, button: u8, pressed: bool) -> Vec<u8> {
    let terminator = if pressed { 'M' } else { 'm' };
    format!(
        "\x1b[<{};{};{}{}",
        button,
        point.column + 1,
        point.line + 1,
        terminator
    )
    .into_bytes()
}

/// X10 / 普通编码：`CSI M` + 三个偏移 32 的字节。
///
/// 坐标上限 **223**（UTF-8 扩展下 2015）—— 这不是我们能选的，协议本身如此。
/// 超限就只能丢弃：发出去程序会收到乱码。
fn normal_report(point: crate::grid::Point, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };
    if point.line >= max_point || point.column >= max_point as usize {
        return None;
    }

    let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

    // UTF-8 扩展：坐标 >= 95 时改用两字节编码，否则一个字节就够。
    let encode = |pos: usize| -> Vec<u8> {
        let pos = 32 + 1 + pos;
        let first = 0xC0 + pos / 64;
        let second = 0x80 + (pos & 63);
        vec![first as u8, second as u8]
    };

    if utf8 && point.column >= 95 {
        msg.extend(encode(point.column));
    } else {
        msg.push(32 + 1 + point.column as u8);
    }
    if utf8 && point.line >= 95 {
        msg.extend(encode(point.line as usize));
    } else {
        msg.push(32 + 1 + point.line as u8);
    }

    Some(msg)
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn no_mods() -> iced_core::keyboard::Modifiers {
        iced_core::keyboard::Modifiers::default()
    }

    fn point(line: i32, column: usize) -> crate::grid::Point {
        crate::grid::Point::new(line, column)
    }

    #[test]
    fn sgr_encoding_uses_one_based_coordinates() {
        // 屏幕左上角 (0,0) 上报成 1;1 —— 偏移错了会让程序点哪都偏一格。
        let bytes = mouse_report(
            point(0, 0),
            MouseButtonCode::Left,
            true,
            no_mods(),
            MouseFormat::Sgr,
        )
        .expect("SGR 应总能编码");
        assert_eq!(bytes, b"\x1b[<0;1;1M");
    }

    #[test]
    fn sgr_press_and_release_differ_only_in_terminator() {
        let press = mouse_report(
            point(3, 4),
            MouseButtonCode::Left,
            true,
            no_mods(),
            MouseFormat::Sgr,
        )
        .unwrap();
        let release = mouse_report(
            point(3, 4),
            MouseButtonCode::Left,
            false,
            no_mods(),
            MouseFormat::Sgr,
        )
        .unwrap();
        assert_eq!(press, b"\x1b[<0;5;4M");
        assert_eq!(release, b"\x1b[<0;5;4m");
    }

    #[test]
    fn modifier_bits_follow_the_xterm_table() {
        // shift=4 / alt=8 / ctrl=16，且是**相加**而不是位或的组合结果。
        let code = MouseButtonCode::Left as u8
            + modifier_bits(true, false, false)
            + modifier_bits(false, true, false)
            + modifier_bits(false, false, true);
        assert_eq!(code, 28, "4 + 8 + 16 应落在 Ctrl+Alt+Shift 这一档");
    }

    #[test]
    fn modifiers_are_encoded_into_the_button_code() {
        let ctrl = iced_core::keyboard::Modifiers::CTRL;
        let bytes = mouse_report(
            point(0, 0),
            MouseButtonCode::Left,
            true,
            ctrl,
            MouseFormat::Sgr,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<16;1;1M", "ctrl 应把按键码加 16");
    }

    #[test]
    fn normal_encoding_is_x10_shape_with_offset_32() {
        // X10：CSI M + 一个"按键码"字节 + 列 + 行，每项都偏移 32。
        let bytes = mouse_report(
            point(0, 0),
            MouseButtonCode::Left,
            true,
            no_mods(),
            MouseFormat::Normal(false),
        )
        .expect("小坐标应能编码");
        assert_eq!(bytes, b"\x1b[M\x20\x21\x21");
    }

    #[test]
    fn normal_release_uses_button_code_three() {
        // 松开固定用 3，与 SGR 用 m 后缀区分。
        let bytes = mouse_report(
            point(0, 0),
            MouseButtonCode::Left,
            false,
            no_mods(),
            MouseFormat::Normal(false),
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[M\x23\x21\x21", "0x20 + (3 + 0) = 0x23");
    }

    #[test]
    fn normal_encoding_refuses_coordinates_beyond_the_protocol_limit() {
        // X10 上限 223，超了发出去就是乱码，只能丢。
        let far = point(300, 10);
        assert!(
            mouse_report(
                far,
                MouseButtonCode::Left,
                true,
                no_mods(),
                MouseFormat::Normal(false)
            )
            .is_none(),
            "第 300 行超出 X10 上限"
        );
    }

    #[test]
    fn utf8_extension_handles_coordinates_beyond_ninety_five() {
        // UTF-8 扩展（1005）：坐标 >= 95 时改用两字节，上限抬到 2015。
        let point = point(100, 10);
        let bytes = mouse_report(
            point,
            MouseButtonCode::Left,
            true,
            no_mods(),
            MouseFormat::Normal(true),
        )
        .expect("UTF-8 扩展应能编码第 100 行");
        // ESC [ M + 按键码 (4) + 列 1 字节 + 行 2 字节 = 7。
        // 前缀是 4 字节不是 3，我第一版按 3 算错过。
        assert_eq!(bytes.len(), 7, "应为一列一行两种长度: {bytes:?}");
        assert_eq!(&bytes[..3], b"\x1b[M");
    }

    #[test]
    fn negative_lines_are_never_reported() {
        // 视口上方的行程序定位不到。
        assert!(
            mouse_report(
                point(-1, 0),
                MouseButtonCode::Left,
                true,
                no_mods(),
                MouseFormat::Sgr
            )
            .is_none()
        );
    }

    #[test]
    fn format_prefers_sgr_over_utf8() {
        // 两者都开时 SGR 优先（Zed 同款判断）。
        let mode = MouseMode {
            sgr: true,
            utf8: true,
            ..Default::default()
        };
        assert_eq!(MouseFormat::from_mode(mode), MouseFormat::Sgr);
    }

    #[test]
    fn button_codes_match_the_protocol() {
        assert_eq!(MouseButtonCode::Left as u8, 0);
        assert_eq!(MouseButtonCode::Middle as u8, 1);
        assert_eq!(MouseButtonCode::Right as u8, 2);
        assert_eq!(MouseButtonCode::NoneMove as u8, 35);
        assert_eq!(MouseButtonCode::ScrollUp as u8, 64);
        assert_eq!(MouseButtonCode::ScrollDown as u8, 65);
    }

    #[test]
    fn unknown_buttons_are_dropped() {
        // 侧键之类没有约定编码，发出去程序会误解。
        assert!(MouseButtonCode::from_iced(iced_core::mouse::Button::Back).is_other());
    }

    #[test]
    fn scroll_direction_maps_to_up_and_down_codes() {
        assert_eq!(
            scroll_code(iced_core::mouse::ScrollDelta::Lines { y: 1., x: 0. }),
            MouseButtonCode::ScrollUp
        );
        assert_eq!(
            scroll_code(iced_core::mouse::ScrollDelta::Lines { y: -1., x: 0. }),
            MouseButtonCode::ScrollDown
        );
    }

    #[test]
    fn dragging_without_a_button_is_none_move() {
        assert_eq!(
            MouseButtonCode::from_dragging(None),
            MouseButtonCode::NoneMove
        );
        assert_eq!(
            MouseButtonCode::from_dragging(Some(iced_core::mouse::Button::Left)),
            MouseButtonCode::LeftMove
        );
    }
}
