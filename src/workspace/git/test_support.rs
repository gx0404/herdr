//! git 相关测试的共享辅助。
//!
//! 测试构建下仓库发现无条件以临时根为上界（见 `discovery::discovery_ceilings`），因此
//! 临时目录之上的 `.git`（例如宿主机 `/.git`）不会被误认为测试目录的仓库；这不依赖
//! 调用顺序，但涉及 git 发现/状态的测试仍应经 `temp_test_dir` / `unique_temp_path`
//! 建目录，保证路径确实位于临时根之下。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 临时根下的唯一路径（不创建）：给 `git worktree add` 这类要求目标不存在的调用使用。
pub(crate) fn unique_temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
}

/// 创建独立的临时测试目录。
pub(crate) fn temp_test_dir(name: &str) -> PathBuf {
    let path = unique_temp_path(&format!("workspace-tests-{name}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

static GIT_VERSION: OnceLock<Option<(u32, u32, u32)>> = OnceLock::new();

/// 本机 `git --version` 探测结果（进程内缓存）；探测失败返回 `None`。
pub(crate) fn git_version() -> Option<(u32, u32, u32)> {
    *GIT_VERSION.get_or_init(|| {
        let output = std::process::Command::new("git")
            .arg("--version")
            .output()
            .ok()?;
        parse_git_version(&String::from_utf8_lossy(&output.stdout))
    })
}

/// 解析形如 `git version 2.25.1`、`git version 2.39.2 (Apple Git-143)`、
/// `git version 2.45.0.windows.1` 的输出。
pub(crate) fn parse_git_version(text: &str) -> Option<(u32, u32, u32)> {
    let token = text
        .trim()
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = token.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// is-at-least 风格比较：本机 git 不低于 `major.minor`。探测失败按旧版本处理，
/// 让调用方走在任何版本都成立的回退路径。
pub(crate) fn git_at_least(major: u32, minor: u32) -> bool {
    git_version()
        .is_some_and(|(have_major, have_minor, _)| (have_major, have_minor) >= (major, minor))
}

/// 在 `repo` 初始化仓库并把 HEAD 指向 `branch`：`git init -b` 自 git 2.28 起可用，
/// 更早版本回退为 `git init` + `git symbolic-ref HEAD`。
pub(crate) fn init_repo_on_branch(repo: &Path, branch: &str) {
    std::fs::create_dir_all(repo).unwrap();
    if git_at_least(2, 28) {
        run_git(repo, &["init", "--quiet", "-b", branch]);
    } else {
        run_git(repo, &["init", "--quiet"]);
        run_git(
            repo,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        );
    }
}

fn init_repo_with_commit(repo: &Path) {
    init_repo_on_branch(repo, "main");
    run_git(repo, &["config", "user.email", "herdr@example.invalid"]);
    run_git(repo, &["config", "user.name", "Herdr Test"]);
    run_git(
        repo,
        &["commit", "--quiet", "--allow-empty", "-m", "initial"],
    );
}

pub(crate) fn create_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let repo = base.join("herdr");
    let checkout = base.join("testr56");
    init_repo_with_commit(&repo);
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "testr56",
            checkout.to_str().unwrap(),
            "HEAD",
        ],
    );
    (base, repo, checkout)
}

pub(crate) fn create_bare_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let seed = base.join("seed");
    let bare = base.join(".bare");
    let checkout = base.join("feature");
    init_repo_with_commit(&seed);
    run_git(
        &base,
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    run_git(
        &bare,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            checkout.to_str().unwrap(),
            "HEAD",
        ],
    );
    (base, bare, checkout)
}

pub(crate) fn write_fake_tracked_repo(root: &Path) {
    let head_oid = "1111111111111111111111111111111111111111";
    let upstream_oid = "2222222222222222222222222222222222222222";
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join(".git/refs/heads/main"), format!("{head_oid}\n")).unwrap();
    std::fs::write(
        root.join(".git/refs/remotes/origin/main"),
        format!("{upstream_oid}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    )
    .unwrap();
}

pub(crate) fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod tests {
    use super::parse_git_version;

    #[test]
    fn git_version_parses_common_vendor_suffixes() {
        assert_eq!(parse_git_version("git version 2.25.1\n"), Some((2, 25, 1)));
        assert_eq!(
            parse_git_version("git version 2.39.2 (Apple Git-143)"),
            Some((2, 39, 2))
        );
        assert_eq!(
            parse_git_version("git version 2.45.0.windows.1"),
            Some((2, 45, 0))
        );
        assert_eq!(parse_git_version("git version 2.36"), Some((2, 36, 0)));
        assert_eq!(
            parse_git_version("git version 2.50.0-rc1"),
            Some((2, 50, 0))
        );
        assert_eq!(parse_git_version("not git"), None);
    }
}
