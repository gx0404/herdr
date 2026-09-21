use super::*;

/// Runs the thin client and enters the main event loop.
pub fn run_client() -> io::Result<()> {
    run_client_with_mode(None, None, "connecting to server", None)
}

/// CFG-01：调用方已经加载过配置时直接复用，避免启动路径二次解析 config.toml。
pub fn run_client_with_startup_config(
    startup_config: crate::config::LoadedConfig,
) -> io::Result<()> {
    run_client_with_mode(None, None, "connecting to server", Some(startup_config))
}

#[cfg(unix)]
pub fn run_terminal_attach(terminal_id: String, takeover: bool) -> io::Result<()> {
    run_client_with_mode(
        Some((terminal_id, takeover)),
        Some(AttachEscapeState::default()),
        "attaching to terminal",
        None,
    )
}

#[cfg(windows)]
pub fn run_terminal_attach(_terminal_id: String, _takeover: bool) -> io::Result<()> {
    debug_assert!(!crate::platform::capabilities().direct_terminal_attach);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "direct terminal attach is not supported on Windows yet",
    ))
}
