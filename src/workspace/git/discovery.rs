use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpaceMetadata {
    pub key: String,
    pub checkout_key: String,
    pub repo_name: String,
    pub repo_root: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeInfo {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub git_common_dir: PathBuf,
    pub is_bare: bool,
    pub is_linked_worktree: bool,
}

pub fn derive_label_from_cwd(cwd: &Path) -> String {
    git_repo_root(cwd)
        .map(|repo_root| automatic_workspace_label(cwd, &repo_root))
        .unwrap_or_else(|| fallback_label_from_cwd(cwd))
}

pub fn fallback_label_from_cwd(cwd: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        if cwd == home {
            return "~".to_string();
        }
    }

    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cwd.display().to_string())
}

pub fn git_worktree_info(cwd: &Path) -> Option<GitWorktreeInfo> {
    let repo_root = git_repo_root(cwd)?;
    let git_dir = canonicalize_best_effort_path(&git_dir_for_repo_root(&repo_root)?);
    let git_common_dir = canonicalize_best_effort_path(&git_common_dir_for_git_dir(&git_dir));
    let is_linked_worktree = git_dir != git_common_dir;
    let is_bare = git_dir_is_bare(&git_dir);

    Some(GitWorktreeInfo {
        repo_root,
        git_dir,
        git_common_dir,
        is_bare,
        is_linked_worktree,
    })
}

pub fn git_space_metadata(cwd: &Path) -> Option<GitSpaceMetadata> {
    let info = git_worktree_info(cwd)?;
    Some(git_space_metadata_from_info(&info))
}

pub(crate) fn automatic_workspace_label(cwd: &Path, repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_label_from_cwd(cwd))
}

pub(super) fn git_space_metadata_from_info(info: &GitWorktreeInfo) -> GitSpaceMetadata {
    let key = canonicalize_best_effort_path(&info.git_common_dir)
        .display()
        .to_string();
    let checkout_key = canonicalize_best_effort_path(&info.repo_root)
        .display()
        .to_string();
    let common_dir_name = info
        .git_common_dir
        .file_name()
        .and_then(|name| name.to_str());
    let label_path = match common_dir_name {
        Some(".git") => info.git_common_dir.parent().unwrap_or(&info.repo_root),
        Some(".bare") => embedded_bare_repo_container(info).unwrap_or(&info.git_common_dir),
        _ => &info.git_common_dir,
    };
    let repo_name = label_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();
    GitSpaceMetadata {
        key,
        checkout_key,
        repo_name,
        repo_root: info.repo_root.clone(),
        is_linked_worktree: info.is_linked_worktree,
    }
}

fn embedded_bare_repo_container(info: &GitWorktreeInfo) -> Option<&Path> {
    let parent = info.git_common_dir.parent()?;
    let parent_git_dir = git_dir_for_repo_root(parent)?;
    (canonicalize_best_effort_path(&parent_git_dir) == info.git_common_dir).then_some(parent)
}

pub(super) fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn git_common_dir_for_git_dir(git_dir: &Path) -> PathBuf {
    let commondir = git_dir.join("commondir");
    let Ok(contents) = std::fs::read_to_string(commondir) else {
        return git_dir.to_path_buf();
    };
    let path = Path::new(contents.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    }
}

/// Outcome of reading one Git ref file, classified without collapsing metadata
/// errors so callers can distinguish "this ref does not exist" from "this ref
/// exists (or cannot be ruled out) but its content must not be trusted".
/// `Path::exists()` cannot distinguish them because it returns `false` on
/// metadata errors. When opening reports `NotFound` or `NotADirectory`,
/// `symlink_metadata` distinguishes a genuinely absent path from a dangling
/// symlink without following the final link.
pub(super) enum RefFileRead {
    Content(String),
    Absent,
    Unavailable,
}

pub(super) fn read_git_ref_file_state(path: &Path) -> RefFileRead {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return match std::fs::symlink_metadata(path) {
                // The directory entry exists but its symlink target is missing
                // or traverses a non-directory. Git treats the loose ref as
                // broken and does not fall back to an older packed ref.
                Ok(_) => RefFileRead::Unavailable,
                Err(metadata_error)
                    if metadata_error.kind() == std::io::ErrorKind::NotFound
                        || metadata_error.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    RefFileRead::Absent
                }
                Err(_) => RefFileRead::Unavailable,
            };
        }
        // Permission or I/O errors: the ref may exist, so its identity is
        // unavailable rather than absent.
        Err(_) => return RefFileRead::Unavailable,
    };
    let mut contents = String::new();
    if file
        .take((MAX_GIT_REF_FILE_BYTES + 1) as u64)
        .read_to_string(&mut contents)
        .is_err()
        || contents.len() > MAX_GIT_REF_FILE_BYTES
    {
        return RefFileRead::Unavailable;
    }
    RefFileRead::Content(contents)
}

pub(super) fn read_git_ref_file(path: &Path) -> Option<String> {
    match read_git_ref_file_state(path) {
        RefFileRead::Content(contents) => Some(contents),
        RefFileRead::Absent | RefFileRead::Unavailable => None,
    }
}

pub fn git_branch(cwd: &Path) -> Option<String> {
    let repo_root = git_repo_root(cwd)?;
    let git_dir = git_dir_for_repo_root(&repo_root)?;
    let git_common_dir = git_common_dir_for_git_dir(&git_dir);
    if git_ref_storage_is_reftable(&git_common_dir) {
        return git_symbolic_head_short(&repo_root);
    }

    let head = read_git_ref_file(&git_dir.join("HEAD"))?;
    parse_git_head_branch(&head)
}

pub(super) fn git_dir_for_repo_root(repo_root: &Path) -> Option<PathBuf> {
    let git_path = repo_root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }

    if let Ok(gitdir) = std::fs::read_to_string(&git_path) {
        if let Some(relative) = gitdir.trim().strip_prefix("gitdir:").map(str::trim) {
            let resolved = Path::new(relative);
            return Some(if resolved.is_absolute() {
                resolved.to_path_buf()
            } else {
                repo_root.join(resolved)
            });
        }
    }

    if path_is_git_dir_layout(repo_root) && git_dir_is_bare(repo_root) {
        return Some(repo_root.to_path_buf());
    }

    None
}

fn path_is_git_dir_layout(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

pub(super) fn git_symbolic_head_full(repo_root: &Path) -> Option<String> {
    git_trimmed_stdout(repo_root, &["symbolic-ref", "--quiet", "HEAD"])
}

fn git_symbolic_head_short(repo_root: &Path) -> Option<String> {
    git_trimmed_stdout(repo_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

pub(super) fn git_rev_parse_verify(repo_root: &Path, revision: &str) -> Option<String> {
    git_trimmed_stdout(repo_root, &["rev-parse", "--verify", revision])
}

pub(super) fn git_ref_storage_is_reftable(git_common_dir: &Path) -> bool {
    read_git_config_value(&git_common_dir.join("config"), "extensions", "refstorage")
        .is_some_and(|value| value.eq_ignore_ascii_case("reftable"))
}

fn git_dir_is_bare(git_dir: &Path) -> bool {
    read_git_config_value(&git_dir.join("config"), "core", "bare")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn parse_git_head_branch(head: &str) -> Option<String> {
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

fn read_git_config_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section_name) = simple_git_config_section(line) {
            in_section = section_name.eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(key) {
            return Some(strip_git_config_comment(value).trim().to_string());
        }
    }
    None
}

fn simple_git_config_section(line: &str) -> Option<&str> {
    let section = line.strip_prefix('[')?.split_once(']')?.0.trim();
    (!section.contains('"')).then_some(section)
}

fn strip_git_config_comment(value: &str) -> &str {
    let value = value.trim();
    for marker in ['#', ';'] {
        if let Some((prefix, _)) = value.split_once(marker) {
            if prefix.chars().next_back().is_some_and(char::is_whitespace) {
                return prefix;
            }
        }
    }
    value
}

fn git_trimmed_stdout(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim();
    (!stdout.is_empty()).then(|| stdout.to_string())
}

/// 向上查找仓库根，遵守 git 的 `GIT_CEILING_DIRECTORIES` 语义：上界目录本身及其祖先不再
/// 被视为仓库根，与 git 子进程对同一目录的判断保持一致。
pub(super) fn git_repo_root(start: &Path) -> Option<PathBuf> {
    git_repo_root_below_ceilings(start, discovery_ceilings())
}

const GIT_CEILING_DIRECTORIES_ENV: &str = "GIT_CEILING_DIRECTORIES";

/// 进程内只解析一次：环境变量在进程生命周期内视为只读，状态路径上每个 workspace 每次
/// 刷新都会发现多次，不重复切分与 canonicalize。
///
/// 测试构建无条件把临时根追加为上界，让临时目录之上的 `.git`（例如宿主机 `/.git`）不会被
/// 误认为测试目录的仓库；这与调用顺序、是否用过某个测试辅助无关，`cargo test` 同进程多线程
/// 与 nextest 每测试独立进程下行为一致，且不改环境变量（无 `set_var` 竞态）。
fn discovery_ceilings() -> &'static [PathBuf] {
    static CEILINGS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CEILINGS.get_or_init(|| {
        let mut ceilings =
            parse_git_ceiling_directories(std::env::var_os(GIT_CEILING_DIRECTORIES_ENV).as_deref());
        if cfg!(test) {
            ceilings.push(canonicalize_best_effort_path(&std::env::temp_dir()));
        }
        ceilings
    })
}

/// 解析 `GIT_CEILING_DIRECTORIES`（同 git `canonicalize_ceiling_entry`）：按平台路径分隔符
/// 切分；相对路径条目忽略；条目默认解析为真实路径（解析失败则丢弃），列表中出现空条目后，
/// 其后的条目按字面值保留。
pub(super) fn parse_git_ceiling_directories(value: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut resolve_real_path = true;
    let mut ceilings = Vec::new();
    for entry in std::env::split_paths(value) {
        if entry.as_os_str().is_empty() {
            resolve_real_path = false;
            continue;
        }
        if !entry.is_absolute() {
            continue;
        }
        if resolve_real_path {
            ceilings.extend(real_path_allowing_missing_leaf(&entry));
        } else {
            ceilings.push(entry);
        }
    }
    ceilings
}

/// git 的 `real_path` 允许最后一级不存在；其余错误视为无效条目。
fn real_path_allowing_missing_leaf(path: &Path) -> Option<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(real) => Some(real),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = std::fs::canonicalize(path.parent()?).ok()?;
            Some(parent.join(path.file_name()?))
        }
        Err(_) => None,
    }
}

fn is_strict_ancestor(ancestor: &Path, path: &Path) -> bool {
    path.strip_prefix(ancestor)
        .is_ok_and(|rest| !rest.as_os_str().is_empty())
}

fn git_repo_root_below_ceilings(start: &Path, ceilings: &[PathBuf]) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    // 上界只在起点（真实路径）严格位于其下时生效，多条命中取最深者（同 git
    // `longest_ancestor_length`）。比较沿真实路径链进行——git 在已解析符号链接的 cwd 上按
    // 偏移比较——一旦当前目录的真实路径不再严格位于上界之下就停止，符号链接把目录挂到上界
    // 子树之外时也不会越过上界继续上溯。返回值仍沿用调用方给出的路径形态，避免符号链接
    // 检出的标签随之改变。
    let mut canonical_current =
        (!ceilings.is_empty()).then(|| canonicalize_best_effort_path(&current));
    let ceiling = canonical_current.as_deref().and_then(|canonical_start| {
        ceilings
            .iter()
            .filter(|ceiling| is_strict_ancestor(ceiling, canonical_start))
            .max_by_key(|ceiling| ceiling.components().count())
    });

    loop {
        if let (Some(ceiling), Some(canonical)) = (ceiling, canonical_current.as_deref()) {
            if !is_strict_ancestor(ceiling, canonical) {
                return None;
            }
        }
        if git_dir_for_repo_root(&current)
            .map(|git_dir| git_dir.join("HEAD").is_file())
            .unwrap_or(false)
        {
            return Some(current);
        }
        let parent = current.parent()?.to_path_buf();
        if ceiling.is_some() {
            canonical_current = Some(canonical_parent(
                &current,
                &parent,
                canonical_current.as_deref(),
            ));
        }
        current = parent;
    }
}

/// 真实路径链随原路径链同步上溯：当前目录末级是普通组件且自身不是符号链接时，其真实路径的
/// 父目录就是父目录的真实路径，省去每级一次 realpath；否则（符号链接、`..`/`.` 末级或路径
/// 不存在）两条链不再对齐，重新解析父目录。
fn canonical_parent(current: &Path, parent: &Path, canonical_current: Option<&Path>) -> PathBuf {
    let aligned = matches!(current.components().next_back(), Some(Component::Normal(_)))
        && std::fs::symlink_metadata(current).is_ok_and(|meta| !meta.file_type().is_symlink());
    match canonical_current.and_then(Path::parent) {
        Some(canonical) if aligned => canonical.to_path_buf(),
        _ => canonicalize_best_effort_path(parent),
    }
}

pub(super) fn read_ref_oid(common_dir: &Path, full_ref: &str) -> Option<String> {
    let loose_ref = common_dir.join(full_ref);
    match read_git_ref_file_state(&loose_ref) {
        RefFileRead::Content(contents) => {
            let oid = contents.trim();
            if oid.is_empty() {
                // An empty loose ref is present but broken. Git does not fall
                // back to an older same-name packed ref in this case.
                return None;
            }
            return Some(oid.to_string());
        }
        // A loose ref that exists — or whose existence cannot be ruled out
        // because of a metadata or I/O error — must not fall back to
        // packed-refs: that could resurrect a stale same-name OID into the
        // status fingerprint. Report the ref as unavailable instead.
        RefFileRead::Unavailable => return None,
        RefFileRead::Absent => {}
    }

    let packed_refs = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    for line in packed_refs.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let oid = parts.next()?;
        let name = parts.next()?;
        if name == full_ref {
            return Some(oid.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::workspace::git::test_support::{run_git, temp_test_dir};

    #[test]
    fn git_branch_reads_head_from_standard_repo() {
        let root = temp_test_dir("standard-repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_loose_ref_is_unavailable_not_absent() {
        let root = temp_test_dir("oversized-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        let loose = refs_dir.join("main");
        std::fs::write(&loose, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(loose)
            .unwrap()
            .set_len(8 * 1024 * 1024)
            .unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid, None,
            "an oversized loose ref must make the ref unavailable, not fall back to the stale packed OID"
        );
    }

    #[test]
    fn empty_or_whitespace_loose_ref_is_unavailable_not_absent() {
        let root = temp_test_dir("empty-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        let loose = refs_dir.join("main");

        for contents in ["", " \n\t"] {
            std::fs::write(&loose, contents).unwrap();
            assert_eq!(
                read_ref_oid(&root, "refs/heads/main"),
                None,
                "an empty or whitespace-only loose ref must not fall back to the stale packed OID"
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_loose_ref_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("dangling-symlink-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        symlink("missing-target", refs_dir.join("main")).unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid, None,
            "a dangling loose ref must not fall back to the stale packed OID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_through_file_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("dangling-symlink-through-file");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        std::fs::write(refs_dir.join("target-parent"), "not a directory").unwrap();
        symlink("target-parent/nested", refs_dir.join("main")).unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid, None,
            "a dangling loose ref whose target traverses a file must not fall back to the stale packed OID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_loose_ref_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("symlink-loop-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();
        // Unlike chmod(000), a symlink loop produces an open error even as root.
        // It exercises the same unavailable-ref branch as permission errors.
        let loose_ref = refs_dir.join("main");
        symlink("main", &loose_ref).unwrap();
        let open_error = std::fs::File::open(&loose_ref).unwrap_err();
        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(open_error.raw_os_error(), Some(libc::ELOOP));
        assert_eq!(
            oid, None,
            "a loose ref behind an open error must be unavailable, not fall back to the stale packed OID"
        );
    }

    #[test]
    fn ref_path_through_a_file_still_reads_packed_refs() {
        let root = temp_test_dir("ref-path-through-file");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).unwrap();
        // refs/heads/main is a file, so refs/heads/main/nested cannot exist as
        // a loose ref; the packed entry is the legitimate source.
        std::fs::write(
            refs_dir.join("main"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        )
        .unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main/nested\n",
        )
        .unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main/nested");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn absent_loose_ref_still_reads_packed_refs() {
        let root = temp_test_dir("packed-only-ref");
        std::fs::create_dir_all(root.join("refs/heads")).unwrap();
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .unwrap();

        let oid = read_ref_oid(&root, "refs/heads/main");
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn oversized_git_head_is_rejected() {
        let root = temp_test_dir("oversized-head");
        let git_dir = root.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let head = git_dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(head)
            .unwrap()
            .set_len(60 * 1024 * 1024)
            .unwrap();

        let branch = git_branch(&root);
        let branch_len = branch.as_ref().map(String::len);
        std::fs::remove_dir_all(root).unwrap();

        assert!(
            branch.is_none(),
            "oversized Git HEAD produced branch with {branch_len:?} bytes"
        );
    }

    #[test]
    fn git_branch_reads_head_from_worktree_gitdir_file() {
        let root = temp_test_dir("worktree");
        let worktree_git_dir = root.join(".bare/worktrees/feature");
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::write(root.join(".git"), "gitdir: .bare/worktrees/feature\n").unwrap();
        std::fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();

        assert_eq!(git_branch(&root).as_deref(), Some("feature"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_returns_none_for_detached_head() {
        let root = temp_test_dir("detached-head");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "3e1b9a8d\n").unwrap();

        assert_eq!(git_branch(&root), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_reads_symbolic_head_from_reftable_repo() {
        let root = temp_test_dir("reftable-branch");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .unwrap();
        if !output.status.success() {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }

        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ceiling_directories_parse_like_git() {
        let base = temp_test_dir("ceiling-parse");
        let resolved = base.join("resolved");
        std::fs::create_dir_all(&resolved).unwrap();
        let verbatim = base.join("./verbatim");
        let value = std::env::join_paths([
            resolved.as_os_str(),
            std::ffi::OsStr::new("relative/entry"),
            std::ffi::OsStr::new(""),
            verbatim.as_os_str(),
        ])
        .unwrap();

        let ceilings = parse_git_ceiling_directories(Some(value.as_os_str()));

        // 相对条目被忽略；空条目之前的条目解析为真实路径，之后的按字面值保留。
        assert_eq!(
            ceilings,
            vec![std::fs::canonicalize(&resolved).unwrap(), verbatim]
        );
        assert_eq!(parse_git_ceiling_directories(None), Vec::<PathBuf>::new());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_repo_root_stops_below_ceiling_directory() {
        let base = temp_test_dir("ceiling-stop");
        let repo = base.join("repo");
        let nested = repo.join("src/deep");
        crate::workspace::git::test_support::write_fake_tracked_repo(&repo);
        std::fs::create_dir_all(&nested).unwrap();
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();

        assert_eq!(
            git_repo_root_below_ceilings(&nested, &[]),
            Some(repo.clone())
        );
        // 上界目录本身不再被视为仓库根。
        assert_eq!(
            git_repo_root_below_ceilings(&nested, &[canonical(&repo)]),
            None
        );
        // 上界位于仓库根之下时，查找在到达上界前就停止。
        assert_eq!(
            git_repo_root_below_ceilings(&nested, &[canonical(&repo.join("src"))]),
            None
        );
        // 起点必须严格位于上界之下，上界才生效（同 git `longest_ancestor_length`）。
        assert_eq!(
            git_repo_root_below_ceilings(&nested, &[canonical(&nested)]),
            Some(repo.clone())
        );
        // 与起点无祖先关系的上界不影响查找。
        assert_eq!(
            git_repo_root_below_ceilings(&nested, &[base.join("elsewhere")]),
            Some(repo.clone())
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_branch_reads_default_branch_from_real_repo_under_ceiling() {
        let root = temp_test_dir("real-repo-default-branch");
        crate::workspace::git::test_support::init_repo_on_branch(&root, "main");

        assert_eq!(git_repo_root(&root), Some(root.clone()));
        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    // 符号链接仅在 unix 上无需特权即可创建；Windows 的等价形态由真实路径链逻辑本身覆盖。
    #[cfg(unix)]
    #[test]
    fn git_repo_root_ceiling_holds_when_start_enters_it_through_symlink() {
        let base = temp_test_dir("ceiling-symlink");
        // 符号链接放在与上界无祖先关系的子树 `elsewhere` 里，且 `elsewhere` 自身是仓库：
        // 上溯若按原路径链逐级比较，离开上界子树后会误命中它（git 在真实路径上会停在上界）。
        let real = base.join("real");
        let target = real.join("x");
        std::fs::create_dir_all(target.join("repo/src")).unwrap();
        let elsewhere = base.join("elsewhere");
        crate::workspace::git::test_support::write_fake_tracked_repo(&elsewhere);
        let link = elsewhere.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let start = link.join("repo/src");
        let ceiling = std::fs::canonicalize(&real).unwrap();

        // 无上界：沿原路径上溯到 `elsewhere`，返回值保持调用方路径形态。
        assert_eq!(
            git_repo_root_below_ceilings(&start, &[]),
            Some(elsewhere.clone())
        );
        // 上界 `real`：起点真实路径位于其下；上溯经符号链接离开上界子树时停止，不会扫到
        // `elsewhere`。
        assert_eq!(
            git_repo_root_below_ceilings(&start, std::slice::from_ref(&ceiling)),
            None
        );
        // 上界之下经符号链接可达的仓库仍能发现，且返回符号链接形态的路径。
        crate::workspace::git::test_support::write_fake_tracked_repo(&target.join("repo"));
        assert_eq!(
            git_repo_root_below_ceilings(&start, std::slice::from_ref(&ceiling)),
            Some(link.join("repo"))
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_repo_root_ceiling_holds_across_dot_dot_components() {
        let base = temp_test_dir("ceiling-dot-dot");
        let repo = base.join("repo");
        crate::workspace::git::test_support::write_fake_tracked_repo(&repo);
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        let start = repo.join("src/../src/deep");
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();

        assert_eq!(
            git_repo_root_below_ceilings(&start, &[]).map(|root| canonical(&root)),
            Some(canonical(&repo))
        );
        assert_eq!(
            git_repo_root_below_ceilings(&start, &[canonical(&repo)]),
            None
        );
        assert_eq!(
            git_repo_root_below_ceilings(&start, &[canonical(&repo.join("src"))]),
            None
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    /// 环境变量接线（`GIT_CEILING_DIRECTORIES` → `discovery_ceilings` → `git_repo_root`）只能
    /// 在启动时就带着该变量的进程里验证：本测试在子进程里重跑自身（同
    /// `platform::remote_bridge_tests` 的做法），父进程不改环境变量。
    #[test]
    fn git_repo_root_honors_ceiling_directories_from_environment() {
        const CHILD_REPO_ENV: &str = "HERDR_TEST_CEILING_CHILD_REPO";
        // 子进程用标记证明断言确实执行过：`--exact` 名字不匹配时子进程会跑 0 个测试仍退出 0。
        const CHILD_MARKER: &str = "herdr-ceiling-env-child-asserted";
        if let Some(repo) = std::env::var_os(CHILD_REPO_ENV) {
            let repo = PathBuf::from(repo);
            // 子进程：上界 `repo/src` 来自真实环境变量；其下的起点发现不到仓库，仓库根自身
            // 不在上界之下，仍可发现。
            assert_eq!(git_repo_root(&repo.join("src/deep")), None);
            assert_eq!(git_repo_root(&repo), Some(repo.clone()));
            println!("{CHILD_MARKER}");
            return;
        }
        let base = temp_test_dir("ceiling-env");
        let repo = base.join("repo");
        crate::workspace::git::test_support::write_fake_tracked_repo(&repo);
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        // 父进程未设置该变量：同一起点能发现仓库。
        assert_eq!(git_repo_root(&repo.join("src/deep")), Some(repo.clone()));

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace::git::discovery::tests::git_repo_root_honors_ceiling_directories_from_environment",
                "--nocapture",
            ])
            .env(CHILD_REPO_ENV, &repo)
            .env(GIT_CEILING_DIRECTORIES_ENV, repo.join("src"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(CHILD_MARKER),
            "child process failed:\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_repo_root_ignores_invalid_git_marker() {
        let base = temp_test_dir("invalid-git-root");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(git_repo_root(&cwd), None);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_repo_root_ignores_standalone_non_bare_git_dir_layout() {
        let root = temp_test_dir("standalone-non-bare-git-dir");
        std::fs::write(root.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("objects")).unwrap();
        std::fs::create_dir_all(root.join("refs")).unwrap();
        std::fs::write(root.join("config"), "[core]\n\tbare = false\n").unwrap();

        assert_eq!(git_repo_root(&root.join("refs")), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_space_metadata_supports_standalone_bare_repo() {
        let bare = temp_test_dir("bare-space");
        run_git(&bare, &["init", "--bare", "."]);
        let nested = bare.join("refs");

        let info = git_worktree_info(&nested).expect("bare repo should be discovered");
        assert!(info.is_bare);
        assert!(!info.is_linked_worktree);
        assert_eq!(info.git_dir, canonicalize_best_effort_path(&bare));

        let metadata = git_space_metadata(&nested).expect("bare repo should map to a git space");
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&bare)
        );
        assert!(!metadata.is_linked_worktree);

        std::fs::remove_dir_all(bare).unwrap();
    }

    #[test]
    fn bare_source_and_linked_checkout_share_repo_name_but_not_auto_label() {
        let (base, bare, checkout) =
            crate::workspace::git::test_support::create_bare_repo_with_linked_worktree(
                "bare-linked-labels",
            );

        let bare_space = git_space_metadata(&bare).unwrap();
        let checkout_space = git_space_metadata(&checkout).unwrap();
        let bare_auto_label = automatic_workspace_label(&bare, &bare_space.repo_root);
        let checkout_auto_label = automatic_workspace_label(&checkout, &checkout_space.repo_root);

        assert_eq!(bare_space.key, checkout_space.key);
        assert_eq!(bare_space.repo_name, ".bare");
        assert_eq!(checkout_space.repo_name, bare_space.repo_name);
        assert_eq!(bare_auto_label, bare.file_name().unwrap().to_str().unwrap());
        assert_eq!(
            checkout_auto_label,
            checkout.file_name().unwrap().to_str().unwrap()
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn embedded_dot_bare_source_and_checkout_use_container_repo_name() {
        let base = temp_test_dir("embedded-dot-bare");
        let seed = base.join("seed");
        let repo = base.join("reported-repo");
        let bare = repo.join(".bare");
        let checkout = repo.join("develop");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&seed, &["init", "--quiet"]);
        run_git(&seed, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&seed, &["config", "user.name", "Herdr Test"]);
        run_git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "initial"],
        );
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
        std::fs::write(repo.join(".git"), "gitdir: ./.bare\n").unwrap();
        run_git(
            &bare,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "develop",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );

        let source = git_space_metadata(&repo).unwrap();
        let linked = git_space_metadata(&checkout).unwrap();

        assert_eq!(source.repo_name, "reported-repo");
        assert_eq!(linked.repo_name, source.repo_name);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_space_metadata_marks_bare_dot_git_repo() {
        let root = temp_test_dir("bare-dot-git");
        run_git(&root, &["init", "--bare", ".git"]);

        let info = git_worktree_info(&root).expect("bare .git repo should be discovered");
        assert!(info.is_bare);
        assert!(!info.is_linked_worktree);
        assert_eq!(
            info.git_dir,
            canonicalize_best_effort_path(&root.join(".git"))
        );

        let metadata = git_space_metadata(&root).expect("bare .git repo should map to a git space");
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&root)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derive_label_prefers_repo_root_name() {
        let root = temp_test_dir("label-repo");
        let nested = root.join("nested");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            derive_label_from_cwd(&nested),
            root.file_name().and_then(|name| name.to_str()).unwrap()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derive_label_uses_path_name_outside_git() {
        let root = temp_test_dir("label-plain");
        let label = root.file_name().and_then(|name| name.to_str()).unwrap();

        assert_eq!(derive_label_from_cwd(Path::new(&root)), label);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_rev_parse_verify_reads_reftable_refs() {
        let root = temp_test_dir("reftable-ref-oid");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .unwrap();
        if !output.status.success() {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }

        run_git(&root, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&root, &["config", "user.name", "Herdr Test"]);
        run_git(&root, &["commit", "--allow-empty", "-m", "initial"]);

        let head_oid = git_rev_parse_verify(&root, "HEAD").unwrap();

        assert_eq!(
            git_rev_parse_verify(&root, "refs/heads/main").as_deref(),
            Some(head_oid.as_str())
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
