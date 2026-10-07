use super::*;

/// 进程退出前等待 pane 终止阶梯收尾的上限：一轮阶梯（reaper 把新请求并入同一个轮询
/// 循环，不会排成多轮）再留同样长的余量给取件延迟与调度抖动。
fn pane_shutdown_drain_timeout() -> Duration {
    crate::pane::PANE_SHUTDOWN_LADDER_WORST_CASE * 2
}

/// 退出前等 pane 终止阶梯收尾；超时说明 pane 进程可能被留成孤儿，必须记进退出日志而
/// 不是静默继续（`rt.shutdown_timeout` 之后进程就没机会再发 SIGKILL 了）。
fn drain_pane_shutdowns_before_exit() {
    if !crate::pane::drain_pending_pane_shutdowns(pane_shutdown_drain_timeout()) {
        tracing::warn!(
            "pane termination exceeded its drain budget; waiting for retained installation consumers"
        );
    }
    crate::terminal::TerminalRuntime::wait_for_retained_shutdown_resources();
}

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server() -> io::Result<()> {
    crate::platform::ignore_server_hangup();
    let args: Vec<String> = std::env::args().collect();
    let handoff_import = args.get(2).map(String::as_str) == Some("--handoff-import");
    let process_context = crate::platform::prepare_server_process(handoff_import);
    init_logging();
    match process_context {
        Ok(true) => info!("server using persistent user service context"),
        Ok(false) => {}
        Err(err) => {
            warn!(%err, "could not select persistent user service context; retaining inherited context")
        }
    }
    crate::platform::raise_server_nofile_limit();
    // 尽早表明 daemon 身份：拉起本进程的客户端若在某个 pane 里，关那个 pane 时不能连本进程
    // 及其整个会话一起终止。
    crate::platform::announce_detached_server_daemon();

    if handoff_import {
        let socket_path = args
            .get(3)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff socket"))?;
        let token = args
            .get(4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff token"))?;
        return run_handoff_import_server(&socket_path, token);
    }

    let loaded_config = config::Config::load();
    #[cfg(windows)]
    if loaded_config.config.server.allow_unelevated_clients {
        crate::platform::allow_unelevated_clients();
    }
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let server_stop = crate::server::shutdown::ServerStop::default();

    // Start the JSON API socket server.
    let _api_server = match api::start_server_with_stop_control(
        api_tx.clone(),
        event_hub.clone(),
        server_stop.clone(),
    ) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("error: herdr server is already running");
            eprintln!("api socket: {}", api::socket_path().display());
            std::process::exit(1);
        }
        Err(err) => return Err(err),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        // Create the App (with AppState, event channels, etc.).
        let app = app::App::try_new(
            &loaded_config.config,
            app::AppPolicy::PRODUCTION,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub,
        )?;
        let startup_cwd = take_startup_cwd();

        // Open the client socket before the startup workspace spawns its first pane, so an
        // auto-started client can connect meanwhile (on Windows the accept thread also
        // completes its handshake). The event loop has not run yet, so that client cannot
        // seed a default workspace first. On Unix the event loop accepts connections, so
        // the client's 5 s wait for the Welcome (`LOCAL_HANDSHAKE_READ_TIMEOUT`) now also
        // covers seeding the startup workspace.
        let mut server = match HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(_api_server),
            server_stop,
        ) {
            Ok(server) => server,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("error: herdr server is already running");
                eprintln!("client socket: {}", client_socket_path().display());
                std::process::exit(1);
            }
            Err(err) => return Err(err),
        };
        seed_startup_workspace_if_empty(&mut server.app, startup_cwd);

        info!(
            api_socket = %api::socket_path().display(),
            client_socket = %client_socket_path().display(),
            "herdr server started"
        );
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.app.run_plugin_startup_hooks();

        server.run().await
    });

    // 终止阶梯跑在 reaper 线程上（HSR-01）：退出前等它收尾，否则 pane 进程会被留成孤儿。
    drain_pane_shutdowns_before_exit();
    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

/// Must run before `HeadlessServer::run`: clients handled by the event loop seed a default
/// workspace (`App::ensure_default_workspace`) that would win over the startup directory.
pub(super) fn seed_startup_workspace_if_empty(app: &mut app::App, cwd: Option<PathBuf>) {
    let Some(cwd) = cwd else {
        return;
    };

    if !app.state.workspaces.is_empty() {
        info!(
            cwd = %cwd.display(),
            "restored session already has workspaces; ignoring startup cwd"
        );
        return;
    }

    match app.create_workspace_with_options(cwd.clone(), true) {
        Ok(_) => {
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), err = %err, "failed to create startup workspace");
            app.state.mode = app::Mode::Navigate;
        }
    }
}

fn take_startup_cwd() -> Option<PathBuf> {
    let cwd = std::env::var_os(crate::server::autodetect::STARTUP_CWD_ENV_VAR)?;
    std::env::remove_var(crate::server::autodetect::STARTUP_CWD_ENV_VAR);
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

#[cfg(unix)]
fn run_handoff_import_server(socket_path: &Path, token: &str) -> io::Result<()> {
    let loaded_config = config::Config::load();
    let mut received = crate::server::handoff::receive(socket_path, token)?;
    crate::server::handoff::log_import_result(received.manifest.panes.len());

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let server_stop = crate::server::shutdown::ServerStop::default();

    let mut imports = HashMap::new();
    for (pane, fd) in received.manifest.panes.into_iter().zip(received.fds) {
        let pane_id = pane.pane_id;
        imports.insert(
            pane_id,
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: fd,
                state: pane,
            },
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        let app = app::App::new_from_handoff(
            &loaded_config.config,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub.clone(),
            &received.manifest.snapshot,
            &mut imports,
        )?;
        crate::server::handoff::report_restored(&mut received.stream)?;
        if std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("after_restored") {
            return Err(io::Error::other(
                "test handoff import failure after restored",
            ));
        }
        wait_for_old_public_sockets_to_close(Duration::from_secs(5))?;

        let api_server = api::start_server_with_stop_control(
            api_tx.clone(),
            event_hub.clone(),
            server_stop.clone(),
        )?;
        let mut server = HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(api_server),
            server_stop,
        )?;
        // Carried across before any client attaches, so the first title sent is
        // the override rather than the configured one it replaced.
        server.api_window_title = received.manifest.api_window_title.take();
        crate::server::handoff::report_ready(&mut received.stream)?;
        crate::server::handoff::wait_committed(&mut received.stream)?;
        server.app.assume_handoff_ownership();
        server.app.unpause_handoff_readers();
        server.pending_handoff_repaint_nudge = true;
        if let Err(err) = crate::server::handoff::report_owned(&mut received.stream) {
            warn!(err = %err, "failed to report handoff ownership; continuing as owner");
        }
        info!("handoff import server started");
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.app.run_plugin_startup_hooks();
        server.run().await
    });

    // 终止阶梯跑在 reaper 线程上（HSR-01）：退出前等它收尾，否则 pane 进程会被留成孤儿。
    drain_pane_shutdowns_before_exit();
    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

#[cfg(not(unix))]
fn run_handoff_import_server(_socket_path: &Path, _token: &str) -> io::Result<()> {
    Err(io::Error::other("live handoff is only supported on Unix"))
}

fn print_ready_message(api_socket: &Path, client_socket: &Path) {
    let message = format!(
        "herdr server running; you can use any herdr CLI command in another terminal.\n\
         api socket: {}\n\
         client socket: {}\n\
         logs: {}\n\
         did you mean to open the Herdr TUI? run `herdr`; you do not need `herdr server`.\n",
        api_socket.display(),
        client_socket.display(),
        crate::session::data_dir()
            .join("herdr-server.log")
            .display()
    );
    // The launching terminal may already be gone; the server keeps running.
    let _ = io::Write::write_all(&mut io::stderr(), message.as_bytes());
}

/// Initialize logging for the server process.
fn init_logging() {
    crate::logging::init_file_logging("herdr-server.log");
}
