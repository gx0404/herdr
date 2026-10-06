use std::path::{Path, PathBuf};

use tracing::warn;

use super::{model::LoadedConfig, Config, CONFIG_PATH_ENV_VAR};

const KNOWN_TOP_LEVEL_CONFIG_KEYS: &[&str] = &[
    "account_usage",
    "advanced",
    "experimental",
    "keys",
    "language",
    "monitor",
    "onboarding",
    "remote",
    "server",
    "session",
    "terminal",
    "theme",
    "ui",
    "update",
    "worktrees",
];

pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

#[cfg(not(test))]
pub fn config_dir() -> PathBuf {
    resolve_config_dir()
}

/// 测试构建：线程本地覆盖原样返回；其余解析结果经 `test_dirs::confine` 限定在临时目录内。
#[cfg(test)]
pub fn config_dir() -> PathBuf {
    test_dirs::config_dir().unwrap_or_else(|| test_dirs::confine(resolve_config_dir(), "config"))
}

#[cfg(not(test))]
pub fn state_dir() -> PathBuf {
    resolve_state_dir()
}

/// 测试构建：口径同 `config_dir`。
#[cfg(test)]
pub fn state_dir() -> PathBuf {
    test_dirs::state_dir().unwrap_or_else(|| test_dirs::confine(resolve_state_dir(), "state"))
}

fn resolve_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    platform_config_dir()
}

fn resolve_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    platform_state_dir()
}

// 测试专用的线程本地目录覆盖：`cargo test` 在同一进程里并发跑测试，
// 经环境变量（XDG_STATE_HOME 等）改路径会串到别的测试；各测试线程改
// 自己的覆盖即可互不干扰。每个测试跑在自己的线程上，覆盖随线程结束。
//
// 没有覆盖时照常按 XDG_* → 平台目录解析，但结果不在临时目录下（开发机真实的
// herdr-dev 目录）就换成共享沙箱：单测在任何平台都碰不到真实的配置与状态目录。
#[cfg(test)]
pub(crate) mod test_dirs {
    use std::cell::RefCell;
    use std::ffi::{OsStr, OsString};
    use std::marker::PhantomData;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::thread::LocalKey;

    thread_local! {
        static CONFIG_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static STATE_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static HOME_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static CONFIG_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static SEARCH_PATH: RefCell<Option<OsString>> = const { RefCell::new(None) };
    }

    pub(crate) fn config_dir() -> Option<PathBuf> {
        CONFIG_DIR.with(|slot| slot.borrow().clone())
    }

    pub(crate) fn state_dir() -> Option<PathBuf> {
        STATE_DIR.with(|slot| slot.borrow().clone())
    }

    pub(crate) fn home_dir() -> Option<PathBuf> {
        HOME_DIR.with(|slot| slot.borrow().clone())
    }

    pub(crate) fn config_path() -> Option<PathBuf> {
        CONFIG_PATH.with(|slot| slot.borrow().clone())
    }

    /// 本线程找可执行文件用的搜索路径覆盖（格式同 PATH），见 `override_search_path`。
    pub(crate) fn search_path() -> Option<OsString> {
        SEARCH_PATH.with(|slot| slot.borrow().clone())
    }

    /// `override_home_dir` / `override_search_path` 的作用域句柄：析构时还原进入前的覆盖
    /// （可嵌套，按后进先出释放）。覆盖是线程本地的，所以句柄不能跨线程移动。
    pub(crate) struct ScopedOverride<T: 'static> {
        slot: &'static LocalKey<RefCell<Option<T>>>,
        previous: Option<T>,
        _not_send: PhantomData<*const ()>,
    }

    impl<T: 'static> Drop for ScopedOverride<T> {
        fn drop(&mut self) {
            // 线程收尾时线程本地变量可能已先销毁，用 try_with 免得析构里再 panic。
            let previous = self.previous.take();
            let _ = self.slot.try_with(|slot| *slot.borrow_mut() = previous);
        }
    }

    fn scoped_override<T: 'static>(
        slot: &'static LocalKey<RefCell<Option<T>>>,
        value: T,
    ) -> ScopedOverride<T> {
        ScopedOverride {
            slot,
            previous: slot.with(|current| current.replace(Some(value))),
            _not_send: PhantomData,
        }
    }

    /// 把本线程的「用户主目录」换成 `dir`：integration 的 agent 配置目录、SSH 配置发现等
    /// 读取处在测试构建里都先看它。代替改进程级 HOME / USERPROFILE——那会被同进程并发的
    /// 测试和它们起的子进程看到。
    pub(crate) fn override_home_dir(dir: impl AsRef<Path>) -> ScopedOverride<PathBuf> {
        scoped_override(&HOME_DIR, dir.as_ref().to_path_buf())
    }

    /// 把本线程找可执行文件用的搜索路径换成 `path`（格式同 PATH，空串即什么都找不到）。
    /// 代替改进程级 PATH——换掉或清空后，同进程并发的测试连 git 都找不到。只影响进程内
    /// 按 PATH 查找的代码（`integration::command_available`）；要让子进程看到，仍须持锁
    /// 改 PATH，且尽量往前追加而不是整个替换。
    pub(crate) fn override_search_path(path: impl AsRef<OsStr>) -> ScopedOverride<OsString> {
        scoped_override(&SEARCH_PATH, path.as_ref().to_os_string())
    }

    /// 覆盖 `config_dir()` 的解析结果（测试线程本地，不需还原）。
    pub(crate) fn set_config_dir(dir: PathBuf) {
        CONFIG_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
    }

    /// 覆盖 `state_dir()` 的解析结果（测试线程本地，不需还原）。
    pub(crate) fn set_state_dir(dir: PathBuf) {
        STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
    }

    /// 覆盖测试里「用户主目录」的解析结果：与 `override_home_dir` 同一覆盖，但不还原
    /// （测试线程本地，随线程结束）。
    pub(crate) fn set_home_dir(dir: PathBuf) {
        HOME_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
    }

    /// 覆盖 `config_path()` 的解析结果（测试线程本地，不需还原）。
    pub(crate) fn set_config_path(path: PathBuf) {
        CONFIG_PATH.with(|slot| *slot.borrow_mut() = Some(path));
    }

    /// 本线程隔离到临时目录的 `config_dir()`，否则 None。只认线程本地覆盖：进程环境
    /// 变量随时可能被同进程并发的别的测试改掉，共享沙箱是各测试共用的，都不算隔离。
    /// 返回校验过的路径本身，调用方不再二次解析。
    pub(crate) fn isolated_config_dir() -> Option<PathBuf> {
        config_dir().filter(|dir| is_under_temp_root(dir))
    }

    /// 本线程隔离到临时目录的 `state_dir()`，否则 None；口径同 `isolated_config_dir`。
    pub(crate) fn isolated_state_dir() -> Option<PathBuf> {
        state_dir().filter(|dir| is_under_temp_root(dir))
    }

    /// `isolate_dirs` 的作用域句柄：析构时还原进入前的覆盖（可嵌套，按后进先出释放），
    /// 并尽力删掉临时根目录。覆盖是线程本地的，所以句柄不能跨线程移动。
    pub(crate) struct IsolatedDirs {
        root: PathBuf,
        config_dir: PathBuf,
        state_dir: PathBuf,
        home_dir: PathBuf,
        previous_config_dir: Option<PathBuf>,
        previous_state_dir: Option<PathBuf>,
        previous_config_path: Option<PathBuf>,
        previous_home_dir: Option<PathBuf>,
        _not_send: PhantomData<*const ()>,
    }

    impl IsolatedDirs {
        pub(crate) fn config_dir(&self) -> &Path {
            &self.config_dir
        }

        pub(crate) fn state_dir(&self) -> &Path {
            &self.state_dir
        }

        /// 隔离的用户主目录（`<root>/home`，不预先创建）。
        pub(crate) fn home_dir(&self) -> &Path {
            &self.home_dir
        }
    }

    impl Drop for IsolatedDirs {
        fn drop(&mut self) {
            // 线程收尾时线程本地变量可能已先销毁，用 try_with 免得析构里再 panic。
            let config_dir = self.previous_config_dir.take();
            let _ = CONFIG_DIR.try_with(|slot| *slot.borrow_mut() = config_dir);
            let state_dir = self.previous_state_dir.take();
            let _ = STATE_DIR.try_with(|slot| *slot.borrow_mut() = state_dir);
            let config_path = self.previous_config_path.take();
            let _ = CONFIG_PATH.try_with(|slot| *slot.borrow_mut() = config_path);
            let home_dir = self.previous_home_dir.take();
            let _ = HOME_DIR.try_with(|slot| *slot.borrow_mut() = home_dir);
            remove_dir_eventually(&self.root);
        }
    }

    /// 删除测试临时目录，删不掉时有界重试（最多 30 秒，删掉即返回）：Windows 上刚退出的
    /// 子进程、杀毒扫描会短暂占着目录里的句柄，一次 `remove_dir_all` 失败就算了会留下残留。
    /// 到期仍删不掉只放弃，不让测试失败。
    pub(crate) fn remove_dir_eventually(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::fs::remove_dir_all(path).is_err()
            && path.exists()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 测试用的唯一临时目录：排他创建，跳过同名旧目录；析构时（含 panic 展开）
    /// 按 `remove_dir_eventually` 只删除本次认领的目录。
    pub(crate) struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            let label: String = name.chars().map(path_safe_char).take(32).collect();
            let path = claim_temp_dir(|| {
                short_temp_base().join(format!(
                    "herdr-{label}-{}-{}",
                    std::process::id(),
                    unique_id()
                ))
            })
            .unwrap_or_else(|err| panic!("failed to create test temp dir: {err}"));
            Self { path }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            remove_dir_eventually(&self.path);
        }
    }

    /// 进程内递增的序号，给测试的临时路径、socket 与命名管道取名用。pid 分开并发的
    /// nextest 进程，同一进程里并发的测试线程只能靠它分开：时间戳会撞（Windows 的系统
    /// 时钟只有 100ns 精度，两个线程同一刻取到同一个值）。
    pub(crate) fn unique_id() -> u64 {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// 新建唯一的临时根，把本线程的 `config_dir()` / `state_dir()` 指向其下的 `config/`
    /// 与 `state/`，`config_path()` 指向 `config/config.toml`（不再读进程级
    /// `HERDR_CONFIG_PATH`；要换配置文件，在此之后调 `set_config_path`），用户主目录
    /// （`override_home_dir` 的同一覆盖）指向 `home/`：`App::new` 列集成推荐等读
    /// `~/.claude`、`~/.codex` 的地方不再碰开发机的真实主目录。不改进程环境变量，所以
    /// 不必持 `test_config_env_lock`；`isolated_config_dir` / `isolated_state_dir` 据此
    /// 判定隔离。unix 上根放在 `/tmp` 下并保持路径短：socket 建在配置目录里，macOS 的
    /// socket 路径上限只有 104 字节。
    pub(crate) fn isolate_dirs(name: &str) -> IsolatedDirs {
        let label: String = name.chars().map(path_safe_char).take(24).collect();
        let root = claim_temp_dir(|| {
            short_temp_base().join(format!(
                "herdr-test-{label}-{}-{}",
                std::process::id(),
                unique_id()
            ))
        })
        .unwrap_or_else(|err| panic!("failed to create isolated test dir: {err}"));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let config_path = config_dir.join("config.toml");
        let home_dir = root.join("home");
        IsolatedDirs {
            previous_config_dir: CONFIG_DIR.with(|slot| slot.replace(Some(config_dir.clone()))),
            previous_state_dir: STATE_DIR.with(|slot| slot.replace(Some(state_dir.clone()))),
            previous_config_path: CONFIG_PATH.with(|slot| slot.replace(Some(config_path))),
            previous_home_dir: HOME_DIR.with(|slot| slot.replace(Some(home_dir.clone()))),
            root,
            config_dir,
            state_dir,
            home_dir,
            _not_send: PhantomData,
        }
    }

    /// 没有线程本地覆盖时的解析结果不在临时目录下，就换成共享沙箱。
    pub(super) fn confine(dir: PathBuf, kind: &str) -> PathBuf {
        if is_under_temp_root(&dir) {
            dir
        } else {
            sandbox_dir(kind)
        }
    }

    /// `HERDR_CONFIG_PATH` 同口径：指到临时目录以外（开发机 shell 里设的真实配置）时，
    /// 换成沙箱里的 config.toml。
    pub(super) fn confine_config_path(path: PathBuf) -> PathBuf {
        if is_under_temp_root(&path) {
            path
        } else {
            sandbox_dir("config").join("config.toml")
        }
    }

    /// 共享沙箱 `<base>/herdr-unit-sandbox-<user>/<kind>/<app>`：本进程各测试与并发的
    /// nextest 进程共用，不算隔离，也不自动清理。目录名带上用户名，免得多用户机器上
    /// 撞到别人建的目录没有权限。
    fn sandbox_dir(kind: &str) -> PathBuf {
        let user: String = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default()
            .chars()
            .map(path_safe_char)
            .collect();
        short_temp_base()
            .join(format!("herdr-unit-sandbox-{user}"))
            .join(kind)
            .join(super::app_dir_name())
    }

    fn path_safe_char(ch: char) -> char {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            ch
        } else {
            '_'
        }
    }

    /// 临时目录根：`std::env::temp_dir()` 与 `short_temp_base()`，各含规范化形式（macOS
    /// 的 `/tmp` 实为 `/private/tmp`，Windows 规范化后带 `\\?\` 前缀）。进程首次用到时的
    /// 根缓存下来（规范化要碰文件系统）；测试临时改了 TMPDIR 等变量时再补上当时的
    /// temp_dir。
    fn is_under_temp_root(path: &Path) -> bool {
        static INITIAL_ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
        let initial = INITIAL_ROOTS
            .get_or_init(|| with_canonical_forms(vec![std::env::temp_dir(), short_temp_base()]));
        if initial.iter().any(|root| path.starts_with(root)) {
            return true;
        }
        let current = std::env::temp_dir();
        !initial.contains(&current)
            && with_canonical_forms(vec![current])
                .iter()
                .any(|root| path.starts_with(root))
    }

    fn with_canonical_forms(roots: Vec<PathBuf>) -> Vec<PathBuf> {
        let canonical: Vec<PathBuf> = roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .collect();
        roots.into_iter().chain(canonical).collect()
    }

    // 测试目录的基准：unix 用短的 `/tmp`（本仓库测试建 socket 也用它，见 macOS 的
    // socket 路径上限），其余平台用系统临时目录。
    #[cfg(unix)]
    fn short_temp_base() -> PathBuf {
        PathBuf::from("/tmp")
    }

    #[cfg(not(unix))]
    fn short_temp_base() -> PathBuf {
        std::env::temp_dir()
    }

    fn claim_temp_dir(mut candidate: impl FnMut() -> PathBuf) -> std::io::Result<PathBuf> {
        loop {
            let path = candidate();
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    mod tests {
        use super::*;

        #[test]
        fn claim_temp_dir_preserves_existing_candidate_and_sentinel() {
            let fixture = TempDir::new("claim-collision");
            let occupied = fixture.join("occupied");
            std::fs::create_dir(&occupied).unwrap();
            let sentinel = occupied.join("sentinel");
            std::fs::write(&sentinel, b"must survive").unwrap();
            let next = fixture.join("next");
            let mut candidates = [occupied.clone(), next.clone()].into_iter();
            let owned = TempDir {
                path: claim_temp_dir(|| candidates.next().expect("only one collision")).unwrap(),
            };
            assert!(
                sentinel.is_file(),
                "existing candidate sentinel was deleted"
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"must survive");
            assert_eq!(owned.path(), next);
            drop(owned);
            assert!(!next.exists());
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"must survive");
        }

        #[test]
        fn claim_temp_dir_creates_first_available_candidate() {
            let fixture = TempDir::new("claim-success");
            let path = fixture.join("new");
            let mut calls = 0;
            let claimed = claim_temp_dir(|| {
                calls += 1;
                path.clone()
            })
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(claimed, path);
            assert!(claimed.is_dir());
            assert_eq!(std::fs::read_dir(claimed).unwrap().count(), 0);
        }

        #[test]
        fn claim_temp_dir_propagates_other_errors_without_retry() {
            let fixture = TempDir::new("claim-error");
            let file = fixture.join("file");
            std::fs::write(&file, b"untouched").unwrap();
            let path = file.join("child");
            let expected = std::fs::create_dir(&path).unwrap_err();
            assert_ne!(expected.kind(), std::io::ErrorKind::AlreadyExists);
            let mut calls = 0;
            let error = claim_temp_dir(|| {
                calls += 1;
                assert_eq!(calls, 1, "non-collision error must not retry");
                path.clone()
            })
            .unwrap_err();
            assert_eq!(error.kind(), expected.kind());
            assert_eq!(error.raw_os_error(), expected.raw_os_error());
            assert_eq!(std::fs::read(file).unwrap(), b"untouched");
        }

        /// 一定不在任何临时目录根下的路径：临时目录所在文件系统的根下。
        fn outside_temp_roots() -> PathBuf {
            let root = std::env::temp_dir()
                .ancestors()
                .last()
                .map(Path::to_path_buf)
                .unwrap();
            let path = root.join("herdr-real-profile-probe");
            assert!(!is_under_temp_root(&path), "{}", path.display());
            path
        }

        #[test]
        fn isolate_dirs_overrides_this_thread_and_restores_on_drop() {
            let outer_state = std::env::temp_dir().join("herdr-isolate-dirs-outer-state");
            set_state_dir(outer_state.clone());
            let root = {
                let dirs = isolate_dirs("outer");
                let root = dirs.config_dir().parent().unwrap().to_path_buf();
                assert!(root.is_dir());
                assert_eq!(crate::config::config_dir().as_path(), dirs.config_dir());
                assert_eq!(crate::config::state_dir().as_path(), dirs.state_dir());
                assert_eq!(
                    crate::config::config_path(),
                    dirs.config_dir().join("config.toml")
                );
                assert_eq!(home_dir().as_deref(), Some(dirs.home_dir()));
                assert!(dirs.home_dir().starts_with(&root));
                assert_eq!(isolated_config_dir().as_deref(), Some(dirs.config_dir()));
                assert_eq!(isolated_state_dir().as_deref(), Some(dirs.state_dir()));
                {
                    let inner = isolate_dirs("inner");
                    assert_ne!(inner.config_dir(), dirs.config_dir());
                    assert_eq!(isolated_config_dir().as_deref(), Some(inner.config_dir()));
                    assert_eq!(isolated_state_dir().as_deref(), Some(inner.state_dir()));
                    assert_eq!(home_dir().as_deref(), Some(inner.home_dir()));
                }
                // 内层结束后回到外层的隔离目录。
                assert_eq!(isolated_config_dir().as_deref(), Some(dirs.config_dir()));
                assert_eq!(isolated_state_dir().as_deref(), Some(dirs.state_dir()));
                assert_eq!(home_dir().as_deref(), Some(dirs.home_dir()));
                root
            };
            // 外层结束：还原进入前的覆盖（state 有，config、config_path 与主目录没有），删掉
            // 临时根。
            assert_eq!(state_dir(), Some(outer_state));
            assert_eq!(config_dir(), None);
            assert_eq!(config_path(), None);
            assert_eq!(home_dir(), None);
            assert!(!root.exists());
            STATE_DIR.with(|slot| slot.borrow_mut().take());
        }

        #[test]
        fn scoped_overrides_nest_and_restore_the_previous_value() {
            let outer = std::env::temp_dir().join("herdr-override-outer-home");
            let inner = std::env::temp_dir().join("herdr-override-inner-home");
            {
                let _outer_home = override_home_dir(&outer);
                let _outer_path = override_search_path(&outer);
                {
                    let _inner_home = override_home_dir(&inner);
                    let _no_path = override_search_path("");
                    assert_eq!(home_dir(), Some(inner.clone()));
                    assert_eq!(search_path(), Some(OsString::new()));
                }
                assert_eq!(home_dir(), Some(outer.clone()));
                assert_eq!(search_path(), Some(outer.clone().into_os_string()));
            }
            assert_eq!(home_dir(), None);
            assert_eq!(search_path(), None);
        }

        #[test]
        fn unisolated_dirs_never_resolve_outside_a_temp_root() {
            // 进程环境变量（可能正被并发的别的测试设成临时目录）不算隔离；没有线程本地
            // 覆盖时解析结果要么在临时目录下，要么是共享沙箱，绝不是真实的 herdr-dev 目录。
            assert_eq!(isolated_config_dir(), None);
            assert_eq!(isolated_state_dir(), None);
            for path in [
                crate::config::config_dir(),
                crate::config::state_dir(),
                crate::config::config_path(),
            ] {
                assert!(is_under_temp_root(&path), "{}", path.display());
            }
        }

        #[test]
        fn confinement_keeps_temp_paths_and_sandboxes_everything_else() {
            let temp = std::env::temp_dir().join("herdr-confine-probe");
            assert_eq!(confine(temp.clone(), "config"), temp);
            let short = short_temp_base().join("hs-confine-probe");
            assert_eq!(confine(short.clone(), "state"), short);
            if let Ok(canonical) = std::fs::canonicalize(std::env::temp_dir()) {
                let canonical = canonical.join("herdr-confine-probe");
                assert_eq!(confine(canonical.clone(), "config"), canonical);
            }

            let real = outside_temp_roots();
            let config_sandbox = confine(real.clone(), "config");
            let state_sandbox = confine(real.clone(), "state");
            let app = crate::config::app_dir_name();
            assert!(is_under_temp_root(&config_sandbox));
            assert!(config_sandbox.ends_with(Path::new("config").join(app)));
            assert!(state_sandbox.ends_with(Path::new("state").join(app)));
            let sandbox_root = config_sandbox.ancestors().nth(2).unwrap();
            assert!(state_sandbox.starts_with(sandbox_root));
            assert!(sandbox_root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("herdr-unit-sandbox-")));
            assert_eq!(
                confine_config_path(real.join("config.toml")),
                config_sandbox.join("config.toml")
            );
            let temp_config = temp.join("config.toml");
            assert_eq!(confine_config_path(temp_config.clone()), temp_config);
        }

        #[test]
        fn explicit_overrides_are_kept_but_only_temp_ones_count_as_isolated() {
            // 线程本地覆盖是测试自己给的，原样返回；不在临时目录下就不算隔离。
            let real = outside_temp_roots();
            set_config_dir(real.clone());
            set_state_dir(real.clone());
            assert_eq!(crate::config::config_dir(), real);
            assert_eq!(crate::config::state_dir(), real);
            assert_eq!(isolated_config_dir(), None);
            assert_eq!(isolated_state_dir(), None);

            let temp = std::env::temp_dir().join("herdr-explicit-override-probe");
            set_config_dir(temp.clone());
            assert_eq!(isolated_config_dir(), Some(temp));
            CONFIG_DIR.with(|slot| slot.borrow_mut().take());
            STATE_DIR.with(|slot| slot.borrow_mut().take());
        }
    }
}

#[cfg(windows)]
fn platform_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("APPDATA") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Roaming")
            .join(app_dir_name());
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(format!(".config/{}", app_dir_name()));
    }
    std::env::temp_dir().join(app_dir_name())
}

#[cfg(not(windows))]
fn platform_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".config/{}", app_dir_name()))
    } else {
        std::env::temp_dir().join(app_dir_name())
    }
}

#[cfg(windows)]
fn platform_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Local")
            .join(app_dir_name());
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(format!(".local/state/{}", app_dir_name()));
    }
    std::env::temp_dir().join(format!("{}-state", app_dir_name()))
}

#[cfg(not(windows))]
fn platform_state_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".local/state/{}", app_dir_name()))
    } else {
        std::env::temp_dir().join(format!("{}-state", app_dir_name()))
    }
}

/// Normalize UTF-8 byte-order marks in config text.
///
/// TOML tolerates a single BOM at the very start of the document, but a BOM at
/// the start of a later line makes the parser reject the whole file. A
/// line-oriented edit can displace a leading BOM into the middle of the file,
/// so drop line-start BOMs that the TOML parser actually rejects. A U+FEFF that
/// is valid string data is kept, because its parse error would not point at it.
fn normalize_utf8_bom(content: &str) -> String {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !content.contains('\u{feff}') {
        return content.to_owned();
    }

    let mut normalized = content.to_owned();
    while let Err(error) = normalized.parse::<toml::Value>() {
        let Some(span) = error.span() else {
            break;
        };
        if normalized.get(span.clone()) != Some("\u{feff}") {
            break;
        }
        normalized.replace_range(span, "");
    }
    normalized
}

pub(super) fn read_optional_config(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(normalize_utf8_bom(&content))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

impl Config {
    pub fn load() -> LoadedConfig {
        let path = config_path();
        let content = match read_optional_config(&path) {
            Ok(Some(content)) => content,
            Ok(None) => {
                return LoadedConfig {
                    config: Self::default(),
                    diagnostics: Vec::new(),
                    invalid_sections: Vec::new(),
                };
            }
            Err(err) => {
                warn!(err = %err, "config read error, using defaults");
                return LoadedConfig {
                    config: Self::default(),
                    diagnostics: vec![format!("config read error: {err}; using defaults")],
                    invalid_sections: Vec::new(),
                };
            }
        };

        match deserialize_with_ignored::<Config, _>(toml::Deserializer::new(&content)) {
            Ok((config, ignored_keys)) => {
                let (unknown_sections, mut diagnostics) =
                    unknown_top_level_sections_from_str(&content);
                diagnostics.extend(unknown_config_key_diagnostics(
                    ignored_keys
                        .into_iter()
                        .filter(|path| {
                            !matches!(path.as_slice(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                        })
                        .collect(),
                    None,
                ));
                diagnostics.extend(config.collect_diagnostics());
                LoadedConfig {
                    config,
                    diagnostics,
                    invalid_sections: Vec::new(),
                }
            }
            Err(err) => {
                warn!(err = %err, "config parse error, using defaults");
                LoadedConfig {
                    config: Self::default(),
                    diagnostics: vec![format!("config parse error: {err}; using defaults")],
                    invalid_sections: Vec::new(),
                }
            }
        }
    }
}

pub(super) fn resolve_config_relative_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(path)
}

pub fn config_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = test_dirs::config_path() {
        return path;
    }
    if let Some(path) = config_path_from_env() {
        return path;
    }
    config_dir().join("config.toml")
}

#[cfg(not(test))]
fn config_path_from_env() -> Option<PathBuf> {
    std::env::var(CONFIG_PATH_ENV_VAR).ok().map(PathBuf::from)
}

/// 测试构建：`HERDR_CONFIG_PATH` 与目录同样限定在临时目录内（见 `test_dirs`）。
#[cfg(test)]
fn config_path_from_env() -> Option<PathBuf> {
    std::env::var(CONFIG_PATH_ENV_VAR)
        .ok()
        .map(|path| test_dirs::confine_config_path(PathBuf::from(path)))
}

pub fn config_diagnostic_summary(diagnostics: &[String]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }

    let target = config_path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml")
        .to_string();
    let read_error = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.starts_with("config read error:"));
    let impact = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("using defaults"))
    {
        if read_error {
            " unreadable; using defaults"
        } else {
            " invalid; using defaults"
        }
    } else if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("keeping current config"))
    {
        if read_error {
            " unreadable; keeping current config"
        } else {
            " invalid; keeping current config"
        }
    } else if diagnostics
        .iter()
        .all(|diagnostic| diagnostic.starts_with("unknown config key "))
    {
        " has unknown keys"
    } else {
        ""
    };

    Some(format!("{target}{impact}; herdr config check"))
}

pub fn load_live_config() -> Result<LoadedConfig, Vec<String>> {
    let path = config_path();
    let content = match read_optional_config(&path) {
        Ok(Some(content)) => content,
        Ok(None) => {
            return Ok(LoadedConfig {
                config: Config::default(),
                diagnostics: Vec::new(),
                invalid_sections: Vec::new(),
            });
        }
        Err(err) => {
            return Err(vec![format!(
                "config read error: {err}; keeping current config"
            )]);
        }
    };
    load_live_config_from_str(&content)
}

fn load_live_config_from_str(content: &str) -> Result<LoadedConfig, Vec<String>> {
    let value = content
        .parse::<toml::Value>()
        .map_err(|err| vec![format!("config parse error: {err}; keeping current config")])?;
    let table = value.as_table().ok_or_else(|| {
        vec![
            "config parse error: top-level config must be a table; keeping current config"
                .to_string(),
        ]
    })?;

    let mut config = Config::default();
    let mut diagnostics = unknown_top_level_section_diagnostics(table);
    diagnostics.extend(unknown_top_level_config_key_diagnostics(table));
    let mut invalid_sections = Vec::new();

    if let Some(value) = table.get("onboarding") {
        match value.clone().try_into::<Option<bool>>() {
            Ok(onboarding) => config.onboarding = onboarding,
            Err(err) => diagnostics.push(format!(
                "invalid onboarding setting: {err}; keeping current onboarding state"
            )),
        }
    }

    load_live_section(
        table,
        "theme",
        "theme config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.theme = section,
    );
    load_live_section(
        table,
        "keys",
        "keybinding config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.keys = section,
    );
    load_live_section(
        table,
        "terminal",
        "terminal config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.terminal = section,
    );
    load_live_section(
        table,
        "session",
        "session config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.session = section,
    );
    load_live_section(
        table,
        "server",
        "server config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.server = section,
    );
    load_live_section(
        table,
        "update",
        "update config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.update = section,
    );
    load_live_section(
        table,
        "ui",
        "ui config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.ui = section,
    );
    load_live_section(
        table,
        "advanced",
        "advanced config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.advanced = section,
    );
    load_live_section(
        table,
        "worktrees",
        "worktree config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.worktrees = section,
    );
    load_live_section(
        table,
        "experimental",
        "experimental config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.experimental = section,
    );
    load_live_section(
        table,
        "remote",
        "remote config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.remote = section,
    );

    load_live_section(
        table,
        "monitor",
        "monitor config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.monitor = section,
    );
    load_live_section(
        table,
        "account_usage",
        "account usage config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.account_usage = section,
    );
    diagnostics.extend(config.theme.diagnostics());
    diagnostics.extend(super::observability::diagnostics(
        &config.monitor,
        &config.account_usage,
    ));

    Ok(LoadedConfig {
        config,
        diagnostics,
        invalid_sections,
    })
}

fn unknown_top_level_sections_from_str(content: &str) -> (Vec<String>, Vec<String>) {
    let Ok(value) = content.parse::<toml::Value>() else {
        return (Vec::new(), Vec::new());
    };
    let Some(table) = value.as_table() else {
        return (Vec::new(), Vec::new());
    };

    let mut keys = Vec::new();
    let mut diagnostics = Vec::new();
    for (key, value) in table {
        if let Some(diagnostic) = unknown_top_level_section_diagnostic(key, value) {
            keys.push(key.clone());
            diagnostics.push(diagnostic);
        }
    }
    (keys, diagnostics)
}

fn unknown_top_level_section_diagnostics(
    table: &toml::map::Map<String, toml::Value>,
) -> Vec<String> {
    table
        .iter()
        .filter_map(|(key, value)| unknown_top_level_section_diagnostic(key, value))
        .collect()
}

fn unknown_top_level_section_diagnostic(key: &str, value: &toml::Value) -> Option<String> {
    if KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key) {
        return None;
    }

    let header = if value.is_table() {
        format!("[{key}]")
    } else if value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    {
        format!("[[{key}]]")
    } else {
        return None;
    };

    if key == "toast" {
        Some(format!(
            "unknown config section {header}; did you mean [ui.toast]? ignoring section"
        ))
    } else {
        Some(format!("unknown config section {header}; ignoring section"))
    }
}

fn unknown_top_level_config_key_diagnostics(
    table: &toml::map::Map<String, toml::Value>,
) -> Vec<String> {
    let paths = table
        .iter()
        .filter(|(key, value)| {
            !KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key.as_str())
                && unknown_top_level_section_diagnostic(key, value).is_none()
        })
        .map(|(key, _)| vec![ConfigKeyPathSegment::Key(key.clone())])
        .collect();
    unknown_config_key_diagnostics(paths, None)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConfigKeyPathSegment {
    Key(String),
    Index(usize),
}

fn config_key_path(path: &serde_ignored::Path<'_>) -> Vec<ConfigKeyPathSegment> {
    fn visit(path: &serde_ignored::Path<'_>, segments: &mut Vec<ConfigKeyPathSegment>) {
        match path {
            serde_ignored::Path::Root => {}
            serde_ignored::Path::Seq { parent, index } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Index(*index));
            }
            serde_ignored::Path::Map { parent, key } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Key(key.clone()));
            }
            serde_ignored::Path::Some { parent }
            | serde_ignored::Path::NewtypeStruct { parent }
            | serde_ignored::Path::NewtypeVariant { parent } => visit(parent, segments),
        }
    }

    let mut segments = Vec::new();
    visit(path, &mut segments);
    segments
}

fn format_config_key_path(path: &[ConfigKeyPathSegment]) -> String {
    path.iter()
        .map(|segment| match segment {
            ConfigKeyPathSegment::Key(key)
                if !key.is_empty()
                    && key.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                    }) =>
            {
                key.clone()
            }
            ConfigKeyPathSegment::Key(key) => toml::Value::String(key.clone()).to_string(),
            ConfigKeyPathSegment::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn unknown_config_key_diagnostics(
    paths: Vec<Vec<ConfigKeyPathSegment>>,
    section: Option<&str>,
) -> Vec<String> {
    let mut paths: Vec<Vec<ConfigKeyPathSegment>> = paths
        .into_iter()
        .map(|mut path| {
            if let Some(section) = section {
                path.insert(0, ConfigKeyPathSegment::Key(section.to_string()));
            }
            path
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            format!(
                "unknown config key {}; ignoring key",
                format_config_key_path(&path)
            )
        })
        .collect()
}

fn deserialize_with_ignored<'de, T, D>(
    deserializer: D,
) -> Result<(T, Vec<Vec<ConfigKeyPathSegment>>), D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let mut ignored = Vec::new();
    let value = serde_ignored::deserialize(deserializer, |path| {
        ignored.push(config_key_path(&path));
    })?;
    Ok((value, ignored))
}

fn load_live_section<T>(
    table: &toml::map::Map<String, toml::Value>,
    section: &'static str,
    label: &str,
    diagnostics: &mut Vec<String>,
    invalid_sections: &mut Vec<String>,
    apply: impl FnOnce(T),
) where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = table.get(section) else {
        return;
    };

    match deserialize_with_ignored(value.clone()) {
        Ok((section_config, ignored_keys)) => {
            diagnostics.extend(unknown_config_key_diagnostics(ignored_keys, Some(section)));
            apply(section_config);
        }
        Err(err) => {
            diagnostics.push(format!(
                "invalid {label}: {err}; keeping current {section} settings"
            ));
            invalid_sections.push(section.to_string());
        }
    }
}

pub(crate) fn upsert_top_level_bool(content: &str, key: &str, value: bool) -> String {
    upsert_top_level_value(content, key, &value.to_string())
}

pub(crate) fn upsert_top_level_value(content: &str, key: &str, value: &str) -> String {
    let replacement = format!("{key} = {value}");
    let mut lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
    let mut in_section = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = true;
            continue;
        }
        if in_section {
            continue;
        }
        if trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")) {
            *line = replacement.clone();
            return lines.join("\n") + "\n";
        }
    }

    if lines.is_empty() {
        format!("{replacement}\n")
    } else {
        format!("{replacement}\n{}\n", lines.join("\n").trim_end())
    }
}

/// Write a key = value pair in a TOML section (creates section if missing).
pub fn upsert_section_value(content: &str, section: &str, key: &str, value: &str) -> String {
    upsert_section_raw(content, section, key, value)
}

pub fn upsert_section_bool(content: &str, section: &str, key: &str, value: bool) -> String {
    upsert_section_raw(content, section, key, &value.to_string())
}

pub fn remove_section_key(content: &str, section: &str, key: &str) -> String {
    let header = format!("[{section}]");
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    let mut in_section = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == header;
            result.push(line.to_string());
            i += 1;
            continue;
        }

        if in_section
            && (trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")))
        {
            i += 1;
            continue;
        }

        result.push(line.to_string());
        i += 1;
    }

    result.join("\n") + "\n"
}

pub fn remove_keybinding_config_sections(content: &str) -> (String, bool) {
    let mut result = Vec::new();
    let mut removed = false;
    let mut skipping_key_section = false;
    let mut in_table = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(table_name) = toml_table_header_name(trimmed) {
            in_table = true;
            skipping_key_section = is_keys_table_name(table_name);
            if skipping_key_section {
                removed = true;
                continue;
            }
        } else if skipping_key_section || (!in_table && is_top_level_keys_assignment(trimmed)) {
            removed = true;
            continue;
        }

        result.push(line.to_string());
    }

    let mut updated = result.join("\n");
    if content.ends_with('\n') || !updated.is_empty() {
        updated.push('\n');
    }
    (updated, removed)
}

fn toml_table_header_name(trimmed: &str) -> Option<&str> {
    if let Some(name) = trimmed
        .strip_prefix("[[")
        .and_then(|value| value.strip_suffix("]]"))
    {
        return Some(name.trim());
    }
    trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .map(str::trim)
}

fn is_keys_table_name(name: &str) -> bool {
    name == "keys" || name.starts_with("keys.")
}

fn is_top_level_keys_assignment(trimmed: &str) -> bool {
    trimmed.starts_with("keys ") || trimmed.starts_with("keys=") || trimmed.starts_with("keys.")
}

fn upsert_section_raw(content: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let assignment = format!("{key} = {value}");
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    let mut found_section = false;
    let mut inserted = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed == header {
            found_section = true;
            result.push(line.to_string());
            i += 1;

            while i < lines.len() {
                let current = lines[i];
                let current_trimmed = current.trim();
                if current_trimmed.starts_with('[') && current_trimmed.ends_with(']') {
                    if !inserted {
                        result.push(assignment.clone());
                        inserted = true;
                    }
                    break;
                }

                if current_trimmed.starts_with(&format!("{key} "))
                    || current_trimmed.starts_with(&format!("{key}="))
                {
                    result.push(assignment.clone());
                    inserted = true;
                } else {
                    result.push(current.to_string());
                }
                i += 1;
            }

            continue;
        }

        result.push(line.to_string());
        i += 1;
    }

    if !found_section {
        if !result.is_empty() && !result.last().is_some_and(|line| line.trim().is_empty()) {
            result.push(String::new());
        }
        result.push(header);
        result.push(assignment);
    } else if !inserted {
        result.push(assignment);
    }

    result.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_top_level_bool_replaces_existing_value() {
        let content = "onboarding = true\n[keys]\nprefix = \"ctrl+b\"\n";
        let updated = upsert_top_level_bool(content, "onboarding", false);
        assert!(updated.contains("onboarding = false"));
        assert!(!updated.contains("onboarding = true"));
    }

    #[test]
    fn upsert_top_level_value_prepends_missing_key_before_sections() {
        let content = "# herdr configuration\n[theme]\nname = \"catppuccin\"\n";
        let updated = upsert_top_level_value(content, "language", "\"zh-CN\"");
        assert!(updated.starts_with("language = \"zh-CN\"\n"));
        assert!(updated.contains("[theme]"));
        let parsed = updated.parse::<toml::Value>().unwrap();
        assert_eq!(
            parsed.get("language").and_then(|v| v.as_str()),
            Some("zh-CN")
        );
    }

    #[test]
    fn language_top_level_key_is_not_flagged_unknown() {
        let content = "language = \"zh-CN\"\n[theme]\nname = \"catppuccin\"\n";
        let loaded = super::load_live_config_from_str(content).expect("config loads");
        let unknown = loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("unknown config key"));
        assert!(!unknown, "diagnostics: {:?}", loaded.diagnostics);
    }

    #[test]
    fn upsert_top_level_value_replaces_existing_and_keeps_sections() {
        let content = "language = \"en\"\n[theme]\nname = \"catppuccin\"\n";
        let updated = upsert_top_level_value(content, "language", "\"zh-CN\"");
        assert!(updated.contains("language = \"zh-CN\""));
        assert!(!updated.contains("language = \"en\""));
        assert!(updated.contains("[theme]"));
    }

    #[test]
    fn upsert_section_bool_adds_missing_section() {
        let updated = upsert_section_bool("", "ui.toast", "enabled", true);
        assert!(updated.contains("[ui.toast]"));
        assert!(updated.contains("enabled = true"));
    }

    #[test]
    fn remove_section_key_removes_matching_key_from_section() {
        let content =
            "[ui.toast]\nenabled = true\ndelivery = \"herdr\"\n[ui.sound]\nenabled = true\n";
        let updated = remove_section_key(content, "ui.toast", "enabled");
        assert!(!updated.contains("[ui.toast]\nenabled = true"));
        assert!(updated.contains("delivery = \"herdr\""));
        assert!(updated.contains("[ui.sound]\nenabled = true"));
    }

    #[test]
    fn config_diagnostic_summary_uses_compact_actionable_banner() {
        let diagnostics = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
            "five".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_unknown_keys_compactly() {
        let diagnostics = vec![
            "unknown config key ui.mouse_captur; ignoring key".to_string(),
            "unknown config key keys.new_tabb; ignoring key".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml has unknown keys; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_keeps_mixed_diagnostics_generic() {
        let diagnostics = vec![
            "invalid ui config: invalid type: string; keeping current ui settings".to_string(),
            "unknown config key keys.new_tabb; ignoring key".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_default_fallback() {
        let diagnostics = vec![
            "config parse error: TOML parse error at line 33, column 8\n   |\n33 | type = \"popup\"\n   |        ^^^^^^^\nunknown variant `popup`; using defaults"
                .to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml invalid; using defaults; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_unreadable_config_impact() {
        let startup = vec!["config read error: permission denied; using defaults".to_string()];
        assert_eq!(
            config_diagnostic_summary(&startup).as_deref(),
            Some("config.toml unreadable; using defaults; herdr config check")
        );

        let reload =
            vec!["config read error: permission denied; keeping current config".to_string()];
        assert_eq!(
            config_diagnostic_summary(&reload).as_deref(),
            Some("config.toml unreadable; keeping current config; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_retained_live_config() {
        let diagnostics = vec![
            "config parse error: TOML parse error at line 7, column 4; keeping current config"
                .to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml invalid; keeping current config; herdr config check")
        );
    }

    #[test]
    fn config_loaders_report_unreadable_path() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path =
            std::env::temp_dir().join(format!("herdr-config-unreadable-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var(CONFIG_PATH_ENV_VAR, &path);

        let startup = Config::load();
        assert!(startup
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("config read error")
                && diagnostic.contains("using defaults")));

        let reload = load_live_config().unwrap_err();
        assert!(reload.iter().any(|diagnostic| {
            diagnostic.contains("config read error")
                && diagnostic.contains("keeping current config")
        }));

        let _ = std::fs::remove_dir_all(path);
    }

    /// 在本线程隔离目录的 config.toml 里写入 `content` 后走启动加载：线程本地覆盖，不经
    /// 进程级 `HERDR_CONFIG_PATH`，不必持测试环境锁，也不会让并发的别的测试读到这份配置。
    fn load_startup_config(name: &str, content: impl AsRef<[u8]>) -> LoadedConfig {
        let dirs = test_dirs::isolate_dirs(name);
        std::fs::create_dir_all(dirs.config_dir()).unwrap();
        std::fs::write(config_path(), content).unwrap();
        Config::load()
    }

    #[test]
    fn load_live_config_parses_session_section() {
        let loaded = load_live_config_from_str(
            r#"
[session]
resume_agents_on_restore = true
"#,
        )
        .unwrap();

        assert!(loaded.config.session.resume_agents_on_restore);
        assert!(loaded.diagnostics.is_empty());
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn load_live_config_warns_about_unknown_theme_names() {
        let loaded = load_live_config_from_str(
            r#"
[theme]
name = "catppucin"
"#,
        )
        .unwrap();

        assert_eq!(loaded.diagnostics.len(), 1);
        assert!(loaded.diagnostics[0].contains("theme.name = \"catppucin\""));
    }

    #[test]
    fn load_live_config_warns_about_unknown_top_level_sections() {
        let loaded = load_live_config_from_str(
            r#"
[toast]
delivery = "system"

[ui.toast]
delivery = "herdr"
"#,
        )
        .unwrap();

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [toast]; did you mean [ui.toast]? ignoring section"]
        );
        assert!(loaded.invalid_sections.is_empty());
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::Herdr
        );
    }

    #[test]
    fn load_live_config_warns_about_unknown_keys_and_applies_known_siblings() {
        let loaded = load_live_config_from_str(
            r##"
plugin = []

[theme.custom]
accentt = "#ffffff"

[advanced]
scrollback_lines = 42

[keys]
fullscreen = "prefix+z"
new_tabb = "prefix+t"

[[keys.command]]
key = "prefix+g"
command = "git status"
descrption = "status"

[ui]
mouse_capture = false
mouse_captur = true
"foo.bar" = true
"foo.?.bar" = false

[ui.toast]
enabled = true
delivry = "system"

[ui.sidebar.agents.rows_by_agent]
claude = [["terminal_title"]]
"##,
        )
        .unwrap();

        assert_eq!(
            loaded.diagnostics,
            vec![
                "unknown config key plugin; ignoring key",
                "unknown config key theme.custom.accentt; ignoring key",
                "unknown config key keys.command.0.descrption; ignoring key",
                "unknown config key keys.new_tabb; ignoring key",
                "unknown config key ui.\"foo.?.bar\"; ignoring key",
                "unknown config key ui.\"foo.bar\"; ignoring key",
                "unknown config key ui.mouse_captur; ignoring key",
                "unknown config key ui.toast.delivry; ignoring key",
            ]
        );
        assert!(loaded.invalid_sections.is_empty());
        assert_eq!(loaded.config.advanced.scrollback_limit_bytes, 42);
        assert!(!loaded.config.ui.mouse_capture);
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::Herdr
        );
        assert!(loaded
            .config
            .keybinds()
            .zoom
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+z"));
    }

    #[test]
    fn load_live_config_accepts_legacy_agent_panel_scope_without_warning() {
        let loaded = load_live_config_from_str(
            r#"
[ui]
agent_panel_scope = "current"
agent_panel_sort = "priority"
"#,
        )
        .unwrap();

        assert!(loaded.diagnostics.is_empty());
        assert!(loaded.invalid_sections.is_empty());
        // `"priority"` 是已移除的旧取值：静默回退默认，`[ui]` 不失效。
        assert_eq!(
            loaded.config.ui.agent_panel_sort,
            super::super::AgentPanelSortConfig::Spaces
        );
    }

    #[test]
    fn startup_config_accepts_removed_priority_sort_without_falling_back() {
        let loaded = load_startup_config(
            "removed-priority-sort",
            "[ui]\nagent_panel_sort = \"priority\"\nsidebar_width = 31\n",
        );

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(
            loaded.config.ui.agent_panel_sort,
            super::super::AgentPanelSortConfig::Spaces
        );
        assert_eq!(
            loaded.config.ui.sidebar_width, 31,
            "旧取值不让整份配置回退默认"
        );
    }

    #[test]
    fn load_live_config_accepts_legacy_toggle_usage_dashboard_without_warning() {
        // 字符串、数组与形状不对的值都要被吞掉：残留键不能让整个 [keys] 失效。
        for value in [r#""prefix+a""#, r#"["prefix+a", "f9"]"#, "5"] {
            let loaded = load_live_config_from_str(&format!(
                "[keys]\ntoggle_usage_dashboard = {value}\nzoom = \"prefix+shift+z\"\n"
            ))
            .unwrap();

            assert!(
                loaded.diagnostics.is_empty(),
                "{value}: {:?}",
                loaded.diagnostics
            );
            assert!(loaded.invalid_sections.is_empty(), "{value}");
            assert!(
                loaded
                    .config
                    .keybinds()
                    .zoom
                    .bindings
                    .iter()
                    .any(|binding| binding.label == "prefix+shift+z"),
                "{value}: 同段其余键位照常生效"
            );
        }
    }

    #[test]
    fn load_live_config_discards_ignored_keys_from_an_invalid_section() {
        let loaded = load_live_config_from_str(
            r#"
[ui]
mouse_capture = "yes"
mouse_captur = true
"#,
        )
        .unwrap();

        assert_eq!(loaded.diagnostics.len(), 1);
        assert!(loaded.diagnostics[0].contains("invalid ui config"));
        assert!(!loaded.diagnostics[0].starts_with("unknown config key"));
        assert_eq!(loaded.invalid_sections, vec!["ui"]);
    }

    /// 用户旧 config.toml 里残留的已删 agent 覆盖键：启动与热重载两条路径都只忽略
    /// 这些键，同段其余设置照常生效，不回退默认、不拒收整段。
    const RETIRED_AGENT_KEYS_CONFIG: &str = r#"
[ui]
mouse_capture = false

[ui.sound]
enabled = true

[ui.sound.agents]
claude = "off"
droid = "off"
cursor = "on"
github_copilot = "default"

[ui.sidebar.agents.rows_by_agent]
claude = [["terminal_title"]]
cursor = [["agent"]]
omp = [["agent"]]
"#;

    fn assert_retired_agent_keys_were_only_ignored(loaded: &LoadedConfig) {
        assert!(loaded.invalid_sections.is_empty(), "{loaded:?}");
        assert!(
            loaded.diagnostics.iter().all(|diagnostic| {
                !diagnostic.contains("using defaults") && !diagnostic.contains("keeping current")
            }),
            "{:?}",
            loaded.diagnostics
        );
        // 已删 agent 的 sound 键走既有的未知键诊断，用户看得到该清理什么。
        for key in ["droid", "cursor", "github_copilot"] {
            assert!(
                loaded.diagnostics.iter().any(|diagnostic| {
                    diagnostic == &format!("unknown config key ui.sound.agents.{key}; ignoring key")
                }),
                "{key}: {:?}",
                loaded.diagnostics
            );
        }
        let ui = &loaded.config.ui;
        assert!(!ui.mouse_capture, "同段其余设置必须照常生效");
        assert_eq!(
            ui.sound.agents.claude,
            crate::config::sound::AgentSoundSetting::Off
        );
        assert_eq!(
            ui.sidebar.agents.rows_by_agent.keys().collect::<Vec<_>>(),
            vec!["claude"]
        );
    }

    #[test]
    fn startup_config_ignores_retired_agent_keys_without_falling_back_to_defaults() {
        let loaded = load_startup_config("retired-agent-keys", RETIRED_AGENT_KEYS_CONFIG);

        assert_retired_agent_keys_were_only_ignored(&loaded);
    }

    #[test]
    fn live_reload_ignores_retired_agent_keys_without_rejecting_the_ui_section() {
        let loaded = load_live_config_from_str(RETIRED_AGENT_KEYS_CONFIG).unwrap();

        assert_retired_agent_keys_were_only_ignored(&loaded);
    }

    #[test]
    fn startup_config_accepts_legacy_agent_panel_scope_without_warning() {
        let loaded = load_startup_config(
            "legacy-agent-panel-scope",
            "[ui]\nagent_panel_scope = \"all\"\n",
        );

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn startup_config_accepts_legacy_toggle_usage_dashboard_without_warning() {
        let loaded = load_startup_config(
            "legacy-toggle-usage",
            "[keys]\ntoggle_usage_dashboard = \"prefix+a\"\nzoom = \"prefix+shift+z\"\n",
        );

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert!(
            loaded
                .config
                .keybinds()
                .zoom
                .bindings
                .iter()
                .any(|binding| binding.label == "prefix+shift+z"),
            "残留键不让整份配置回退默认"
        );
    }

    #[test]
    fn startup_config_load_warns_about_unknown_top_level_sections() {
        let loaded = load_startup_config(
            "unknown-section",
            r#"
[[plugin]]
id = "example"

[ui.toast]
delivery = "system"
"#,
        );

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [[plugin]]; ignoring section"]
        );
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::System
        );
    }

    #[test]
    fn remove_keybinding_config_sections_removes_keys_tables_only() {
        let content = r#"onboarding = false

[theme]
name = "catppuccin"

[keys]
prefix = "ctrl+a"
new_tab = "c"

[[keys.command]]
key = "g"
command = "lazygit"

[keys.indexed]
tabs = "ctrl"

[ui]
mouse_capture = false
"#;

        let (updated, removed) = remove_keybinding_config_sections(content);

        assert!(removed);
        assert!(updated.contains("onboarding = false"));
        assert!(updated.contains("[theme]\nname = \"catppuccin\""));
        assert!(updated.contains("[ui]\nmouse_capture = false"));
        assert!(!updated.contains("[keys]"));
        assert!(!updated.contains("[[keys.command]]"));
        assert!(!updated.contains("[keys.indexed]"));
        assert!(toml::from_str::<toml::Value>(&updated).is_ok());
    }

    #[test]
    fn remove_keybinding_config_sections_reports_noop_without_keys() {
        let content = "[ui]\nmouse_capture = true\n";
        let (updated, removed) = remove_keybinding_config_sections(content);
        assert!(!removed);
        assert_eq!(updated, content);
    }

    #[test]
    fn normalize_utf8_bom_removes_a_leading_bom() {
        let content = "\u{feff}onboarding = false\n[terminal]\n";
        assert_eq!(
            normalize_utf8_bom(content),
            "onboarding = false\n[terminal]\n"
        );
    }

    #[test]
    fn normalize_utf8_bom_recovers_from_a_displaced_mid_file_bom() {
        let content = "onboarding = false\n\u{feff}[terminal]\ndefault_shell = \"pwsh.exe\"\n";
        let normalized = normalize_utf8_bom(content);
        assert_eq!(
            normalized,
            "onboarding = false\n[terminal]\ndefault_shell = \"pwsh.exe\"\n"
        );
        assert!(normalized.parse::<toml::Value>().is_ok());
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_basic_strings() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\n";
        assert!(content.parse::<toml::Value>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_literal_strings() {
        let content = "[theme]\nname = '''\nfirst\n\u{feff}second\n'''\n";
        assert!(content.parse::<toml::Value>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_string_boms_despite_other_errors() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\nbroken = \n";
        assert!(content.parse::<toml::Value>().is_err());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn config_load_recovers_from_a_mid_file_bom() {
        let loaded = load_startup_config(
            "mid-file-bom",
            b"onboarding = false\n\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        );

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(loaded.config.terminal.default_shell, "pwsh.exe");
    }
}
