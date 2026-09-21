use std::io;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

use portable_pty::CommandBuilder;

pub(crate) const HERDR_PANE_ID_ENV_VAR: &str = "HERDR_PANE_ID";
pub(crate) const HERDR_TAB_ID_ENV_VAR: &str = "HERDR_TAB_ID";
pub(crate) const HERDR_WORKSPACE_ID_ENV_VAR: &str = "HERDR_WORKSPACE_ID";

pub(crate) const PI_CODING_AGENT_DIR_ENV_VAR: &str = "PI_CODING_AGENT_DIR";
pub(crate) const CLAUDE_CONFIG_DIR_ENV_VAR: &str = "CLAUDE_CONFIG_DIR";
pub(crate) const CODEX_HOME_ENV_VAR: &str = "CODEX_HOME";
pub(crate) const KIMI_CODE_HOME_ENV_VAR: &str = "KIMI_CODE_HOME";

pub(crate) fn apply_pane_base_env(cmd: &mut CommandBuilder) {
    cmd.env(crate::api::SOCKET_PATH_ENV_VAR, crate::api::socket_path());
    if let Ok(executable) = crate::platform::launch_executable() {
        cmd.env("HERDR_BIN_PATH", executable);
    }
}

pub(crate) fn pi_extension_dir() -> io::Result<PathBuf> {
    Ok(
        config_dir_from_env_or_home(PI_CODING_AGENT_DIR_ENV_VAR, &[".pi", "agent"])?
            .join("extensions"),
    )
}

pub(crate) fn claude_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CLAUDE_CONFIG_DIR_ENV_VAR, &[".claude"])
}

/// Claude Code 的主状态文件（`oauthAccount`、目录信任、启动计数等）：设置
/// `CLAUDE_CONFIG_DIR` 时位于该目录内，否则是主目录下的 `~/.claude.json`。
pub(crate) fn claude_state_file() -> io::Result<PathBuf> {
    if let Some(value) =
        std::env::var_os(CLAUDE_CONFIG_DIR_ENV_VAR).filter(|value| !value.is_empty())
    {
        return expand_tilde_path(PathBuf::from(value)).map(|dir| dir.join(".claude.json"));
    }
    Ok(home_dir()?.join(".claude.json"))
}

pub(crate) fn codex_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CODEX_HOME_ENV_VAR, &[".codex"])
}

pub(crate) fn kimi_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(KIMI_CODE_HOME_ENV_VAR, &[".kimi-code"])
}

pub(crate) fn config_dir_from_env_or_home(
    env_var: &str,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    let mut path = home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

pub(crate) fn expand_tilde_path(path: PathBuf) -> io::Result<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path);
    };

    if raw == "~" {
        return home_dir();
    }

    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| raw.strip_prefix('~'))
    {
        return Ok(home_dir()?.join(rest));
    }

    Ok(path)
}

pub(crate) fn opencode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/opencode"))
}

/// OpenCode 的数据目录（`auth.json` 等登录态）：`XDG_DATA_HOME` 覆盖，否则
/// `~/.local/share/opencode`。与 `opencode_state_dir` 同一范式。
pub(crate) fn opencode_data_dir() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("opencode"));
    }

    Ok(home_dir()?.join(".local/share/opencode"))
}

pub(crate) fn opencode_state_dir() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("opencode"));
    }

    Ok(home_dir()?.join(".local/state/opencode"))
}

pub(crate) fn home_dir() -> io::Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }

    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (
            std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty()),
            std::env::var_os("HOMEPATH").filter(|value| !value.is_empty()),
        ) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Ok(home);
        }
    }

    Err(io::Error::other(
        "home directory is not set; cannot locate home directory",
    ))
}

#[cfg(test)]
pub(crate) struct IntegrationEnvLock {
    _guard: MutexGuard<'static, ()>,
    #[cfg(windows)]
    appdata: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl Drop for IntegrationEnvLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(appdata) = self.appdata.take() {
            std::env::set_var("APPDATA", appdata);
        } else {
            std::env::remove_var("APPDATA");
        }
    }
}

#[cfg(test)]
pub(crate) fn integration_env_lock() -> IntegrationEnvLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    IntegrationEnvLock {
        _guard: guard,
        #[cfg(windows)]
        appdata: std::env::var_os("APPDATA"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_state_dir_defaults_to_local_state() {
        let _lock = integration_env_lock();
        let original = std::env::var_os("XDG_STATE_HOME");
        std::env::remove_var("XDG_STATE_HOME");
        let expected = home_dir().unwrap().join(".local/state/opencode");
        assert_eq!(opencode_state_dir().unwrap(), expected);
        match original {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }

    #[test]
    fn opencode_data_dir_expands_tilde_in_xdg_data_home() {
        let _lock = integration_env_lock();
        let original = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", "~/data");
        assert_eq!(
            opencode_data_dir().unwrap(),
            home_dir().unwrap().join("data").join("opencode")
        );
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(
            opencode_data_dir().unwrap(),
            home_dir().unwrap().join(".local/share/opencode")
        );
        match original {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    fn claude_state_file_follows_the_config_dir_override() {
        let _lock = integration_env_lock();
        let original = std::env::var_os(CLAUDE_CONFIG_DIR_ENV_VAR);
        std::env::set_var(CLAUDE_CONFIG_DIR_ENV_VAR, "~/profiles/work");
        assert_eq!(
            claude_state_file().unwrap(),
            home_dir()
                .unwrap()
                .join("profiles/work")
                .join(".claude.json")
        );
        std::env::remove_var(CLAUDE_CONFIG_DIR_ENV_VAR);
        assert_eq!(
            claude_state_file().unwrap(),
            home_dir().unwrap().join(".claude.json")
        );
        match original {
            Some(value) => std::env::set_var(CLAUDE_CONFIG_DIR_ENV_VAR, value),
            None => std::env::remove_var(CLAUDE_CONFIG_DIR_ENV_VAR),
        }
    }

    #[test]
    fn opencode_state_dir_honors_xdg_state_home() {
        let _lock = integration_env_lock();
        let original = std::env::var_os("XDG_STATE_HOME");
        let xdg = std::env::temp_dir().join("herdr-xdg-state");
        std::env::set_var("XDG_STATE_HOME", &xdg);
        assert_eq!(opencode_state_dir().unwrap(), xdg.join("opencode"));
        match original {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
}
