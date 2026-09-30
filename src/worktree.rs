use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_WORKTREE_PREFIX: &str = "worktree";

/// git 删除中途失败（`failed to delete`）且 worktree 已注销后，收尾残留的总时限：瞬态（杀毒
/// 扫描、尚在退出的 pane 进程）通常很快过去，只有残留确实删不掉时才会等满。
const FAILED_DELETE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// 收尾各轮之间的间隔：从 50ms 起翻倍、封顶 1s，5 秒内最多扫十轮，而不是每 100ms 一轮。
const FAILED_DELETE_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(50);
const FAILED_DELETE_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
    /// porcelain 的 `locked` 行（git 2.31 起随 `prunable` 一同输出）：git 对加锁的 worktree
    /// 从不判定 prunable，回退判据优先采信它。
    pub is_locked: bool,
}

pub(crate) fn generated_branch_slug(seed: u64) -> String {
    let adjectives = [
        "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
    ];
    let nouns = [
        "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
    ];
    let adjective = adjectives[(seed as usize) % adjectives.len()];
    let noun = nouns[((seed / adjectives.len() as u64) as usize) % nouns.len()];
    let suffix = seed & 0xffff;
    format!("{DEFAULT_WORKTREE_PREFIX}/{adjective}-{noun}-{suffix:04x}")
}

pub(crate) fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        DEFAULT_WORKTREE_PREFIX.to_string()
    } else {
        trimmed
    }
}

pub(crate) fn expand_tilde_path(path: &str) -> PathBuf {
    expand_tilde_path_from_env(path, cfg!(windows), |key| std::env::var_os(key))
}

fn expand_tilde_path_from_env(
    path: &str,
    is_windows: bool,
    env: impl Fn(&str) -> Option<OsString> + Copy,
) -> PathBuf {
    if path == "~" {
        return home_dir_from_env(is_windows, env).unwrap_or_else(|_| PathBuf::from(path));
    }

    let tilde_rest = path.strip_prefix("~/").or_else(|| {
        if is_windows {
            path.strip_prefix("~\\")
        } else {
            None
        }
    });
    if let Some(rest) = tilde_rest {
        return home_dir_from_env(is_windows, env)
            .map(|home| join_tilde_rest(home, rest, is_windows))
            .unwrap_or_else(|_| PathBuf::from(path));
    }

    PathBuf::from(path)
}

fn join_tilde_rest(home: PathBuf, rest: &str, is_windows: bool) -> PathBuf {
    if is_windows {
        rest.split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .fold(home, |path, component| path.join(component))
    } else {
        home.join(rest)
    }
}

fn home_dir_from_env(
    is_windows: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, ()> {
    if !is_windows {
        return env("HOME").map(PathBuf::from).ok_or(());
    }

    if let Some(path) = usable_home_path(env("USERPROFILE")) {
        return Ok(path);
    }
    if let (Some(drive), Some(path)) = (
        usable_home_component(env("HOMEDRIVE")),
        usable_home_component(env("HOMEPATH")),
    ) {
        let path = path.to_string_lossy();
        if !path.starts_with(['\\', '/']) {
            return usable_home_path(env("HOME")).ok_or(());
        }
        let combined = format!("{}{}", drive.to_string_lossy(), path);
        if let Some(path) = usable_home_path(Some(OsString::from(combined))) {
            return Ok(path);
        }
    }

    usable_home_path(env("HOME")).ok_or(())
}

fn usable_home_path(value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() || value == "~" {
        return None;
    }
    Some(PathBuf::from(value))
}

fn usable_home_component(value: Option<OsString>) -> Option<OsString> {
    let value = value?;
    if value.is_empty() || value == "~" {
        return None;
    }
    Some(value)
}

pub(crate) fn expand_tilde_absolute_path(path: &str) -> PathBuf {
    let path = expand_tilde_path(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

pub(crate) fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn repository_git_command(repo_root: &Path, trust_repository: bool) -> std::process::Command {
    let mut command = crate::noninteractive_process::command("git");
    command.args(repository_git_args(repo_root, trust_repository));
    command
}

fn repository_git_args(repo_root: &Path, trust_repository: bool) -> Vec<String> {
    let mut args = Vec::new();
    if trust_repository {
        args.push("-c".to_string());
        args.push(format!("safe.directory={}", repo_root.display()));
    }
    args.push("-C".to_string());
    args.push(repo_root.display().to_string());
    args
}

pub(crate) fn default_checkout_path(root: &Path, repo_name: &str, branch: &str) -> PathBuf {
    root.join(repo_name).join(branch_to_path_slug(branch))
}

pub(crate) fn build_worktree_remove_command(
    repo_root: &Path,
    path: &Path,
    force: bool,
    trust_repository: bool,
) -> WorktreeCommand {
    let mut args = repository_git_args(repo_root, trust_repository);
    args.extend(["worktree".to_string(), "remove".to_string()]);
    if force {
        args.push("--force".to_string());
    }
    args.push(path.display().to_string());

    WorktreeCommand {
        program: "git".to_string(),
        args,
    }
}

pub(crate) fn is_dirty_worktree_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("contains modified or untracked files")
        && lower.contains("use --force to delete it"))
        || lower.contains("working trees containing submodules cannot be moved or removed")
}

pub(crate) fn is_not_working_tree_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("is not a working tree") || lower.contains("is not a worktree")
}

/// git 过了删除前的检查后，删检出或管理目录时的任何失败都报
/// `error: failed to delete '<path>': <原因>`：Windows 上杀毒扫描或索引器恰在此时持有、新建
/// 条目（`Directory not empty`），尚在退出的 pane 进程的 cwd 仍在检出里（`Permission
/// denied`）等。git 此时已不回头，删检出失败也会接着删管理目录。按行首匹配，路径里碰巧含
/// 这段文字的其它报错不算。
fn is_failed_delete_remove_error(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .lines()
        .any(|line| line.starts_with("error: failed to delete '"))
}

#[cfg(windows)]
pub(crate) fn worktree_dirty_remove_message(path: &Path) -> String {
    format!(
        "fatal: '{}' contains modified or untracked files, use --force to delete it",
        path.display()
    )
}

#[cfg(any(windows, test))]
pub(crate) fn checkout_has_dirty_files(
    path: &Path,
    trust_repository: bool,
) -> Result<bool, String> {
    let output = repository_git_command(path, trust_repository)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(!output.stdout.is_empty());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        Err(stderr)
    } else if !stdout.is_empty() {
        Err(stdout)
    } else {
        Err(format!("git status failed with status {}", output.status))
    }
}

pub(crate) fn build_worktree_add_new_branch_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
    trust_repository: bool,
) -> WorktreeCommand {
    let mut args = repository_git_args(repo_root, trust_repository);
    args.extend([
        "worktree".to_string(),
        "add".to_string(),
        "-b".to_string(),
        branch.to_string(),
        path.display().to_string(),
        base.to_string(),
    ]);
    WorktreeCommand {
        program: "git".to_string(),
        args,
    }
}

pub(crate) fn build_worktree_add_existing_branch_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    trust_repository: bool,
) -> WorktreeCommand {
    let mut args = repository_git_args(repo_root, trust_repository);
    args.extend([
        "worktree".to_string(),
        "add".to_string(),
        path.display().to_string(),
        branch.to_string(),
    ]);
    WorktreeCommand {
        program: "git".to_string(),
        args,
    }
}

fn local_branch_exists(
    repo_root: &Path,
    branch: &str,
    trust_repository: bool,
) -> Result<bool, String> {
    let output = repository_git_command(repo_root, trust_repository)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        Err(stderr)
    } else if !stdout.is_empty() {
        Err(stdout)
    } else {
        Err(format!("git show-ref failed with status {}", output.status))
    }
}

pub(crate) fn run_worktree_add_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
    trust_repository: bool,
) -> Result<(), String> {
    let command = if local_branch_exists(repo_root, branch, trust_repository)? {
        build_worktree_add_existing_branch_command(repo_root, path, branch, trust_repository)
    } else {
        build_worktree_add_new_branch_command(repo_root, path, branch, base, trust_repository)
    };
    run_worktree_command(&command)
}

pub(crate) fn run_worktree_command(command: &WorktreeCommand) -> Result<(), String> {
    let output = crate::noninteractive_process::command(&command.program)
        // Removal errors are classified by Git's English diagnostics.
        .env("LC_ALL", "C")
        .args(&command.args)
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("{} failed with status {}", command.program, output.status)
    } else {
        message
    })
}

pub(crate) fn run_worktree_remove_command_with_recovery(
    command: &WorktreeCommand,
    repo_root: &Path,
    path: &Path,
    force: bool,
    trust_repository: bool,
) -> Result<(), String> {
    run_worktree_command(command).or_else(|err| {
        recover_worktree_remove_error(
            err,
            repo_root,
            path,
            force,
            trust_repository,
            FAILED_DELETE_RECOVERY_TIMEOUT,
        )
    })
}

/// 按 git 报错判断失败的删除能否视为已完成；其余报错原样返回。
fn recover_worktree_remove_error(
    err: String,
    repo_root: &Path,
    path: &Path,
    force: bool,
    trust_repository: bool,
    failed_delete_timeout: Duration,
) -> Result<(), String> {
    if is_failed_delete_remove_error(&err) {
        return recover_failed_delete_remove(
            err,
            repo_root,
            path,
            trust_repository,
            failed_delete_timeout,
        );
    }
    if !force || !is_not_working_tree_remove_error(&err) {
        return Err(err);
    }
    if worktree_list_contains_path(repo_root, path, trust_repository)? {
        return Err(err);
    }
    if path.exists() {
        if !leftover_worktree_checkout_matches_repo(repo_root, path, trust_repository) {
            return Err(err);
        }
        std::fs::remove_dir_all(path).map_err(|remove_err| {
            format!(
                "{err}; failed to remove leftover checkout {}: {remove_err}",
                path.display()
            )
        })?;
    }
    Ok(())
}

/// git 删除中途失败后仍会接着删管理目录，所以报 `failed to delete` 时 worktree 通常已注销，
/// 只剩残留检出目录和/或不带 `gitdir` 的孤儿管理目录（`git worktree list` 看不到，`git gc`
/// 会清理）。此时重试只会报 "is not a working tree"，force 也因检出里的 `.git` 已删而认不出
/// 残留，只能在这里收尾。不跑 `git worktree prune`：它会连带清掉无关的陈旧 worktree（例如
/// 位于未挂载盘上的）。
///
/// git 已退出、herdr 也不 prune，注册状态之后不会再变：只查一次，仍注册就原样返回 git 的
/// 报错。已注销则在时限内收尾残留（见 `clean_up_unregistered_leftovers`）：只删空目录，文件
/// 一律保留——可能是删除期间新写入的真实数据，也可能是 git 在第一处失败后没再删到的检出
/// 内容。检出路径消失即成功；到时仍有残留也算成功并告警（git 视角已删除，保持 herdr
/// workspace 状态一致更重要）。
fn recover_failed_delete_remove(
    err: String,
    repo_root: &Path,
    path: &Path,
    trust_repository: bool,
    timeout: Duration,
) -> Result<(), String> {
    let registered = worktree_list_contains_path(repo_root, path, trust_repository)
        .map_err(|list_err| format!("{err}; failed to check worktree registration: {list_err}"))?;
    if registered {
        return Err(err);
    }
    let cleanup = clean_up_unregistered_leftovers(path, timeout);
    if cleanup.removed {
        tracing::info!(
            path = %path.display(),
            err = %err,
            sweeps = cleanup.sweeps,
            "worktree removal recovered after git failed to delete part of it"
        );
    } else {
        tracing::warn!(
            path = %path.display(),
            kept_entry = ?cleanup.kept,
            err = %err,
            sweeps = cleanup.sweeps,
            "worktree was removed from git, but leftover entries remain in its checkout \
             directory {}; they were kept and the directory must be removed by hand",
            path.display()
        );
    }
    Ok(())
}

/// 已注销 worktree 的残留收尾结果。
struct LeftoverCleanup {
    /// 检出路径已不存在。
    removed: bool,
    /// 扫过整棵残留树的轮数。
    sweeps: u32,
    /// 最近一轮遇到的一个永不删除的条目（文件、符号链接或 junction）。
    kept: Option<PathBuf>,
}

/// 时限内分轮收尾：每轮自底向上删空目录，删不掉的（例如仍是某进程的 cwd）等它释放，轮间
/// 间隔指数退避。残留里只要还有一个永不删除的条目，检出目录就不可能删掉，所以记下的那个
/// 条目还在时只看它一眼、不再重扫整棵树（`node_modules`、`target/` 这类大残留只扫一遍）；
/// 它消失了才重扫。
fn clean_up_unregistered_leftovers(path: &Path, timeout: Duration) -> LeftoverCleanup {
    let deadline = Instant::now() + timeout;
    let mut backoff = FAILED_DELETE_RETRY_INITIAL_BACKOFF;
    let mut cleanup = LeftoverCleanup {
        removed: false,
        sweeps: 0,
        kept: None,
    };
    loop {
        if !cleanup.kept.as_deref().is_some_and(leftover_entry_remains) {
            cleanup.kept = None;
            cleanup.sweeps += 1;
            if remove_empty_dirs_bottom_up(path, deadline, &mut cleanup.kept) {
                cleanup.removed = true;
                return cleanup;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return cleanup;
        }
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(FAILED_DELETE_RETRY_MAX_BACKOFF);
    }
}

/// 条目仍占着路径：读元数据被拒（例如删除挂起）也算还在，它照样挡着上层目录的删除。
fn leftover_entry_remains(entry: &Path) -> bool {
    match std::fs::symlink_metadata(entry) {
        Ok(_) => true,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

/// 自底向上删除 `dir` 及其下的空目录，返回 `dir` 是否已不存在。只删真实目录，不跟随链接：
/// 文件、符号链接与 junction 一律保留，遇到的第一个记进 `kept`；已知非空的目录不再试
/// `remove_dir`。每个条目前都看 `deadline`，过了就停手，大残留树不会让调用拖过时限。
fn remove_empty_dirs_bottom_up(dir: &Path, deadline: Instant, kept: &mut Option<PathBuf>) -> bool {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            kept.get_or_insert_with(|| dir.to_path_buf());
            return false;
        }
        Err(error) => {
            return matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            )
        }
    }
    let mut emptied = true;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if Instant::now() >= deadline {
                return false;
            }
            let entry_path = entry.path();
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                emptied &= remove_empty_dirs_bottom_up(&entry_path, deadline, kept);
            } else {
                kept.get_or_insert(entry_path);
                emptied = false;
            }
        }
    }
    emptied
        && match std::fs::remove_dir(dir) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
}

fn leftover_worktree_checkout_matches_repo(
    repo_root: &Path,
    path: &Path,
    trust_repository: bool,
) -> bool {
    let git_file = path.join(".git");
    let Ok(content) = std::fs::read_to_string(&git_file) else {
        return false;
    };
    let Some(gitdir) = content.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.join(gitdir)
    };
    let Some(worktrees_dir) = git_common_worktrees_dir(repo_root, trust_repository) else {
        return false;
    };
    canonical_or_original(&gitdir).starts_with(canonical_or_original(&worktrees_dir))
}

fn git_common_worktrees_dir(repo_root: &Path, trust_repository: bool) -> Option<PathBuf> {
    let output = repository_git_command(repo_root, trust_repository)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let common_dir = stdout.trim();
    if common_dir.is_empty() {
        None
    } else {
        let common_dir = PathBuf::from(common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            repo_root.join(common_dir)
        };
        Some(common_dir.join("worktrees"))
    }
}

pub(crate) fn parse_worktree_list_porcelain(output: &str) -> Vec<ExistingWorktree> {
    let mut entries = Vec::new();
    let mut current: Option<ExistingWorktree> = None;

    for line in output.lines() {
        if line.trim().is_empty() {
            entries.extend(current.take());
            continue;
        }
        if let Some(value) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(ExistingWorktree {
                path: PathBuf::from(value),
                branch: None,
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_locked: false,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(value) = line.strip_prefix("branch ") {
            entry.branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value)
                    .to_string(),
            );
        } else if line == "detached" {
            entry.is_detached = true;
        } else if line == "bare" {
            entry.is_bare = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            entry.is_prunable = true;
        } else if line == "locked" || line.starts_with("locked ") {
            entry.is_locked = true;
        }
    }

    entries.extend(current.take());
    entries
}

pub(crate) fn list_existing_worktrees(
    repo_root: &Path,
    trust_repository: bool,
) -> Result<Vec<ExistingWorktree>, String> {
    #[cfg(test)]
    test_list_gate::wait(repo_root);
    let output = repository_git_command(repo_root, trust_repository)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut entries = parse_worktree_list_porcelain(&stdout);
        mark_missing_linked_checkouts_prunable(&mut entries, repo_root, trust_repository);
        return Ok(entries);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("git worktree list failed with status {}", output.status)
    } else {
        stderr
    })
}

/// git 2.31 之前的 `worktree list --porcelain` 没有 `prunable`/`locked` 行。按 git 自身
/// `should_prune_worktree` 的判据回退：linked worktree 的 `.git` gitdir 文件已不存在且该
/// worktree 未加锁，即视为 prunable。
///
/// 回退只补充、不覆盖 git 的结论：porcelain 已标 `prunable` 的条目保持不变；已标 `locked` 的
/// 条目直接跳过；只有 porcelain 两者都没标、且 `.git` 确实缺失的条目才去扫 admin 目录确认
/// 未加锁。在带注解的 git 版本上，这样的条目 git 自己也会判定 prunable，因此两者一致。
fn mark_missing_linked_checkouts_prunable(
    entries: &mut [ExistingWorktree],
    repo_root: &Path,
    trust_repository: bool,
) {
    let mut locked_targets: Option<HashSet<PathBuf>> = None;
    // git 总是先列出主 worktree，只有其后的 linked worktree 会被 prune。
    for entry in entries.iter_mut().skip(1) {
        if entry.is_bare || entry.is_prunable || entry.is_locked {
            continue;
        }
        let git_file = entry.path.join(".git");
        if !checkout_git_file_is_missing(&git_file) {
            continue;
        }
        let locked = locked_targets
            .get_or_insert_with(|| locked_worktree_gitdir_targets(repo_root, trust_repository));
        if locked.contains(&forgiving_real_path(&git_file)) {
            continue;
        }
        entry.is_prunable = true;
    }
}

/// 只把「不存在」视为缺失；权限等错误保守地当作仍然存在，避免把可用检出标成 prunable。
fn checkout_git_file_is_missing(git_file: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(git_file),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            )
    )
}

/// 已加锁 worktree 的 gitdir 目标（`<common>/worktrees/<id>/gitdir` 的内容）集合：
/// git 对加锁的 worktree 从不判定 prunable。
fn locked_worktree_gitdir_targets(repo_root: &Path, trust_repository: bool) -> HashSet<PathBuf> {
    let Some(worktrees_dir) = git_common_worktrees_dir(repo_root, trust_repository) else {
        return HashSet::new();
    };
    locked_gitdir_targets_in_admin_dir(&worktrees_dir)
}

/// 扫描 `<common>/worktrees/*`：带 `locked` 标记的 admin 目录，其 `gitdir` 内容即目标。
/// git 2.48 起（`worktree.useRelativePaths` / `--relative-paths`）该内容可能是相对于
/// admin 目录的相对路径，与检出侧 `.git` 文件的处理一致：相对则以所在目录为基准解析。
fn locked_gitdir_targets_in_admin_dir(worktrees_dir: &Path) -> HashSet<PathBuf> {
    let mut targets = HashSet::new();
    let Ok(admin_dirs) = std::fs::read_dir(worktrees_dir) else {
        return targets;
    };
    for admin_dir in admin_dirs.flatten().map(|entry| entry.path()) {
        if std::fs::symlink_metadata(admin_dir.join("locked")).is_err() {
            continue;
        }
        let Ok(gitdir) = std::fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = Path::new(gitdir.trim());
        if gitdir.as_os_str().is_empty() {
            continue;
        }
        let gitdir = if gitdir.is_absolute() {
            gitdir.to_path_buf()
        } else {
            admin_dir.join(gitdir)
        };
        targets.insert(forgiving_real_path(&gitdir));
    }
    targets
}

/// 同 git `strbuf_realpath_forgiving`：解析最深的存在祖先的真实路径，其余组件按字面追加。
/// 目标缺失（这正是 prunable 判据关心的情形）时，绝对与相对写法仍能规范到同一路径比较。
fn forgiving_real_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&current) {
            return missing.into_iter().rev().fold(real, |mut acc, name| {
                acc.push(name);
                acc
            });
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_list_gate {
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    /// 等测试放行的上限，与测试侧 `LOADED_WAIT` 一致：负载高时测试线程要很久才走到放行，
    /// 一放行即返回，只有测试确实忘了放行才会等满。
    const RELEASE_WAIT: Duration = Duration::from_secs(30);

    struct Gate {
        path: PathBuf,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());

    pub(crate) fn block(path: &Path) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, entry_rx) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        GATES.lock().unwrap().push(Gate {
            path: super::canonical_or_original(path),
            entered,
            release,
        });
        (entry_rx, release_tx)
    }

    pub(super) fn wait(path: &Path) {
        let gate = {
            let mut gates = GATES.lock().unwrap();
            gates
                .iter()
                .position(|gate| gate.path == super::canonical_or_original(path))
                .map(|index| gates.remove(index))
        };
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            gate.release
                .recv_timeout(RELEASE_WAIT)
                .expect("test must release worktree discovery without blocking the server loop");
        }
    }
}

/// 按 `forgiving_real_path` 比较：删除恢复时检出目录往往已不存在，仍要把经由短文件名或
/// 符号链接祖先的写法与 git 记录的真实路径对上，否则仍注册的 worktree 会被当成已注销。
fn worktree_list_contains_path(
    repo_root: &Path,
    path: &Path,
    trust_repository: bool,
) -> Result<bool, String> {
    let expected = forgiving_real_path(path);
    Ok(list_existing_worktrees(repo_root, trust_repository)?
        .into_iter()
        .any(|entry| forgiving_real_path(&entry.path) == expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 临时路径统一经共享辅助创建：测试构建下仓库发现以临时根为上界（见
    // `workspace::git::discovery::discovery_ceilings`），宿主机上层 `.git` 不会干扰。
    use crate::workspace::git_test_support::unique_temp_path;

    fn run_git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git command failed: git -C {} {}",
            repo.display(),
            args.join(" ")
        );
    }

    fn create_committed_repo(name: &str) -> PathBuf {
        let repo = unique_temp_path(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Herdr Test"]);
        std::fs::write(repo.join("README.md"), "test\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    #[test]
    fn trusted_repository_git_args_are_request_scoped() {
        assert_eq!(
            repository_git_args(Path::new("/repo/herdr"), false),
            ["-C", "/repo/herdr"]
        );
        assert_eq!(
            repository_git_args(Path::new("/repo/herdr"), true),
            ["-c", "safe.directory=/repo/herdr", "-C", "/repo/herdr",]
        );
    }

    #[test]
    fn generated_branch_slug_is_worktree_namespaced_and_stable() {
        assert_eq!(generated_branch_slug(0), "worktree/brave-river-0000");
        assert_eq!(generated_branch_slug(9), "worktree/calm-cloud-0009");
    }

    #[test]
    fn parses_git_worktree_list_porcelain() {
        let output = "\
worktree /repo/main
HEAD abc
branch refs/heads/main

worktree /repo/issue
HEAD def
branch refs/heads/worktree/issue
locked on removable media

worktree /repo/detached
HEAD fed
detached
prunable stale

";

        assert_eq!(
            parse_worktree_list_porcelain(output),
            vec![
                ExistingWorktree {
                    path: PathBuf::from("/repo/main"),
                    branch: Some("main".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                    is_locked: false,
                },
                ExistingWorktree {
                    path: PathBuf::from("/repo/issue"),
                    branch: Some("worktree/issue".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                    is_locked: true,
                },
                ExistingWorktree {
                    path: PathBuf::from("/repo/detached"),
                    branch: None,
                    is_bare: false,
                    is_detached: true,
                    is_prunable: true,
                    is_locked: false,
                },
            ]
        );
    }

    #[test]
    fn branch_to_path_slug_makes_branch_safe_folder_name() {
        assert_eq!(
            branch_to_path_slug("worktree/brave-river"),
            "worktree-brave-river"
        );
        assert_eq!(
            branch_to_path_slug("issue/137 Worktree Spaces"),
            "issue-137-worktree-spaces"
        );
        assert_eq!(branch_to_path_slug("///"), "worktree");
    }

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        assert_eq!(
            expand_tilde_path_from_env("~/.herdr/worktrees", false, |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from("/home/me/.herdr/worktrees")
        );
        assert_eq!(
            expand_tilde_path_from_env("/tmp/worktrees", false, |_| None),
            PathBuf::from("/tmp/worktrees")
        );
    }

    #[test]
    fn home_dir_uses_windows_profile_before_literal_home() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOME" => Some("~".into()),
                "USERPROFILE" => Some(r"C:\Users\herdr".into()),
                _ => None,
            }),
            Ok(PathBuf::from(r"C:\Users\herdr"))
        );
    }

    #[test]
    fn home_dir_uses_windows_drive_and_path_when_profile_is_missing() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some(r"\Users\herdr".into()),
                _ => None,
            }),
            Ok(PathBuf::from(r"C:\Users\herdr"))
        );
    }

    #[test]
    fn home_dir_rejects_incomplete_windows_drive_and_path() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some("".into()),
                _ => None,
            }),
            Err(())
        );
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some("Users\\herdr".into()),
                _ => None,
            }),
            Err(())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_tilde_expansion_keeps_windows_separator_literal() {
        assert_eq!(
            expand_tilde_path_from_env(r"~\.herdr\worktrees", false, |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from(r"~\.herdr\worktrees")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_tilde_expansion_normalizes_separators() {
        fn env(key: &str) -> Option<OsString> {
            match key {
                "HOME" => Some("~".into()),
                "USERPROFILE" => Some(r"C:\Users\herdr".into()),
                _ => None,
            }
        }

        let default_path = expand_tilde_path_from_env("~/.herdr/worktrees", true, env);
        assert_eq!(
            default_path,
            PathBuf::from(r"C:\Users\herdr\.herdr\worktrees")
        );
        assert_eq!(
            default_path.display().to_string(),
            r"C:\Users\herdr\.herdr\worktrees"
        );
        assert_eq!(
            expand_tilde_path_from_env(r"~\.herdr\worktrees", true, env),
            PathBuf::from(r"C:\Users\herdr\.herdr\worktrees")
        );
    }

    #[test]
    fn default_checkout_path_appends_repo_and_branch_slug() {
        assert_eq!(
            default_checkout_path(
                Path::new("/home/me/.herdr/worktrees"),
                "herdr",
                "worktree/brave-river",
            ),
            PathBuf::from("/home/me/.herdr/worktrees/herdr/worktree-brave-river")
        );
    }

    #[test]
    fn checkout_dirty_detection_reports_clean_and_dirty_worktrees() {
        let repo = create_committed_repo("worktree-dirty-detection-repo");
        let checkout = unique_temp_path("worktree-dirty-detection-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/dirty-detection",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        assert_eq!(checkout_has_dirty_files(&checkout, false), Ok(false));
        std::fs::write(checkout.join("README.md"), "dirty\n").unwrap();
        assert_eq!(checkout_has_dirty_files(&checkout, false), Ok(true));

        let remove = build_worktree_remove_command(&repo, &checkout, true, false);
        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn worktree_remove_command_preserves_branch_by_not_deleting_it() {
        let command = build_worktree_remove_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/issue-137"),
            false,
            false,
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "remove",
                "/w/herdr/issue-137"
            ]
        );
    }

    #[test]
    fn forced_worktree_remove_command_uses_git_force_flag() {
        let command = build_worktree_remove_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/issue-137"),
            true,
            false,
        );
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "remove",
                "--force",
                "/w/herdr/issue-137"
            ]
        );
    }

    #[test]
    fn dirty_remove_error_detection_matches_git_force_hint() {
        assert!(is_dirty_worktree_remove_error(
            "fatal: '/w/herdr' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_worktree_remove_error(
            "fatal: '/w/herdr' is a missing but already registered worktree"
        ));
        assert!(!is_dirty_worktree_remove_error(
            "fatal: '/w/herdr' contains a locked worktree, use --force only if you know why"
        ));
    }

    #[test]
    fn submodule_remove_error_requires_force_confirmation() {
        assert!(is_dirty_worktree_remove_error(
            "fatal: working trees containing submodules cannot be moved or removed"
        ));
    }

    #[test]
    fn submodule_worktree_removal_requires_explicit_force() {
        let repo = create_committed_repo("submodule-remove-repo");
        let submodule = create_committed_repo("submodule-remove-source");
        let checkout = unique_temp_path("submodule-remove-checkout");
        run_git(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                submodule.to_str().unwrap(),
                "sub",
            ],
        );
        run_git(&repo, &["commit", "--quiet", "-am", "add submodule"]);
        let add = build_worktree_add_new_branch_command(
            &repo,
            &checkout,
            "worktree/submodule-remove",
            "HEAD",
            false,
        );
        run_worktree_command(&add).unwrap();
        run_git(
            &checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--quiet",
            ],
        );
        assert!(!checkout_has_dirty_files(&checkout, false).unwrap());

        let remove = build_worktree_remove_command(&repo, &checkout, false, false);
        let error = run_worktree_command(&remove).unwrap_err();
        assert!(is_dirty_worktree_remove_error(&error), "{error}");
        assert!(checkout.join("sub/README.md").exists());

        std::fs::write(checkout.join("sub/README.md"), "uncommitted change\n").unwrap();
        let error = run_worktree_command(&remove).unwrap_err();
        assert!(is_dirty_worktree_remove_error(&error), "{error}");
        assert!(checkout.join("sub/README.md").exists());

        let forced = build_worktree_remove_command(&repo, &checkout, true, false);
        run_worktree_remove_command_with_recovery(&forced, &repo, &checkout, true, false).unwrap();
        assert!(!checkout.exists());
        assert!(!worktree_list_contains_path(&repo, &checkout, false).unwrap());
        assert!(repo.join("sub/README.md").exists());

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(submodule).unwrap();
    }

    #[test]
    fn worktree_add_command_creates_new_branch_from_base() {
        let command = build_worktree_add_new_branch_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/worktree-brave-river"),
            "worktree/brave-river",
            "HEAD",
            false,
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "add",
                "-b",
                "worktree/brave-river",
                "/w/herdr/worktree-brave-river",
                "HEAD"
            ]
        );
    }

    #[test]
    fn worktree_add_command_checks_out_existing_branch() {
        let command = build_worktree_add_existing_branch_command(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/worktree-brave-river"),
            "worktree/brave-river",
            false,
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/herdr",
                "worktree",
                "add",
                "/w/herdr/worktree-brave-river",
                "worktree/brave-river"
            ]
        );
    }

    #[test]
    fn run_worktree_add_and_remove_create_and_delete_checkout() {
        let repo = create_committed_repo("worktree-run-repo");
        let checkout = unique_temp_path("worktree-run-checkout");
        let branch = "worktree/test-create-remove";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD", false);
        run_worktree_command(&add).unwrap();

        assert!(checkout.join("README.md").exists());
        let branch_name = std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert!(branch_name.status.success());
        assert_eq!(
            String::from_utf8(branch_name.stdout).unwrap().trim(),
            branch
        );

        let remove = build_worktree_remove_command(&repo, &checkout, false, false);
        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, false, false).unwrap();
        assert!(!checkout.exists());

        let _ = std::fs::remove_dir_all(repo);
    }

    fn raw_worktree_list_porcelain(repo: &Path) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn missing_linked_checkout_is_prunable_on_every_git_version() {
        use crate::workspace::git_test_support::git_at_least;

        let repo = create_committed_repo("worktree-prunable-fallback-repo");
        let checkout = unique_temp_path("worktree-prunable-fallback-checkout");
        let branch = "worktree/prunable-fallback";
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::remove_dir_all(&checkout).unwrap();

        let raw = raw_worktree_list_porcelain(&repo);
        let porcelain_only = parse_worktree_list_porcelain(&raw);
        let porcelain_stale = porcelain_only
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch))
            .unwrap();
        let listed = list_existing_worktrees(&repo, false).unwrap();
        let listed_stale = listed
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch))
            .unwrap();

        // 无论 git 版本，最终列表都把目录已删除的 linked worktree 标为 prunable。
        assert!(listed_stale.is_prunable, "porcelain output:\n{raw}");
        assert!(
            !listed[0].is_prunable,
            "main worktree must never be prunable"
        );
        if git_at_least(2, 31) {
            // git >= 2.31 自带 `prunable` 行；回退判据与之并存且一致。
            assert!(
                porcelain_stale.is_prunable,
                "git >= 2.31 porcelain should carry the prunable line:\n{raw}"
            );
        } else if !porcelain_stale.is_prunable {
            // 更早版本（或探测失败）通常没有 `prunable` 行，只能靠回退判据；发行版回移
            // 注解不算缺陷，因此不对旧版本作反向断言，只记录本次走的是回退路径。
            eprintln!("git < 2.31: prunable derived by fallback (no porcelain annotation)");
        }

        run_git(&repo, &["worktree", "prune"]);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn present_linked_checkout_is_not_prunable() {
        let repo = create_committed_repo("worktree-present-repo");
        let checkout = unique_temp_path("worktree-present-checkout");
        let branch = "worktree/present";
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        let listed = list_existing_worktrees(&repo, false).unwrap();

        assert!(listed.iter().all(|entry| !entry.is_prunable));
        assert!(listed
            .iter()
            .any(|entry| entry.branch.as_deref() == Some(branch)));

        let remove = build_worktree_remove_command(&repo, &checkout, true, false);
        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn locked_missing_checkout_is_not_prunable() {
        let repo = create_committed_repo("worktree-locked-repo");
        let checkout = unique_temp_path("worktree-locked-checkout");
        let branch = "worktree/locked";
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        run_git(&repo, &["worktree", "lock", checkout.to_str().unwrap()]);
        std::fs::remove_dir_all(&checkout).unwrap();

        let listed = list_existing_worktrees(&repo, false).unwrap();
        let locked = listed
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch))
            .unwrap();

        // git 对加锁的 worktree 从不判定 prunable（例如位于可移动介质上），回退判据同样如此。
        assert!(!locked.is_prunable);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn locked_relative_gitdir_targets_resolve_against_admin_dir() {
        // 不依赖 git 版本：手工铺设 `<common>/worktrees/<id>/{locked,gitdir}`，gitdir 用
        // git 2.48 `--relative-paths` 的相对写法，且检出目录已缺失（canonicalize 必然失败）。
        let base = unique_temp_path("worktree-relative-gitdir");
        let admin_dir = base.join("repo/.git/worktrees/wt");
        std::fs::create_dir_all(&admin_dir).unwrap();
        std::fs::write(admin_dir.join("locked"), "").unwrap();
        std::fs::write(admin_dir.join("gitdir"), "../../../checkout/.git\n").unwrap();
        let unlocked_dir = base.join("repo/.git/worktrees/other");
        std::fs::create_dir_all(&unlocked_dir).unwrap();
        std::fs::write(unlocked_dir.join("gitdir"), "../../../other/.git\n").unwrap();
        let missing_checkout = base.join("repo/checkout");
        assert!(!missing_checkout.exists());

        let targets = locked_gitdir_targets_in_admin_dir(&base.join("repo/.git/worktrees"));

        // 相对写法规范到与检出侧 `<path>/.git` 相同的路径，回退判据据此把它当作已加锁。
        assert_eq!(
            targets,
            HashSet::from([forgiving_real_path(&missing_checkout.join(".git"))])
        );

        let mut entries = vec![
            ExistingWorktree {
                path: base.join("repo"),
                branch: Some("main".into()),
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_locked: false,
            },
            ExistingWorktree {
                path: missing_checkout.clone(),
                branch: Some("locked".into()),
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_locked: false,
            },
            ExistingWorktree {
                path: base.join("repo/other"),
                branch: Some("other".into()),
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_locked: false,
            },
        ];
        crate::workspace::git_test_support::run_git(&base.join("repo"), &["init", "--quiet"]);
        mark_missing_linked_checkouts_prunable(&mut entries, &base.join("repo"), false);

        assert!(
            !entries[1].is_prunable,
            "locked worktree must not be prunable"
        );
        assert!(
            entries[2].is_prunable,
            "unlocked missing worktree is prunable"
        );

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn porcelain_locked_line_takes_precedence_over_admin_dir_scan() {
        // repo_root 不存在：若回退判据无视 porcelain 的 `locked` 行去扫 admin 目录，扫描
        // 得到空集，条目会被错误地标成 prunable。
        let base = unique_temp_path("worktree-porcelain-locked");
        let mut entries = vec![
            ExistingWorktree {
                path: base.join("repo"),
                branch: Some("main".into()),
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_locked: false,
            },
            ExistingWorktree {
                path: base.join("missing"),
                branch: Some("locked".into()),
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_locked: true,
            },
        ];

        mark_missing_linked_checkouts_prunable(&mut entries, &base.join("repo"), false);

        assert!(!entries[1].is_prunable);
    }

    #[test]
    fn locked_missing_relative_paths_checkout_is_not_prunable() {
        use crate::workspace::git_test_support::git_at_least;

        if !git_at_least(2, 48) {
            // `worktree.useRelativePaths` / `--relative-paths` 自 git 2.48 起可用；更早版本由
            // `locked_relative_gitdir_targets_resolve_against_admin_dir` 用手工布局覆盖。
            eprintln!("skipping: git < 2.48 has no relative worktree paths");
            return;
        }
        let repo = create_committed_repo("worktree-relative-locked-repo");
        let checkout = unique_temp_path("worktree-relative-locked-checkout");
        let branch = "worktree/relative-locked";
        run_git(
            &repo,
            &[
                "-c",
                "worktree.useRelativePaths=true",
                "worktree",
                "add",
                "--quiet",
                "--relative-paths",
                "-b",
                branch,
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        run_git(&repo, &["worktree", "lock", checkout.to_str().unwrap()]);
        std::fs::remove_dir_all(&checkout).unwrap();

        let listed = list_existing_worktrees(&repo, false).unwrap();
        let locked = listed
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch))
            .unwrap();

        assert!(locked.is_locked);
        assert!(!locked.is_prunable);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn forced_worktree_remove_recovers_leftover_unregistered_checkout() {
        let repo = create_committed_repo("worktree-recovery-repo");
        let checkout = unique_temp_path("worktree-recovery-checkout");
        let branch = "worktree/recovery";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD", false);
        run_worktree_command(&add).unwrap();
        let remove = build_worktree_remove_command(&repo, &checkout, true, false);
        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        let stale_admin_dir = git_common_worktrees_dir(&repo, false)
            .unwrap()
            .join("stale");
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", stale_admin_dir.display()),
        )
        .unwrap();
        std::fs::write(checkout.join("leftover"), "leftover\n").unwrap();

        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false).unwrap();

        assert!(!checkout.exists());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn forced_worktree_remove_recovery_keeps_unrelated_replacement_directory() {
        let repo = create_committed_repo("worktree-recovery-unrelated-repo");
        let checkout = unique_temp_path("worktree-recovery-unrelated-checkout");
        let branch = "worktree/recovery-unrelated";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD", false);
        run_worktree_command(&add).unwrap();
        let remove = build_worktree_remove_command(&repo, &checkout, true, false);
        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("unrelated"), "do not delete\n").unwrap();

        let err = run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false)
            .expect_err("unrelated replacement directory should not be removed");

        assert!(is_not_working_tree_remove_error(&err));
        assert!(checkout.join("unrelated").exists());
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    /// 等得到结果的用例给宽上限：条件一成立即返回，负载高时也不误判。
    const GENEROUS_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
    /// 注定等满的用例（残留删不掉）用短上限，不白等生产的 5 秒。
    const SHORT_RECOVERY_TIMEOUT: Duration = Duration::from_millis(300);

    /// 与 git 删除中途失败时的报错同形（`run_worktree_command` 固定 `LC_ALL=C`）。
    fn failed_delete_error(path: &Path, reason: &str) -> String {
        format!("error: failed to delete '{}': {reason}", path.display())
    }

    #[test]
    fn failed_delete_remove_error_matches_every_git_delete_failure() {
        for reason in [
            "Directory not empty",
            "Permission denied",
            "Device or resource busy",
        ] {
            for path in ["D:/w/herdr/issue-137", ".git/worktrees/issue-137"] {
                let message = failed_delete_error(Path::new(path), reason);
                assert!(is_failed_delete_remove_error(&message), "{message}");
            }
        }
        // 前面带 `git status` 的 warning 行时照样认得出。
        assert!(is_failed_delete_remove_error(
            "warning: could not open directory 'sub/': Permission denied\n\
             error: failed to delete 'D:/w/herdr/issue-137': Permission denied"
        ));
        // git 在删除前就拒绝的报错不算：此时什么都还没删，worktree 仍注册；路径里碰巧含这段
        // 文字也不算。
        for message in [
            "fatal: '/w/x' is not a working tree",
            "fatal: '/w/x' contains modified or untracked files, use --force to delete it",
            "fatal: '/w/x' is a main working tree",
            "fatal: cannot remove a locked working tree, lock reason: usb\n\
             use 'remove -f -f' to override or unlock first",
            "fatal: validation failed, cannot remove working tree: '/w/x/.git' does not exist",
            "fatal: failed to run 'git status' on '/w/x'",
            "fatal: '/w/error: failed to delete 'x' is not a working tree",
        ] {
            assert!(!is_failed_delete_remove_error(message), "{message}");
        }
    }

    #[test]
    fn failed_delete_recovery_removes_empty_leftovers_of_unregistered_worktree() {
        let repo = create_committed_repo("worktree-failed-delete-leftover-repo");
        // 删管理目录失败时留下的孤儿管理目录没有 `gitdir`，git 列表看不到它。
        let orphan_admin_dir = git_common_worktrees_dir(&repo, false)
            .unwrap()
            .join("orphan");
        std::fs::create_dir_all(orphan_admin_dir.join("logs")).unwrap();
        std::fs::write(orphan_admin_dir.join("logs/HEAD"), "log\n").unwrap();
        for reason in ["Directory not empty", "Permission denied"] {
            for force in [false, true] {
                let checkout = unique_temp_path("worktree-failed-delete-leftover-checkout");
                std::fs::create_dir_all(checkout.join("src/nested/deeper")).unwrap();
                std::fs::create_dir_all(checkout.join("docs")).unwrap();

                let result = recover_worktree_remove_error(
                    failed_delete_error(&checkout, reason),
                    &repo,
                    &checkout,
                    force,
                    false,
                    GENEROUS_RECOVERY_TIMEOUT,
                );

                assert_eq!(result, Ok(()), "{reason}, force={force}");
                assert!(!checkout.exists(), "{reason}, force={force}");
            }
        }
        // 只有管理目录删失败、检出早已删光：同样已注销，没有残留要收尾。
        let gone = unique_temp_path("worktree-failed-delete-leftover-gone");
        let result = recover_worktree_remove_error(
            failed_delete_error(&orphan_admin_dir, "Permission denied"),
            &repo,
            &gone,
            false,
            false,
            GENEROUS_RECOVERY_TIMEOUT,
        );
        assert_eq!(result, Ok(()));
        // 不跑 `git worktree prune`：孤儿管理目录留给 `git gc`。
        assert!(orphan_admin_dir.join("logs/HEAD").exists());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn failed_delete_recovery_waits_for_transient_entries_to_clear() {
        let repo = create_committed_repo("worktree-failed-delete-transient-repo");
        let checkout = unique_temp_path("worktree-failed-delete-transient-checkout");
        // 模拟杀毒扫描等瞬态：残留条目过一会儿自行消失，之后空目录才删得掉。
        let transient = checkout.join("src/scan.tmp");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        std::fs::write(&transient, "held\n").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::remove_file(transient).unwrap();
        });

        let result = recover_worktree_remove_error(
            failed_delete_error(&checkout, "Directory not empty"),
            &repo,
            &checkout,
            false,
            false,
            GENEROUS_RECOVERY_TIMEOUT,
        );
        release.join().unwrap();

        assert_eq!(result, Ok(()));
        assert!(!checkout.exists());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn failed_delete_recovery_keeps_leftover_files_of_unregistered_worktree() {
        let repo = create_committed_repo("worktree-failed-delete-files-repo");
        let checkout = unique_temp_path("worktree-failed-delete-files-checkout");
        std::fs::create_dir_all(checkout.join("src/empty")).unwrap();
        std::fs::create_dir_all(checkout.join("target/debug")).unwrap();
        let kept = checkout.join("src/written-during-removal.txt");
        std::fs::write(&kept, "keep me\n").unwrap();

        let result = recover_worktree_remove_error(
            failed_delete_error(&checkout, "Permission denied"),
            &repo,
            &checkout,
            false,
            false,
            SHORT_RECOVERY_TIMEOUT,
        );

        // git 视角已注销：照样成功（残留只告警），文件原样保留，空目录清掉。
        assert_eq!(result, Ok(()));
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "keep me\n");
        assert!(!checkout.join("src/empty").exists());
        assert!(!checkout.join("target").exists());
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn failed_delete_recovery_returns_git_error_while_worktree_is_registered() {
        let repo = create_committed_repo("worktree-failed-delete-registered-repo");
        let checkout = unique_temp_path("worktree-failed-delete-registered-checkout");
        let add = build_worktree_add_new_branch_command(
            &repo,
            &checkout,
            "worktree/failed-delete-registered",
            "HEAD",
            false,
        );
        run_worktree_command(&add).unwrap();
        std::fs::create_dir_all(checkout.join("empty")).unwrap();
        let error = failed_delete_error(&checkout, "Permission denied");

        let started = Instant::now();
        let result = recover_worktree_remove_error(
            error.clone(),
            &repo,
            &checkout,
            false,
            false,
            GENEROUS_RECOVERY_TIMEOUT,
        );

        assert_eq!(result, Err(error));
        // 注册状态在 git 退出后不会再变：查一次就返回，不等时限。
        let elapsed = started.elapsed();
        assert!(elapsed < GENEROUS_RECOVERY_TIMEOUT / 2, "{elapsed:?}");
        // 仍注册时不碰检出，连空目录也保留。
        assert!(checkout.join("empty").is_dir());
        assert!(worktree_list_contains_path(&repo, &checkout, false).unwrap());

        let remove = build_worktree_remove_command(&repo, &checkout, true, false);
        run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, true, false).unwrap();
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn failed_delete_recovery_keeps_git_error_when_registration_is_unknown() {
        let base = unique_temp_path("worktree-failed-delete-unknown");
        let checkout = base.join("checkout");
        std::fs::create_dir_all(checkout.join("empty")).unwrap();
        let error = failed_delete_error(&checkout, "Directory not empty");

        // 仓库已不在：`git worktree list` 失败，无法确认注销就不能当成已删除。
        let result = recover_worktree_remove_error(
            error.clone(),
            &base.join("missing-repo"),
            &checkout,
            false,
            false,
            GENEROUS_RECOVERY_TIMEOUT,
        );

        let message = result.unwrap_err();
        assert!(message.starts_with(&error), "{message}");
        assert!(checkout.join("empty").is_dir());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn unrecognized_worktree_remove_errors_are_returned_unchanged() {
        let repo = create_committed_repo("worktree-remove-error-passthrough-repo");
        let checkout = unique_temp_path("worktree-remove-error-passthrough-checkout");
        std::fs::create_dir_all(checkout.join("empty")).unwrap();
        let errors = [
            (
                false,
                format!(
                    "fatal: '{}' contains modified or untracked files, use --force to delete it",
                    checkout.display()
                ),
            ),
            (
                false,
                format!("fatal: '{}' is not a working tree", checkout.display()),
            ),
            (
                true,
                "fatal: cannot remove a locked working tree;\n\
                 use 'remove -f -f' to override or unlock first"
                    .to_string(),
            ),
        ];
        for (force, error) in errors {
            let result = recover_worktree_remove_error(
                error.clone(),
                &repo,
                &checkout,
                force,
                false,
                GENEROUS_RECOVERY_TIMEOUT,
            );
            assert_eq!(result, Err(error));
        }
        // 真实 git 报错同样原样返回：非 force 删除未注册的路径。
        let remove = build_worktree_remove_command(&repo, &checkout, false, false);
        let error =
            run_worktree_remove_command_with_recovery(&remove, &repo, &checkout, false, false)
                .unwrap_err();
        assert!(is_not_working_tree_remove_error(&error), "{error}");
        // 只有 git 删除中途失败才会清理空目录。
        assert!(checkout.join("empty").is_dir());
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn leftover_cleanup_sweeps_once_while_a_kept_entry_remains() {
        let checkout = unique_temp_path("worktree-leftover-sweep-once");
        std::fs::create_dir_all(checkout.join("src/empty")).unwrap();
        std::fs::create_dir_all(checkout.join("target/debug")).unwrap();
        let kept = checkout.join("src/keep.txt");
        std::fs::write(&kept, "keep me\n").unwrap();
        let timeout = Duration::from_secs(1);

        let started = Instant::now();
        let cleanup = clean_up_unregistered_leftovers(&checkout, timeout);

        // 文件在，检出目录就删不掉：第一轮清掉空目录后只盯着这个文件、不再重扫整棵树，但仍
        // 等到时限，好在它消失时接着收尾。
        assert!(!cleanup.removed);
        assert_eq!(cleanup.sweeps, 1);
        assert_eq!(cleanup.kept.as_deref(), Some(kept.as_path()));
        assert!(started.elapsed() >= timeout);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "keep me\n");
        assert!(!checkout.join("src/empty").exists());
        assert!(!checkout.join("target").exists());
        let _ = std::fs::remove_dir_all(checkout);
    }

    #[test]
    fn leftover_cleanup_resweeps_once_the_kept_entry_is_gone() {
        let checkout = unique_temp_path("worktree-leftover-resweep");
        std::fs::create_dir_all(checkout.join("src/nested")).unwrap();
        let transient = checkout.join("src/scan.tmp");
        std::fs::write(&transient, "held\n").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::remove_file(transient).unwrap();
        });

        let cleanup = clean_up_unregistered_leftovers(&checkout, GENEROUS_RECOVERY_TIMEOUT);
        release.join().unwrap();

        // 挡路的文件消失后再扫一轮即可，不是每次退避醒来都扫。
        assert!(cleanup.removed);
        assert!(cleanup.sweeps <= 2, "{}", cleanup.sweeps);
        assert!(!checkout.exists());
    }

    #[test]
    fn leftover_sweep_stops_at_the_deadline() {
        let checkout = unique_temp_path("worktree-leftover-deadline");
        std::fs::create_dir_all(checkout.join("a/b/c")).unwrap();
        std::fs::create_dir_all(checkout.join("d")).unwrap();
        let mut kept = None;

        // 时限已过：一个条目都不再处理，大残留树不会让调用拖过时限。
        assert!(!remove_empty_dirs_bottom_up(
            &checkout,
            Instant::now(),
            &mut kept
        ));

        assert!(checkout.join("a/b/c").is_dir());
        assert!(checkout.join("d").is_dir());
        assert_eq!(kept, None);
        let _ = std::fs::remove_dir_all(checkout);
    }

    /// 在 `link` 建一个指向目录 `target` 的链接（Windows 用无需特权的 junction）。
    #[cfg(unix)]
    fn link_dir(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn link_dir(target: &Path, link: &Path) {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn leftover_sweep_keeps_links_without_following_them() {
        let base = unique_temp_path("worktree-leftover-link");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("empty")).unwrap();
        let checkout = base.join("checkout");
        std::fs::create_dir_all(checkout.join("empty")).unwrap();
        let link = checkout.join("link");
        link_dir(&outside, &link);
        let mut kept = None;

        let deadline = Instant::now() + GENEROUS_RECOVERY_TIMEOUT;
        assert!(!remove_empty_dirs_bottom_up(&checkout, deadline, &mut kept));

        // 链接本身留着，也不顺着它删目标里的空目录；旁边的空目录照删。
        assert_eq!(kept.as_deref(), Some(link.as_path()));
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(outside.join("empty").is_dir());
        assert!(!checkout.join("empty").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    /// 打开目录且不共享删除，同 Windows 给进程当前目录开的句柄：释放前谁也删不掉它，用来
    /// 模拟尚在退出、cwd 仍在检出里的 pane 进程。
    #[cfg(windows)]
    fn hold_dir_like_process_cwd(dir: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .unwrap()
    }

    /// 真实 git：检出根被占用时，git 删光里面的内容、删根失败，但照样删掉管理目录。占用在
    /// 时限内释放就收尾成功；一直不释放也按已注销成功，空的根留着。
    #[cfg(windows)]
    #[test]
    fn failed_delete_recovery_handles_checkout_held_by_exiting_process() {
        let repo = create_committed_repo("worktree-held-checkout-repo");
        for released_in_time in [true, false] {
            let checkout = unique_temp_path("worktree-held-checkout");
            let add = build_worktree_add_new_branch_command(
                &repo,
                &checkout,
                &format!("worktree/held-checkout-{released_in_time}"),
                "HEAD",
                false,
            );
            run_worktree_command(&add).unwrap();
            let held = hold_dir_like_process_cwd(&checkout);
            let remove = build_worktree_remove_command(&repo, &checkout, false, false);

            let error = run_worktree_command(&remove).unwrap_err();
            eprintln!("git worktree remove with the checkout held: {error}");
            assert!(is_failed_delete_remove_error(&error), "{error}");

            if released_in_time {
                let release = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    drop(held);
                });
                let result = recover_worktree_remove_error(
                    error,
                    &repo,
                    &checkout,
                    false,
                    false,
                    GENEROUS_RECOVERY_TIMEOUT,
                );
                release.join().unwrap();
                assert_eq!(result, Ok(()));
                assert!(!checkout.exists());
            } else {
                let started = Instant::now();
                let result = recover_worktree_remove_error(
                    error,
                    &repo,
                    &checkout,
                    false,
                    false,
                    SHORT_RECOVERY_TIMEOUT,
                );
                assert!(started.elapsed() >= SHORT_RECOVERY_TIMEOUT);
                assert_eq!(result, Ok(()));
                assert!(std::fs::read_dir(&checkout).unwrap().next().is_none());
                drop(held);
                let _ = std::fs::remove_dir(&checkout);
            }
            assert!(!worktree_list_contains_path(&repo, &checkout, false).unwrap());
        }
        let _ = std::fs::remove_dir_all(repo);
    }

    /// 真实 git：被占用的是检出里的子目录时，git 在第一处失败就停手，排在它后面的检出内容
    /// 没删到。恢复只删空目录，这些文件原样保留，到时按已注销成功（告警）。
    #[cfg(windows)]
    #[test]
    fn failed_delete_recovery_keeps_files_git_did_not_reach() {
        let repo = create_committed_repo("worktree-held-subdir-repo");
        let checkout = unique_temp_path("worktree-held-subdir-checkout");
        let add = build_worktree_add_new_branch_command(
            &repo,
            &checkout,
            "worktree/held-subdir",
            "HEAD",
            false,
        );
        run_worktree_command(&add).unwrap();
        // NTFS 按名字顺序枚举：`aaa` 排在 `README.md` 之前，git 删到它就停。
        let held_dir = checkout.join("aaa");
        std::fs::create_dir(&held_dir).unwrap();
        let held = hold_dir_like_process_cwd(&held_dir);
        let remove = build_worktree_remove_command(&repo, &checkout, false, false);

        let error = run_worktree_command(&remove).unwrap_err();
        drop(held);
        eprintln!("git worktree remove with a subdirectory held: {error}");
        assert!(is_failed_delete_remove_error(&error), "{error}");
        let readme = checkout.join("README.md");
        assert!(readme.is_file(), "git should stop before README.md");

        let result = recover_worktree_remove_error(
            error,
            &repo,
            &checkout,
            false,
            false,
            SHORT_RECOVERY_TIMEOUT,
        );

        assert_eq!(result, Ok(()));
        assert!(!held_dir.exists());
        assert!(readme.is_file());
        assert!(!worktree_list_contains_path(&repo, &checkout, false).unwrap());
        let _ = std::fs::remove_dir_all(checkout);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[cfg(windows)]
    #[test]
    fn leftover_cleanup_backs_off_while_an_empty_dir_is_held() {
        let checkout = unique_temp_path("worktree-leftover-held");
        std::fs::create_dir_all(&checkout).unwrap();
        let held = hold_dir_like_process_cwd(&checkout);

        let cleanup = clean_up_unregistered_leftovers(&checkout, Duration::from_secs(2));

        // 没有永不删除的条目、只是目录被占着：按退避重试（50ms 起翻倍、封顶 1s），2 秒内
        // 至多七轮，而不是每 100ms 一轮。
        assert!(!cleanup.removed);
        assert_eq!(cleanup.kept, None);
        assert!((2..=7).contains(&cleanup.sweeps), "{}", cleanup.sweeps);
        drop(held);
        assert!(clean_up_unregistered_leftovers(&checkout, GENEROUS_RECOVERY_TIMEOUT).removed);
    }
}
