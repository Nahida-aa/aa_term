//! 输入映射 —— 把 UI 侧的键盘/鼠标事件翻译成终端的语义或要发出去的字节。
//!
//! 对应 Zed 的 `terminal/src/mappings/`（`keys` / `mouse` / `colors`）。
//! 拆出这一层的意义：**「终端认什么」和「怎么画」是两个独立问题**。映射只
//! 依赖键盘鼠标事件的抽象，可以在不起窗口的前提下逐条测完；渲染层则完全
//! 不必知道某个组合键会发出什么字节。
//!
//! ```text
//! 键盘/鼠标事件 ──► mappings ──► 发往 PTY 的字节（喂给 shell）
//!                              └► 终端自身行为（滚动、拖选）
//! ```
//!
//! 分岔的判断在这里做，不要在渲染层做：例如 `Ctrl+Shift+C` 是应用快捷键
//! （复制），而 `Ctrl+C` 是发 `\x03` 给 shell。两者按键相同，语义相反。

pub mod keys;
pub mod mouse;

#[cfg(test)]
mod tests {
    use crate::buffer::Terminal;
    use crate::grid::{GridGeometry, Point, Rounding, SelectionSide};
    use crate::session::window_size_from;

    fn terminal() -> (Terminal, GridGeometry) {
        let geometry = GridGeometry::new(800., 480., 10., 20.);
        let terminal = Terminal::for_test(window_size_from(&geometry));
        (terminal, geometry)
    }

    /// 模块存在的理由：命中测试不该由渲染层各写一份。
    #[test]
    fn hit_test_lands_on_the_clicked_cell() {
        let (terminal, geometry) = terminal();
        // 第 2 列第 1 行的正中间
        let point = terminal.hit_test(2. * 10. + 5., 20. + 10., &geometry);
        assert_eq!(point, Point::new(1, 2));
    }

    #[test]
    fn hit_test_clamps_beyond_the_viewport() {
        let (terminal, geometry) = terminal();
        let far = terminal.hit_test(9_999., 9_999., &geometry);
        assert_eq!(far, Point::new(23, 79));
        let negative = terminal.hit_test(-5., -5., &geometry);
        assert_eq!(negative, Point::new(0, 0));
    }

    #[test]
    fn hit_test_side_splits_the_cell_in_half() {
        let (terminal, geometry) = terminal();
        assert_eq!(terminal.hit_test_side(14., &geometry), SelectionSide::Left);
        assert_eq!(terminal.hit_test_side(16., &geometry), SelectionSide::Right);
        // 边界：正好一半算左边，与上游一致
        assert_eq!(terminal.hit_test_side(15., &geometry), SelectionSide::Left);
    }

    #[test]
    fn hit_test_agrees_with_selection_point() {
        // 两条路径（模块内的 hit_test 与 selection_point）必须给出一致的落点，
        // 否则鼠标点选和拖选的起点会差一格。
        let (terminal, geometry) = terminal();
        for (x, y) in [(0., 0.), (45., 30.), (100., 100.)] {
            let hit = terminal.hit_test(x, y, &geometry).column;
            let selection = terminal.selection_point(x, y, &geometry).column.0;
            assert_eq!(hit, selection, "({x}, {y}) 两条路径不一致");
        }
    }

    #[test]
    fn rounding_policy_is_explicit() {
        // 上游两套像素→cell 取整并存（mappings/mouse.rs 用截断，
        // mouse.rs::content_index_for_mouse 用四舍五入）。我们把策略做成显式
        // 参数，这里锁住本模块选的是截断。
        let geometry = GridGeometry::new(800., 480., 10., 20.);
        let truncated = geometry.point_at(16., 0., Rounding::Truncate);
        let rounded = geometry.point_at(16., 0., Rounding::Round);
        assert_eq!(truncated.column, 1, "16/10 截断得第 1 列");
        assert_eq!(rounded.column, 2, "同样位置四舍五入得第 2 列");
    }
}
