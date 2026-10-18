//! 工作树 —— 对应 Zed 的 `crates/worktree`，这里只搬标题栏要用的那一小块。
//!
//! 标题栏要显示「项目名 + 当前分支」（zed `title_bar` 的 `render_project_name` /
//! `render_worktree_and_branch`）。zed 那边这些信息来自 `project::Project` 里的
//! `git_store`（一个常驻的、会发事件的仓库句柄）；我们还没有那一层，所以这一版是
//! **每次现读**：
//!
//! - 从某个目录（终端会话的工作目录）往上找含 `.git` 的那一级 = 仓库根；
//! - 读 `.git/HEAD` 得出当前分支（detached 时给短 sha）。
//!
//! 受限之处（真接了 git store 就该换掉）：
//!
//! - 只认「目录里有没有 `.git`」，不看 `GIT_DIR`/`GIT_WORK_TREE` 环境变量；
//! - 不订阅变化 —— 用户在终端里 `git switch` 之后，标题栏不会自己更新（zed 会）。
//!   现在靠宿主在合适的时机重读（例如窗口重新获得焦点时）。
//! - 不解析 packed-refs / 远程分支，只看 HEAD 指向什么。

use std::path::{Path, PathBuf};

/// 短 sha 的长度（zed `title_bar` 的 `MAX_SHORT_SHA_LENGTH = 8`）。
const MAX_SHORT_SHA_LENGTH: usize = 8;

/// 从 `dir` 出发往上找仓库根：含 `.git` 的那一级（`.git` 是目录或文件都算，
/// 后者是 linked worktree）。
///
/// 找不到（不在仓库里）返回 `None`。
pub fn find_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

/// 项目名：仓库根目录的名字（zed 用的也是 worktree 根的 `file_name`）。
pub fn project_name(root: &Path) -> Option<String> {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// 当前分支。
///
/// HEAD 是 `ref: refs/heads/<name>` 时给 `<name>`（分支名可以带斜杠，
/// `feature/foo` 原样返回）；detached HEAD 时给前 8 位 sha（与 zed 的
/// `MAX_SHORT_SHA_LENGTH` 一致）。
pub fn branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir(root)?.join("HEAD")).ok()?;
    let head = head.trim();

    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        // `refs/heads/feature/foo` → `feature/foo`；别的 ref（tag、远程）原样给短名。
        let short = reference
            .strip_prefix("refs/heads/")
            .unwrap_or_else(|| reference.rsplit('/').next().unwrap_or(reference));
        return Some(short.to_owned());
    }

    // detached HEAD：HEAD 里直接是 40 位 sha。读不懂的内容不硬塞给界面
    // （宁可没有分支，也不显示一行垃圾）。
    let sha = head.trim();
    (sha.len() >= MAX_SHORT_SHA_LENGTH && sha.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| sha.chars().take(MAX_SHORT_SHA_LENGTH).collect())
}

/// 仓库的 git 目录：`.git` 是目录就用它；是文件（linked worktree）就顺着
/// `gitdir: <path>` 走一层（相对路径按仓库根解析，git 就是这么写的）。
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }

    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let target = contents.trim().strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个干净目录；名字带上进程号，避免并行跑时互相踩。
    ///
    /// 没有 `tempfile` 这个依赖（workspace 里还没有），就用 `std::env::temp_dir`。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aa_worktree_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 造一个「仓库」：写 `<root>/.git/HEAD`。
    fn repo(name: &str, head: &str) -> PathBuf {
        let root = temp_dir(name);
        std::fs::create_dir_all(root.join(".git")).expect("建 .git");
        std::fs::write(root.join(".git/HEAD"), head).expect("写 HEAD");
        root
    }

    #[test]
    fn the_root_is_the_closest_ancestor_holding_a_dot_git() {
        let root = repo("root", "ref: refs/heads/main\n");
        let nested = root.join("packages/sidebar/src");
        std::fs::create_dir_all(&nested).expect("建子目录");

        assert_eq!(find_root(&nested).as_deref(), Some(root.as_path()));
        // 项目名就是仓库根目录的名字（temp_dir 加的进程号/线程号都在，只断言前缀）。
        assert!(
            project_name(&root).is_some_and(|name| name.starts_with("aa_worktree_root_")),
            "项目名取仓库根的目录名"
        );
    }

    #[test]
    fn a_directory_outside_any_repo_has_no_root() {
        let dir = temp_dir("no_repo");
        assert_eq!(find_root(&dir), None);
        assert_eq!(branch(&dir), None);
    }

    #[test]
    fn the_branch_comes_from_head() {
        let root = repo("branch", "ref: refs/heads/main\n");
        assert_eq!(branch(&root).as_deref(), Some("main"));
    }

    #[test]
    fn slashes_in_a_branch_name_are_kept() {
        let root = repo("branch_slash", "ref: refs/heads/feature/title-bar\n");
        assert_eq!(branch(&root).as_deref(), Some("feature/title-bar"));
    }

    #[test]
    fn a_detached_head_shows_a_short_sha() {
        let root = repo("detached", "0123456789abcdef0123456789abcdef01234567\n");
        assert_eq!(
            branch(&root).as_deref(),
            Some("01234567"),
            "detached 时给前 8 位（zed MAX_SHORT_SHA_LENGTH）"
        );
    }

    /// linked worktree：`.git` 是文件，里面写着真正的 git 目录。
    #[test]
    fn a_dot_git_file_points_at_the_real_git_dir() {
        let root = temp_dir("linked");
        let git_dir = temp_dir("linked_gitdir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/linked-branch\n").expect("写 HEAD");
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("写 .git 文件");

        assert_eq!(find_root(&root).as_deref(), Some(root.as_path()));
        assert_eq!(branch(&root).as_deref(), Some("linked-branch"));
    }

    /// 相对路径的 `gitdir:` 按仓库根解析。
    #[test]
    fn a_relative_gitdir_is_resolved_from_the_root() {
        let root = temp_dir("linked_rel");
        std::fs::create_dir_all(root.join("common/worktrees/foo")).expect("建 git 目录");
        std::fs::write(
            root.join("common/worktrees/foo/HEAD"),
            "ref: refs/heads/rel-branch\n",
        )
        .expect("写 HEAD");
        std::fs::write(root.join(".git"), "gitdir: common/worktrees/foo\n").expect("写 .git 文件");

        assert_eq!(branch(&root).as_deref(), Some("rel-branch"));
    }

    /// 读不懂的 HEAD 不 panic，也不硬塞进界面 —— 当作没有分支。
    #[test]
    fn a_head_that_makes_no_sense_yields_no_branch() {
        assert_eq!(branch(&repo("broken", "垃圾内容\n")), None);
        assert_eq!(branch(&repo("empty_head", "\n")), None, "HEAD 空");
        assert_eq!(
            branch(&repo("short_sha", "abc\n")),
            None,
            "短得不像 sha 的也不认"
        );
    }

    #[test]
    fn a_missing_head_yields_no_branch() {
        let root = temp_dir("no_head");
        std::fs::create_dir_all(root.join(".git")).expect("建 .git");
        assert_eq!(branch(&root), None);
    }
}
