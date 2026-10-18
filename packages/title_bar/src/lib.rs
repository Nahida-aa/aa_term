//! 标题栏**内容** —— 对应 Zed 的 `crates/title_bar`。
//!
//! Zed 的 `TitleBar` 渲染的是窗口框架（`crates/platform_title_bar`）**里面**那一行：
//! 左边的 `File/Edit/…` 菜单与项目名、分支，右边的协作者 / 连接状态 / 用户菜单。
//! 窗口框架（拖拽区、最小化/最大化/关闭）在 [`aa_ui_iced::TitleBar`]；本 crate 只
//! 产出「内容」，由 app 塞进框架的插槽里 —— 与上游两个 crate 的分工一致。
//!
//! **菜单名与条目照 zed**（`zed/src/zed/app_menus.rs` 的 8 个菜单 + 各菜单里我们
//! 能对上的条目）。还没有对应功能的条目**照上游显示但置灰**（`None` 动作），缺口
//! 留在界面上看得见，而不是悄悄省略。
//!
//! **未搬**：
//!
//! - 菜单里剩下的条目（New File/Open/Save/Find/…），它们要命令系统、编辑器、
//!   终端面板这些还不存在的东西；
//! - 菜单开着时悬停别的菜单**就换过去**（上游 `open_menus_on_hover` 默认是关的，
//!   只有「已经有一个菜单开着」时才跟着指针换；那条设置本身要设置系统，见下）、
//!   菜单栏可整体展开成一行（`show_menus` 默认 false —— Linux 上上游平时只画一个
//!   ☰ 按钮，点了才展开整条菜单栏，我们是常显，与 `show_menus = true` 等价）、
//!   菜单里的上下键选择与回车确认（要 `ContextMenu` 有选中态）；
//! - 右侧那一串（协作者头像、连接状态、更新版本、登录、用户菜单）—— 等有账号与
//!   协作再说，现在留空；
//! - 项目名的下拉是「最近项目」（zed `render_recent_projects_popover`），我们这里
//!   只是复制路径 / 在文件管理器里打开。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use aa_theme_iced::Theme as AppTheme;
use aa_ui_iced::{ContextMenu, ContextMenuEntry};
use iced::widget::{button, mouse_area, row, text};
use iced::{Alignment, Element};

/// 项目名的显示上限 —— 上游 `title_bar::MAX_PROJECT_NAME_LENGTH`。
const MAX_PROJECT_NAME_LENGTH: usize = 40;
/// 分支名的显示上限 —— 上游 `title_bar::MAX_BRANCH_NAME_LENGTH`。
const MAX_BRANCH_NAME_LENGTH: usize = 40;

/// 菜单栏上的一个菜单。
///
/// 名字与顺序照 zed（`app_menus.rs`）：File / Edit / Selection / View / Go / Run /
/// Window / Help。Mac 上还会多出 `Zed` 与 `Editor Layout`，Linux 没有。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuKind {
    /// 文件
    File,
    /// 编辑
    Edit,
    /// 选择
    Selection,
    /// 视图
    View,
    /// 跳转
    Go,
    /// 运行
    Run,
    /// 窗口
    Window,
    /// 帮助
    Help,
}

impl MenuKind {
    /// 菜单栏上的全部菜单，按显示顺序。
    pub const ALL: [MenuKind; 8] = [
        MenuKind::File,
        MenuKind::Edit,
        MenuKind::Selection,
        MenuKind::View,
        MenuKind::Go,
        MenuKind::Run,
        MenuKind::Window,
        MenuKind::Help,
    ];

    /// 菜单上显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            MenuKind::File => "File",
            MenuKind::Edit => "Edit",
            MenuKind::Selection => "Selection",
            MenuKind::View => "View",
            MenuKind::Go => "Go",
            MenuKind::Run => "Run",
            MenuKind::Window => "Window",
            MenuKind::Help => "Help",
        }
    }

    /// 左右相邻的那个菜单（到两头绕回去）。
    ///
    /// 上游 `navigate_menus_in_direction`（`application_menu.rs`）：菜单开着时按
    /// 左右键在菜单之间换，从最后一个往右回到第一个。
    pub fn neighbour(self, step: i32) -> MenuKind {
        let index = Self::ALL
            .iter()
            .position(|kind| *kind == self)
            .expect("每个菜单都在 ALL 里");
        let count = Self::ALL.len() as i32;
        Self::ALL[(index as i32 + step).rem_euclid(count) as usize]
    }

    /// 菜单里的条目：`action` 为 `None` 就是置灰占位（上游有、我们还没有）。
    fn entries(self) -> &'static [MenuEntry] {
        match self {
            MenuKind::File => FILE_MENU,
            MenuKind::Edit => EDIT_MENU,
            MenuKind::Selection => SELECTION_MENU,
            MenuKind::View => VIEW_MENU,
            MenuKind::Go => GO_MENU,
            MenuKind::Run => RUN_MENU,
            MenuKind::Window => WINDOW_MENU,
            MenuKind::Help => HELP_MENU,
        }
    }
}

/// 各菜单的条目表。
///
/// 写成 `const` 而不是在 `entries()` 里就地造：就地 `&[..]` 借的是临时数组，
/// 活不到 `'static`，而菜单表本来就该是编译期的常量。
const FILE_MENU: &[MenuEntry] = &[
    MenuEntry::new("New Thread", Some(MenuAction::NewThread)),
    MenuEntry::separator(),
    MenuEntry::disabled("New Window"),
    MenuEntry::disabled("Open…"),
    MenuEntry::separator(),
    // 单窗口应用里 File > Quit 就是关掉窗口（同 `CloseWindow`）。
    MenuEntry::new("Quit", Some(MenuAction::CloseWindow)),
];

const EDIT_MENU: &[MenuEntry] = &[
    MenuEntry::new("Copy", Some(MenuAction::Copy)),
    MenuEntry::new("Paste", Some(MenuAction::Paste)),
    MenuEntry::separator(),
    MenuEntry::disabled("Select All"),
];

const SELECTION_MENU: &[MenuEntry] = &[
    MenuEntry::disabled("Select All"),
    MenuEntry::disabled("Expand Selection"),
    MenuEntry::disabled("Add Cursor Above"),
];

const VIEW_MENU: &[MenuEntry] = &[
    MenuEntry::new("Toggle Sidebar", Some(MenuAction::ToggleSidebar)),
    MenuEntry::separator(),
    MenuEntry::disabled("Zoom In"),
    MenuEntry::disabled("Zoom Out"),
    MenuEntry::disabled("Reset Zoom"),
];

const GO_MENU: &[MenuEntry] = &[
    MenuEntry::disabled("Go to Line…"),
    MenuEntry::disabled("Go to Symbol…"),
    MenuEntry::disabled("Next Problem"),
];

const RUN_MENU: &[MenuEntry] = &[
    MenuEntry::disabled("Run"),
    MenuEntry::disabled("Stop"),
    MenuEntry::separator(),
    MenuEntry::disabled("Run Without Debugging"),
];

const WINDOW_MENU: &[MenuEntry] = &[
    MenuEntry::new("Minimize", Some(MenuAction::Minimize)),
    MenuEntry::new("Maximize", Some(MenuAction::Maximize)),
    MenuEntry::separator(),
    MenuEntry::new("Close Window", Some(MenuAction::CloseWindow)),
];

const HELP_MENU: &[MenuEntry] = &[
    MenuEntry::disabled("Documentation"),
    MenuEntry::separator(),
    MenuEntry::disabled("About aa_term"),
];

/// 菜单里的一条。
struct MenuEntry {
    label: &'static str,
    action: Option<MenuAction>,
    separator: bool,
}

impl MenuEntry {
    const fn new(label: &'static str, action: Option<MenuAction>) -> Self {
        MenuEntry {
            label,
            action,
            separator: false,
        }
    }

    /// 置灰占位。
    const fn disabled(label: &'static str) -> Self {
        MenuEntry {
            label,
            action: None,
            separator: false,
        }
    }

    const fn separator() -> Self {
        MenuEntry {
            label: "",
            action: None,
            separator: true,
        }
    }
}

/// 菜单里点得到的动作。
///
/// 名字取的是**动作**（上游是 `#[derive(Action)]` 的结构体，如 `workspace::Copy`、
/// `zed_actions::Quit`），宿主再映射成自己的消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// File > New Thread
    NewThread,
    /// Edit > Copy（复制终端选区）
    Copy,
    /// Edit > Paste（粘贴进 shell）
    Paste,
    /// View > Toggle Sidebar
    ToggleSidebar,
    /// Window > Minimize
    Minimize,
    /// Window > Maximize
    Maximize,
    /// Window > Close Window / File > Quit。
    ///
    /// 上游两个是不同的动作（`CloseWindow` / `Quit`），我们单窗口：关掉最后一个窗口
    /// 就是退出，两条合一。
    CloseWindow,
    /// 项目名菜单：复制项目路径
    CopyProjectPath,
    /// 项目名菜单：在文件管理器里打开
    RevealProject,
    /// 分支菜单：复制分支名
    CopyBranchName,
}

/// 标题栏发出来的消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// 点了菜单栏上的某个菜单：请宿主在指针处弹菜单。
    ///
    /// 位置不用从这儿传：宿主自己跟踪指针（iced 拿不到控件的屏幕坐标）。
    ShowMenu(MenuKind),
    /// 指针移到了某个菜单按钮上 —— 已经有一个菜单开着时，宿主据此换菜单
    /// （上游 `application_menu.rs` 里那个 `on_hover`）。
    HoverMenu(MenuKind),
    /// 点了项目名：请宿主弹项目菜单。
    ShowProjectMenu,
    /// 点了分支名：请宿主弹分支菜单。
    ShowBranchMenu,
}

/// 标题栏内容的状态：菜单栏 + 项目名 + 分支。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TitleBar {
    project_path: Option<PathBuf>,
    project_name: Option<String>,
    branch: Option<String>,
}

impl TitleBar {
    /// 空的标题栏：没有项目（界面上就没项目名与分支）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 认下 `dir` 所在的项目：往上找仓库根、取项目名与当前分支。
    ///
    /// 不在仓库里时项目名取 `dir` 自己的名字、没有分支 —— 与 zed 打开一个普通
    /// 文件夹的情形一致。返回「有没有变化」，宿主可以据此决定要不要重画。
    pub fn set_project_at(&mut self, dir: &Path) -> bool {
        let (path, name, branch) = match worktree::find_root(dir) {
            Some(root) => {
                let name = worktree::project_name(&root);
                (root.clone(), name, worktree::branch(&root))
            }
            None => (dir.to_path_buf(), worktree::project_name(dir), None),
        };

        let changed = self.project_path.as_deref() != Some(path.as_path()) || self.branch != branch;
        self.project_path = Some(path);
        self.project_name = name;
        self.branch = branch;
        changed
    }

    /// 项目根目录（没设过就是 `None`）。
    pub fn project_path(&self) -> Option<&Path> {
        self.project_path.as_deref()
    }

    /// 项目名。
    pub fn project_name(&self) -> Option<&str> {
        self.project_name.as_deref()
    }

    /// 当前分支（detached 时是短 sha）。
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// **画出来**的项目名（超长会截断）。
    ///
    /// 与 `view` 走同一份规则，测试要断言「画的是什么」时用它，别自己再截一遍。
    pub fn display_project_name(&self) -> Option<String> {
        self.project_name()
            .map(|name| truncate(name, MAX_PROJECT_NAME_LENGTH))
    }

    /// **画出来**的分支名（超长会截断）。
    pub fn display_branch(&self) -> Option<String> {
        self.branch()
            .map(|branch| truncate(branch, MAX_BRANCH_NAME_LENGTH))
    }
}

/// 某个菜单的下拉内容。
///
/// 位置由宿主给（指针位置）：iced 拿不到控件的屏幕坐标，所以菜单贴在指针处 ——
/// 上游是贴在菜单按钮下面。
pub fn menu<'a, Message: Clone + 'a>(
    kind: MenuKind,
    position: iced::Point,
    map: impl Fn(MenuAction) -> Message + 'a,
) -> ContextMenu<Message> {
    let map: Rc<dyn Fn(MenuAction) -> Message + 'a> = Rc::new(map);
    let mut menu = ContextMenu::new(position);
    for entry in kind.entries() {
        menu = if entry.separator {
            menu.separator()
        } else {
            let item = ContextMenuEntry::new(entry.label);
            menu.entry(match entry.action {
                Some(action) => {
                    let map = Rc::clone(&map);
                    item.on_press(map(action))
                }
                None => item.disabled(),
            })
        };
    }
    menu
}

/// 项目名按钮的下拉内容。
pub fn project_menu<'a, Message: Clone + 'a>(
    position: iced::Point,
    map: impl Fn(MenuAction) -> Message + 'a,
) -> ContextMenu<Message> {
    ContextMenu::new(position)
        .entry(
            ContextMenuEntry::new("Copy Project Path").on_press(map(MenuAction::CopyProjectPath)),
        )
        .entry(
            ContextMenuEntry::new("Reveal in File Manager")
                .on_press(map(MenuAction::RevealProject)),
        )
        .separator()
        .entry(ContextMenuEntry::new("Open Recent Project").disabled())
}

/// 分支按钮的下拉内容。
pub fn branch_menu<'a, Message: Clone + 'a>(
    position: iced::Point,
    map: impl Fn(MenuAction) -> Message + 'a,
) -> ContextMenu<Message> {
    ContextMenu::new(position)
        .entry(ContextMenuEntry::new("Copy Branch Name").on_press(map(MenuAction::CopyBranchName)))
        .separator()
        .entry(ContextMenuEntry::new("Branch Comparison").disabled())
}

/// 渲染标题栏内容（左边那一串）：菜单栏 + 项目名 + 分支。
///
/// 窗口框架（拖拽、窗口按钮）由 [`aa_ui_iced::TitleBar`] 提供 —— 宿主把这里的
/// 结果塞进它的 `left` 插槽。
pub fn view<'a, Host: Clone + 'a>(
    state: &'a TitleBar,
    theme: &'a AppTheme,
    on_message: impl Fn(Message) -> Host + 'a,
) -> Element<'a, Host> {
    let on_message: Rc<dyn Fn(Message) -> Host + 'a> = Rc::new(on_message);

    let mut bar = row![].align_y(Alignment::Center).spacing(MENU_GAP);

    for kind in MenuKind::ALL {
        let on_message = Rc::clone(&on_message);
        let on_hover = Rc::clone(&on_message);
        // 包一层 `mouse_area` 只为了拿 `on_enter`（`button` 不发悬停消息）：
        // 它先把事件交给里面的按钮，按钮没吃掉的才轮到自己，所以点击照旧。
        bar = bar.push(
            mouse_area(ghost_button(
                text(kind.label()).size(MENU_FONT_SIZE),
                theme,
                move || on_message(Message::ShowMenu(kind)),
            ))
            .on_enter(on_hover(Message::HoverMenu(kind))),
        );
    }

    if let Some(name) = state.display_project_name() {
        let on_message = Rc::clone(&on_message);
        bar = bar.push(ghost_button(
            row![
                text(PROJECT_ICON).size(MENU_FONT_SIZE),
                text(name).size(MENU_FONT_SIZE),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
            theme,
            move || on_message(Message::ShowProjectMenu),
        ));
    }

    if let Some(branch) = state.display_branch() {
        let on_message = Rc::clone(&on_message);
        bar = bar.push(ghost_button(
            row![
                text(BRANCH_ICON).size(MENU_FONT_SIZE),
                text(branch).size(MENU_FONT_SIZE),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
            theme,
            move || on_message(Message::ShowBranchMenu),
        ));
    }

    bar.into()
}

/// 菜单栏的字号 —— 上游菜单按钮是 `LabelSize::Small`。
const MENU_FONT_SIZE: f32 = 12.0;

/// 菜单之间、菜单与项目名之间的间距 —— 上游 `gap_0p5()`（2px）。
const MENU_GAP: f32 = 2.0;

/// 项目名的字形（上游是 `IconName::Folder`）。
const PROJECT_ICON: &str = "\u{2302}";
/// 分支的字形（上游是 `IconName::GitBranch`）。
const BRANCH_ICON: &str = "\u{2387}";

/// 菜单栏上的一个「隐形按钮」：平时无底、悬停才浮出 `ghost_element_hover`
/// （上游 `ButtonStyle::Subtle`）。
fn ghost_button<'a, Host: Clone + 'a>(
    content: impl Into<Element<'a, Host>>,
    theme: &'a AppTheme,
    on_press: impl Fn() -> Host + 'a,
) -> Element<'a, Host> {
    let colors = theme.colors();
    button(content)
        .padding([2, 6])
        .on_press_with(on_press)
        .style(move |_, status| button::Style {
            background: matches!(status, button::Status::Hovered | button::Status::Pressed)
                .then(|| colors.ghost_element_hover.into()),
            text_color: colors.text,
            border: iced::Border {
                radius: 4.0.into(),
                ..iced::Border::default()
            },
            ..button::Style::default()
        })
        .into()
}

/// 超长截断（上游用 `util::truncate_and_trailoff`）。
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    static THEME: std::sync::LazyLock<AppTheme> = std::sync::LazyLock::new(AppTheme::dark);

    /// 左右相邻的菜单：逐项往前、到两头绕回去（上游
    /// `navigate_menus_in_direction`）。
    #[test]
    fn menus_wrap_around_when_navigating() {
        assert_eq!(MenuKind::File.neighbour(1), MenuKind::Edit);
        assert_eq!(MenuKind::Edit.neighbour(-1), MenuKind::File);
        assert_eq!(
            MenuKind::Help.neighbour(1),
            MenuKind::File,
            "最后一个往右回第一个"
        );
        assert_eq!(
            MenuKind::File.neighbour(-1),
            MenuKind::Help,
            "第一个往左到最后一个"
        );
        // 不绕圈的多步（上游只能一步一步走，这里按项数取模）。
        assert_eq!(MenuKind::View.neighbour(2), MenuKind::Run);
        assert_eq!(
            MenuKind::Window.neighbour(-2),
            MenuKind::Go,
            "往回负步要算对"
        );
        // 跨过端点：Window 往后两步越过 Help，回到 File。
        assert_eq!(MenuKind::Window.neighbour(2), MenuKind::File);
    }

    /// 一个「从某个目录出发」的标题栏状态。
    fn title_bar() -> TitleBar {
        let mut bar = TitleBar::new();
        bar.set_project_at(&temp_dir_with_branch("title_bar", "feature/标题栏"));
        bar
    }

    /// 造一个带 `.git/HEAD` 的临时目录。
    fn temp_dir_with_branch(name: &str, branch: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aa_title_bar_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).expect("建 .git");
        std::fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n"))
            .expect("写 HEAD");
        dir
    }

    #[test]
    fn the_menu_bar_shows_zed_menus_in_order() {
        let bar = title_bar();
        let mut ui = iced_test::simulator(view(&bar, &THEME, |message| message));
        for kind in MenuKind::ALL {
            assert!(
                ui.find(kind.label()).is_ok(),
                "菜单栏上应有 {}",
                kind.label()
            );
        }
    }

    #[test]
    fn clicking_a_menu_reports_which_one() {
        let bar = title_bar();
        let mut ui = iced_test::simulator(view(&bar, &THEME, |message| message));
        assert!(ui.click("View").is_ok(), "点得到 View");
        let messages: Vec<Message> = ui.into_messages().collect();
        assert_eq!(messages, vec![Message::ShowMenu(MenuKind::View)]);
    }

    #[test]
    fn the_view_menu_toggles_the_sidebar() {
        let menu = menu(MenuKind::View, iced::Point::ORIGIN, |action| action);
        let mut ui = iced_test::simulator(menu.overlay(&THEME, MenuAction::Copy));
        assert!(ui.click("Toggle Sidebar").is_ok());
        let messages: Vec<MenuAction> = ui.into_messages().collect();
        assert_eq!(messages, vec![MenuAction::ToggleSidebar]);
    }

    /// 置灰的条目照上游显示，但点不动。
    #[test]
    fn entries_without_a_backend_are_disabled() {
        let menu = menu(MenuKind::File, iced::Point::ORIGIN, |action| action);
        let mut ui = iced_test::simulator(menu.overlay(&THEME, MenuAction::Copy));
        assert!(ui.find("New Window").is_ok(), "照上游显示（不省略）");
        assert!(ui.click("New Window").is_ok(), "画出来了就点得到");
        let messages: Vec<MenuAction> = ui.into_messages().collect();
        assert!(messages.is_empty(), "置灰条目不产消息：{messages:?}");
    }

    #[test]
    fn the_file_menu_starts_a_thread_and_quits() {
        let file_menu = menu(MenuKind::File, iced::Point::ORIGIN, |action| action);
        let mut ui = iced_test::simulator(file_menu.overlay(&THEME, MenuAction::Copy));
        assert!(ui.click("New Thread").is_ok());
        let messages: Vec<MenuAction> = ui.into_messages().collect();
        assert_eq!(messages, vec![MenuAction::NewThread]);

        // File > Quit 与 Window > Close Window 是同一个动作（单窗口应用里关掉最后
        // 一个窗口就是退出）。
        let quit_menu = menu(MenuKind::File, iced::Point::ORIGIN, |action| action);
        let mut ui = iced_test::simulator(quit_menu.overlay(&THEME, MenuAction::Copy));
        assert!(ui.click("Quit").is_ok());
        let messages: Vec<MenuAction> = ui.into_messages().collect();
        assert_eq!(messages, vec![MenuAction::CloseWindow]);
    }

    #[test]
    fn the_edit_and_window_menus_carry_their_actions() {
        for (kind, label, action) in [
            (MenuKind::Edit, "Copy", MenuAction::Copy),
            (MenuKind::Edit, "Paste", MenuAction::Paste),
            (MenuKind::Window, "Minimize", MenuAction::Minimize),
            (MenuKind::Window, "Maximize", MenuAction::Maximize),
            (MenuKind::Window, "Close Window", MenuAction::CloseWindow),
        ] {
            let menu = menu(kind, iced::Point::ORIGIN, |action| action);
            let mut ui = iced_test::simulator(menu.overlay(&THEME, MenuAction::Copy));
            assert!(ui.click(label).is_ok(), "{kind:?} 里点得到 {label}");
            let messages: Vec<MenuAction> = ui.into_messages().collect();
            assert_eq!(messages, vec![action], "{kind:?} > {label}");
        }
    }

    #[test]
    fn the_project_name_and_branch_are_shown() {
        let bar = title_bar();
        let mut ui = iced_test::simulator(view(&bar, &THEME, |message| message));

        // 夹具的目录名带进程号/线程号，超过上限会被截断 —— 查的就是画出来的那个
        // 名字（顺带钉住截断行为）。
        let name = bar.display_project_name().expect("有项目名");
        assert!(ui.find(name.as_str()).is_ok(), "项目名要画出来");
        assert!(ui.find("feature/标题栏").is_ok(), "分支要画出来");
        assert!(ui.find(PROJECT_ICON).is_ok(), "项目名带字形");
        assert!(ui.find(BRANCH_ICON).is_ok(), "分支带字形");
    }

    #[test]
    fn clicking_the_project_name_and_branch_reports_the_right_menu() {
        let bar = title_bar();

        let mut ui = iced_test::simulator(view(&bar, &THEME, |message| message));
        let name = bar.display_project_name().expect("有项目名");
        assert!(ui.click(name.as_str()).is_ok());
        let messages: Vec<Message> = ui.into_messages().collect();
        assert_eq!(messages, vec![Message::ShowProjectMenu]);

        let mut ui = iced_test::simulator(view(&bar, &THEME, |message| message));
        assert!(ui.click("feature/标题栏").is_ok());
        let messages: Vec<Message> = ui.into_messages().collect();
        assert_eq!(messages, vec![Message::ShowBranchMenu]);
    }

    #[test]
    fn the_project_and_branch_menus_carry_their_actions() {
        for (menu, label, action) in [
            (
                project_menu(iced::Point::ORIGIN, |action| action),
                "Copy Project Path",
                MenuAction::CopyProjectPath,
            ),
            (
                project_menu(iced::Point::ORIGIN, |action| action),
                "Reveal in File Manager",
                MenuAction::RevealProject,
            ),
            (
                branch_menu(iced::Point::ORIGIN, |action| action),
                "Copy Branch Name",
                MenuAction::CopyBranchName,
            ),
        ] {
            let mut ui = iced_test::simulator(menu.overlay(&THEME, MenuAction::Copy));
            assert!(ui.click(label).is_ok(), "点得到 {label}");
            let messages: Vec<MenuAction> = ui.into_messages().collect();
            assert_eq!(messages, vec![action], "{label}");
        }
    }

    #[test]
    fn a_directory_outside_a_repo_has_a_name_but_no_branch() {
        let dir = std::env::temp_dir().join(format!(
            "aa_title_bar_no_repo_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");

        let mut bar = TitleBar::new();
        assert!(bar.set_project_at(&dir), "第一次设必然有变化");
        assert_eq!(
            bar.project_name().map(str::to_owned),
            worktree::project_name(&dir)
        );
        assert_eq!(bar.branch(), None, "不在仓库里就没有分支");
        assert!(!bar.set_project_at(&dir), "同样的目录再设一次不算变化");
    }

    #[test]
    fn long_names_are_truncated_with_an_ellipsis() {
        assert_eq!(truncate("short", 40), "short");
        let long: String = "x".repeat(60);
        let truncated = truncate(&long, 40);
        assert_eq!(truncated.chars().count(), 40);
        assert!(truncated.ends_with('…'));
    }
}
