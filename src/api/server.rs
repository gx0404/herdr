use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use tracing::{debug, error, info, warn};

#[cfg(all(test, unix))]
use std::fs;

use crate::api::schema::{
    ErrorBody, ErrorResponse, Method, Request, ResponseResult, ServerCapabilities, SuccessResponse,
};
use crate::api::subscriptions::{poll_subscriptions_round, ActiveSubscription};
use crate::api::wait::{prompt_agent, wait_for_agent, wait_for_event, wait_for_output};
use crate::api::{request_changes_ui, socket_path, ApiRequestMessage, ApiRequestSender, EventHub};
use crate::ipc::{
    bind_local_listener, is_connection_closed_error, local_stream_peer_closed,
    poll_local_stream_read, remove_socket_file_if_owned, set_local_stream_polling,
    socket_file_identity, LocalStream, LocalStreamRead, SocketFileIdentity,
};

#[cfg(test)]
mod subscription_socket_tests;

const SOCKET_PERMISSION_MODE: u32 = 0o600;
pub(super) const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const APP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_INITIAL_REQUEST_BYTES: usize = 1024 * 1024;

pub struct ServerHandle {
    _thread: std::thread::JoinHandle<()>,
    path: PathBuf,
    identity: SocketFileIdentity,
    running: Arc<AtomicBool>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);

        if let Err(err) = self.remove_socket_file_if_owned() {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %self.path.display(), err = %err, "failed to remove api socket on shutdown");
            }
        }
    }
}

impl ServerHandle {
    pub(crate) fn remove_socket_file_if_owned(&self) -> std::io::Result<()> {
        remove_socket_file_if_owned(&self.path, &self.identity)
    }
}

pub(crate) fn start_server_with_stop_control(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    server_stop: Arc<AtomicBool>,
) -> std::io::Result<ServerHandle> {
    start_server_inner(api_tx, event_hub, default_capabilities(), Some(server_stop))
}

fn default_capabilities() -> Option<ServerCapabilities> {
    Some(ServerCapabilities {
        live_handoff: crate::platform::capabilities().live_handoff,
        detached_server_daemon: crate::platform::current_process_is_detached_server_daemon(),
        endpoint_protocol_generation: Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION),
        surface_interest: true,
        health_check: true,
        ssh_agent_registration: false,
    })
}

fn start_server_inner(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    mut capabilities: Option<ServerCapabilities>,
    server_stop: Option<Arc<AtomicBool>>,
) -> std::io::Result<ServerHandle> {
    let path = socket_path();
    prepare_socket_path(&path)?;

    let listener = bind_local_listener(&path)?;
    restrict_socket_permissions(&path)?;
    let identity = socket_file_identity(&path)?;
    info!(path = %path.display(), "api server listening");

    #[cfg(unix)]
    let ssh_agents = match crate::platform::ssh_agent::SshAgentRegistry::new(
        crate::platform::ssh_agent::socket_path(),
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
    ) {
        Ok(registry) => Some(registry),
        Err(error) => {
            warn!(%error, "SSH agent refresh unavailable; retaining inherited pane environment");
            None
        }
    };

    if let Some(capabilities) = capabilities.as_mut() {
        capabilities.ssh_agent_registration = {
            #[cfg(unix)]
            {
                ssh_agents.is_some()
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
    }

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let api_tx = api_tx.clone();
                    let event_hub = event_hub.clone();
                    let capabilities = capabilities.clone();
                    let server_stop = server_stop.clone();
                    let connection_running = Arc::clone(&listener_running);
                    #[cfg(unix)]
                    let ssh_agents = ssh_agents.clone();
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection_with_stop(
                            stream,
                            &api_tx,
                            &event_hub,
                            &connection_running,
                            capabilities,
                            server_stop.as_ref(),
                            #[cfg(unix)]
                            ssh_agents.as_ref(),
                        ) {
                            warn!(err = %err, "api connection failed");
                        }
                    });
                }
                Err(err) => {
                    error!(err = %err, "api listener accept failed");
                    break;
                }
            }
        }
        debug!("api server thread exiting");
    });

    Ok(ServerHandle {
        _thread: thread,
        path,
        identity,
        running,
    })
}

fn retired_pane_graphics_method_error(line: &str, id: &str) -> Option<ErrorResponse> {
    #[derive(serde::Deserialize)]
    struct RequestMethod {
        method: String,
    }

    let envelope = serde_json::from_str::<RequestMethod>(line).ok()?;
    let method = envelope.method.as_str();
    if !matches!(
        method,
        "pane.graphics.info" | "pane.graphics.set" | "pane.graphics.clear" | "pane.graphics.stream"
    ) {
        return None;
    }

    Some(ErrorResponse {
        id: id.into(),
        error: ErrorBody {
            code: "unknown_method".into(),
            message: format!("unknown method: {method}"),
        },
    })
}

fn prepare_socket_path(path: &Path) -> std::io::Result<()> {
    crate::ipc::prepare_socket_path(path, |path| {
        format!(
            "herdr is already running (socket busy at {})",
            path.display()
        )
    })
}

fn restrict_socket_permissions(path: &Path) -> std::io::Result<()> {
    crate::ipc::restrict_socket_permissions(path, SOCKET_PERMISSION_MODE)
}

#[cfg(test)]
fn handle_connection(
    stream: LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
) -> std::io::Result<()> {
    handle_connection_with_stop(
        stream,
        api_tx,
        event_hub,
        running,
        capabilities,
        None,
        #[cfg(unix)]
        None,
    )
}

fn handle_connection_with_stop(
    mut stream: LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    #[cfg(unix)] ssh_agents: Option<&crate::platform::ssh_agent::SshAgentRegistry>,
) -> std::io::Result<()> {
    if let Err(err) = stream.set_send_timeout(Some(STREAM_WRITE_TIMEOUT)) {
        debug!(err = %err, "api connection write timeout unavailable");
    }

    let Some(line) = read_initial_request_line(&mut stream)? else {
        return Ok(());
    };

    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    let request = match serde_json::from_str::<Request>(line) {
        Ok(request) => request,
        Err(request_error) => {
            // Recover correlation without relaxing typed request validation or accepting
            // ambiguous duplicate IDs. Invalid JSON and non-string IDs stay uncorrelated.
            #[derive(serde::Deserialize)]
            struct RequestId {
                id: String,
            }
            let id = if line.starts_with('{') {
                serde_json::from_str::<RequestId>(line)
                    .map(|request| request.id)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let response =
                retired_pane_graphics_method_error(line, &id).unwrap_or_else(|| ErrorResponse {
                    id,
                    error: ErrorBody {
                        code: "invalid_request".into(),
                        message: format!("invalid request: {request_error}"),
                    },
                });
            write_json_line_allow_disconnect(&mut stream, &response)?;
            return Ok(());
        }
    };

    let request_id = request.id.clone();
    let method = api_method_name(&request.method);
    let changes_ui = request_changes_ui(&request);
    crate::logging::api_request_started(&request_id, method, changes_ui);

    match request.method {
        Method::SystemMetricsSubscribe(params) => stream_observations(
            stream,
            Request {
                id: request_id,
                method: Method::SystemMetricsSubscribe(params),
            },
            api_tx,
            running,
        ),
        Method::AccountUsageSubscribe(params) => stream_observations(
            stream,
            Request {
                id: request_id,
                method: Method::AccountUsageSubscribe(params),
            },
            api_tx,
            running,
        ),
        #[cfg(unix)]
        Method::ServerSshAgentRegister(params) => {
            let lease = ssh_agents
                .ok_or_else(|| io::Error::other("SSH agent registration is unavailable"))
                .and_then(|registry| registry.register(PathBuf::from(params.socket_path)));
            let lease = match lease {
                Ok(lease) => lease,
                Err(error) => {
                    return write_text_line_allow_disconnect(
                        &mut stream,
                        &error_response_json(
                            request_id,
                            if error.kind() == io::ErrorKind::InvalidInput {
                                "invalid_ssh_agent"
                            } else {
                                "ssh_agent_unavailable"
                            },
                            error.to_string(),
                        ),
                    )
                }
            };
            write_json_line(
                &mut stream,
                &SuccessResponse {
                    id: request_id,
                    result: ResponseResult::Ok {},
                },
            )?;
            set_local_stream_polling(&mut stream, true)?;
            let mut byte = [0];
            while running.load(Ordering::Relaxed) {
                match poll_local_stream_read(&mut stream, &mut byte)? {
                    LocalStreamRead::Pending => {
                        // SSH can unlink an inherited socket after its bridge's lease closes.
                        lease.refresh()?;
                        std::thread::sleep(CONNECTION_POLL_INTERVAL);
                    }
                    _ => break,
                }
            }
            Ok(())
        }
        Method::EventsSubscribe(params) => {
            let result = stream_subscriptions(
                stream,
                request_id.clone(),
                params,
                api_tx,
                event_hub,
                running,
            );
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    "stream_closed",
                    None,
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
        Method::EventsWait(params) => {
            let response = wait_for_event(
                request_id.clone(),
                params,
                &mut stream,
                api_tx,
                event_hub,
                running,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method, changes_ui)
        }
        Method::AgentPrompt(params) => {
            let response = prompt_agent(
                request_id.clone(),
                params,
                &mut stream,
                api_tx,
                event_hub,
                running,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method, changes_ui)
        }
        Method::AgentWait(params) => {
            let response = wait_for_agent(
                request_id.clone(),
                params,
                &mut stream,
                api_tx,
                event_hub,
                running,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method, changes_ui)
        }
        Method::PaneWaitForOutput(params) => {
            let response =
                wait_for_output(request_id.clone(), params, &mut stream, api_tx, running)?;
            finish_wait_response(&mut stream, response, &request_id, method, changes_ui)
        }
        method_body => {
            let (response_write_tx, response_write_rx) = std::sync::mpsc::channel();
            let response = handle_request(
                Request {
                    id: request_id.clone(),
                    method: method_body,
                },
                api_tx,
                capabilities,
                server_stop,
                Some(response_write_rx),
            );
            let result = write_text_line_allow_disconnect(&mut stream, &response);
            let _ = response_write_tx.send(());
            match &result {
                Ok(()) => {
                    let outcome = api_response_details(&response);
                    crate::logging::api_request_completed(
                        &request_id,
                        method,
                        outcome.outcome,
                        outcome.error_code.as_deref(),
                        changes_ui,
                    )
                }
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
    }
}

fn finish_wait_response(
    stream: &mut LocalStream,
    response: Option<String>,
    request_id: &str,
    method: &'static str,
    changes_ui: bool,
) -> std::io::Result<()> {
    let Some(response) = response else {
        crate::logging::api_request_completed(
            request_id,
            method,
            "client_disconnected",
            None,
            changes_ui,
        );
        return Ok(());
    };
    let result = write_text_line_allow_disconnect(stream, &response);
    match &result {
        Ok(()) => {
            let outcome = api_response_details(&response);
            crate::logging::api_request_completed(
                request_id,
                method,
                outcome.outcome,
                outcome.error_code.as_deref(),
                changes_ui,
            )
        }
        Err(err) => crate::logging::api_request_failed(request_id, method, &err.to_string()),
    }
    result
}

fn handle_request(
    request: Request,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
) -> String {
    if matches!(&request.method, Method::Ping(_)) {
        return serde_json::to_string(&SuccessResponse {
            id: request.id,
            result: ResponseResult::Pong {
                version: crate::build_info::version().to_owned(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities,
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
                .to_string()
        });
    }

    if matches!(&request.method, Method::ClientShellSurfaceSet(_)) {
        return error_response_json(
            request.id,
            "connection_local_only",
            "client_shell.surface.set is only available through a client shell endpoint".into(),
        );
    }

    if matches!(&request.method, Method::ServerStop(_)) {
        if let Some(server_stop) = server_stop {
            server_stop.store(true, Ordering::Release);
            return serde_json::to_string(&SuccessResponse {
                id: request.id,
                result: ResponseResult::Ok {},
            })
            .unwrap_or_else(|_| "{}".to_string());
        }
    } else if server_stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
        return error_response_json(
            request.id,
            "server_unavailable",
            "server is shutting down".into(),
        );
    }

    dispatch_to_app(request, api_tx, None, response_write_complete, None)
}

pub(crate) fn api_method_name(method: &Method) -> &'static str {
    match method {
        Method::SystemMetricsGet(_) => "system.metrics.get",
        Method::SystemMetricsSubscribe(_) => "system.metrics.subscribe",
        Method::SystemMetricsUnsubscribe(_) => "system.metrics.unsubscribe",
        Method::SystemProcessList(_) => "system.process.list",
        Method::SystemProcessGet(_) => "system.process.get",
        Method::SystemProcessTerminate(_) => "system.process.terminate",
        Method::AccountUsageProviders(_) => "account.usage.providers",
        Method::AccountUsageGet(_) => "account.usage.get",
        Method::AccountUsageIntegration(_) => "account.usage.integration",
        Method::AccountUsageRefresh(_) => "account.usage.refresh",
        Method::AccountUsageSubscribe(_) => "account.usage.subscribe",
        Method::AccountUsageUnsubscribe(_) => "account.usage.unsubscribe",
        Method::AccountUsageReport(_) => "account.usage.report",
        Method::AccountBindingSet(_) => "account.binding.set",
        Method::ClientViewsSet(_) => "client.views.set",
        Method::Ping(_) => "ping",
        Method::ServerStop(_) => "server.stop",
        Method::ServerLiveHandoff(_) => "server.live_handoff",
        Method::ServerReloadConfig(_) => "server.reload_config",
        Method::ServerSshAgentRegister(_) => "server.ssh_agent.register",
        Method::ServerAgentManifests(_) => "server.agent_manifests",
        Method::ServerReloadAgentManifests(_) => "server.reload_agent_manifests",
        Method::NotificationShow(_) => "notification.show",
        Method::ProductAnnouncementDismiss(_) => "product_announcement.dismiss",
        Method::ReleaseNotesDismiss(_) => "release_notes.dismiss",
        Method::CommandInvoke(_) => "command.invoke",
        Method::ClientWindowTitleSet(_) => "client.window_title.set",
        Method::ClientWindowTitleClear(_) => "client.window_title.clear",
        Method::ClientShellSurfaceSet(_) => "client_shell.surface.set",
        Method::SessionSnapshot(_) => "session.snapshot",
        Method::WorkspaceCreate(_) => "workspace.create",
        Method::WorkspaceList(_) => "workspace.list",
        Method::WorkspaceGet(_) => "workspace.get",
        Method::WorkspaceFocus(_) => "workspace.focus",
        Method::WorkspaceRename(_) => "workspace.rename",
        Method::WorkspaceMove(_) => "workspace.move",
        Method::WorkspaceMoveBlock(_) => "workspace.move_block",
        Method::WorkspaceReportMetadata(_) => "workspace.report_metadata",
        Method::WorkspaceClose(_) => "workspace.close",
        Method::WorktreeList(_) => "worktree.list",
        Method::WorktreeCreate(_) => "worktree.create",
        Method::WorktreeOpen(_) => "worktree.open",
        Method::WorktreeRemove(_) => "worktree.remove",
        Method::TabCreate(_) => "tab.create",
        Method::TabList(_) => "tab.list",
        Method::TabGet(_) => "tab.get",
        Method::TabFocus(_) => "tab.focus",
        Method::TabRename(_) => "tab.rename",
        Method::TabMove(_) => "tab.move",
        Method::TabClose(_) => "tab.close",
        Method::AgentList(_) => "agent.list",
        Method::AgentGet(_) => "agent.get",
        Method::AgentRead(_) => "agent.read",
        Method::AgentExplain(_) => "agent.explain",
        Method::AgentSendKeys(_) => "agent.send_keys",
        Method::AgentRename(_) => "agent.rename",
        Method::AgentViewSet(_) => "agent.view.set",
        Method::AgentViewClear(_) => "agent.view.clear",
        Method::AgentFocus(_) => "agent.focus",
        Method::AgentStart(_) => "agent.start",
        Method::AgentPrompt(_) => "agent.prompt",
        Method::AgentWait(_) => "agent.wait",
        Method::AgentActivityRead(_) => "agent.activity.read",
        Method::AgentExternalList(_) => "agent.external.list",
        Method::PaneSplit(_) => "pane.split",
        Method::PaneSwap(_) => "pane.swap",
        Method::PaneMove(_) => "pane.move",
        Method::PaneZoom(_) => "pane.zoom",
        Method::PaneLayout(_) => "pane.layout",
        Method::PaneProcessInfo(_) => "pane.process_info",
        Method::LayoutExport(_) => "layout.export",
        Method::LayoutApply(_) => "layout.apply",
        Method::LayoutSetSplitRatio(_) => "layout.set_split_ratio",
        Method::PaneNeighbor(_) => "pane.neighbor",
        Method::PaneEdges(_) => "pane.edges",
        Method::PaneFocusDirection(_) => "pane.focus_direction",
        Method::PaneResize(_) => "pane.resize",
        Method::PaneScroll(_) => "pane.scroll",
        Method::PaneClear(_) => "pane.clear",
        Method::PaneEditScrollback(_) => "pane.edit_scrollback",
        Method::PaneTextSnapshotCapture(_) => "pane.text_snapshot.capture",
        Method::PaneTextSnapshotRead(_) => "pane.text_snapshot.read",
        Method::PaneTextSnapshotSelection(_) => "pane.text_snapshot.selection",
        Method::PaneTextSnapshotRetain(_) => "pane.text_snapshot.retain",
        Method::PaneTextSnapshotRelease(_) => "pane.text_snapshot.release",
        Method::PaneSelectionRead(_) => "pane.selection.read",
        Method::PaneCopyMotion(_) => "pane.copy_motion",
        Method::PaneCopySearch(_) => "pane.copy_search",
        Method::PaneList(_) => "pane.list",
        Method::PaneCurrent(_) => "pane.current",
        Method::PaneGet(_) => "pane.get",
        Method::PaneFocus(_) => "pane.focus",
        Method::PaneInputSet(_) => "pane.input.set",
        Method::PaneLinkActivate(_) => "pane.link.activate",
        Method::PaneLinkResolve(_) => "pane.link.resolve",
        Method::PaneRename(_) => "pane.rename",
        Method::PaneSendText(_) => "pane.send_text",
        Method::PaneSendKeys(_) => "pane.send_keys",
        Method::PaneSendInput(_) => "pane.send_input",
        Method::PaneRead(_) => "pane.read",
        Method::PaneReportAgent(_) => "pane.report_agent",
        Method::PaneReportAgentSession(_) => "pane.report_agent_session",
        Method::PaneReportAgentActivity(_) => "pane.report_agent_activity",
        Method::PaneReportMetadata(_) => "pane.report_metadata",
        Method::PaneClearAgentAuthority(_) => "pane.clear_agent_authority",
        Method::PaneReleaseAgent(_) => "pane.release_agent",
        Method::PaneClose(_) => "pane.close",
        Method::PopupClose(_) => "popup.close",
        Method::EventsSubscribe(_) => "events.subscribe",
        Method::EventsWait(_) => "events.wait",
        Method::PaneWaitForOutput(_) => "pane.wait_for_output",
        Method::IntegrationList(_) => "integration.list",
        Method::IntegrationInstall(_) => "integration.install",
        Method::IntegrationUninstall(_) => "integration.uninstall",
        Method::PluginLink(_) => "plugin.link",
        Method::PluginList(_) => "plugin.list",
        Method::PluginUnlink(_) => "plugin.unlink",
        Method::PluginEnable(_) => "plugin.enable",
        Method::PluginDisable(_) => "plugin.disable",
        Method::PluginActionList(_) => "plugin.action.list",
        Method::PluginActionInvoke(_) => "plugin.action.invoke",
        Method::PluginLogList(_) => "plugin.log.list",
        Method::PluginPaneOpen(_) => "plugin.pane.open",
        Method::PluginPaneFocus(_) => "plugin.pane.focus",
        Method::PluginPaneClose(_) => "plugin.pane.close",
    }
}

/// 响应文本的日志结局：`outcome` 是固定枚举，`error_code` 只在错误响应时出现。
struct ApiResponseOutcome {
    outcome: &'static str,
    error_code: Option<String>,
}

fn api_response_details(response: &str) -> ApiResponseOutcome {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return ApiResponseOutcome {
            outcome: "error",
            error_code: None,
        };
    };

    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(|code| code.as_str())
        // 错误码是短标识；截断只防御畸形响应，正常响应不会触及。
        .map(|code| code.chars().take(64).collect::<String>());
    ApiResponseOutcome {
        outcome: match code.as_deref() {
            Some("timeout") => "timeout",
            Some(_) => "error",
            None => "ok",
        },
        error_code: code,
    }
}

// 生产调用方随上游 #4561 删除的 graphics stream 一起消失，只剩测试钉住判定口径。
#[cfg(test)]
fn api_response_outcome(response: &str) -> &'static str {
    api_response_details(response).outcome
}

fn read_initial_request_line(stream: &mut LocalStream) -> std::io::Result<Option<String>> {
    read_initial_request_line_with_timeout(stream, INITIAL_REQUEST_TIMEOUT)
}

fn read_initial_request_line_with_timeout(
    stream: &mut LocalStream,
    timeout: Duration,
) -> std::io::Result<Option<String>> {
    read_initial_request_line_with_limits(stream, timeout, MAX_INITIAL_REQUEST_BYTES)
}

fn read_initial_request_line_with_limits(
    stream: &mut LocalStream,
    timeout: Duration,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    set_local_stream_polling(stream, true)?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];

    let result = loop {
        let read = match poll_local_stream_read(stream, &mut byte) {
            Ok(read) => read,
            Err(err) => break Err(err),
        };
        match read {
            LocalStreamRead::Closed => break Ok(None),
            LocalStreamRead::Data => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    break String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
                }
                if bytes.len() > max_bytes {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "api request line is too large",
                    ));
                }
            }
            LocalStreamRead::Pending => {
                if Instant::now() >= deadline {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out reading api request",
                    ));
                }
                std::thread::sleep(CONNECTION_POLL_INTERVAL);
            }
        }
    };
    set_local_stream_polling(stream, false)?;
    result
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc::{self, Receiver};

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "herdr-api-{name}-{}-{}.sock",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, path)
    }

    fn spawn_connection(
        server: LocalStream,
    ) -> (Receiver<std::io::Result<()>>, std::thread::JoinHandle<()>) {
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
            let result = handle_connection(
                server,
                &api_tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
            );
            done_tx.send(result).unwrap();
        });
        (done_rx, thread)
    }

    #[test]
    fn windows_delayed_partial_initial_request_returns_pong() {
        let (mut client, server, path) = local_stream_pair("delayed-request");
        let (done_rx, server_thread) = spawn_connection(server);

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            done_rx.try_recv().is_err(),
            "idle connected client must not be treated as closed"
        );

        client
            .write_all(br#"{"id":"delayed","method":"ping","params":{}}"#)
            .unwrap();
        client.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            done_rx.try_recv().is_err(),
            "partial request must wait for its newline"
        );
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let mut response = String::new();
        BufReader::new(&mut client)
            .read_line(&mut response)
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "delayed");
        assert_eq!(response["result"]["type"], "pong");

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_disconnected_initial_request_returns_promptly() {
        let (client, server, path) = local_stream_pair("disconnected-request");
        let (done_rx, server_thread) = spawn_connection(server);

        drop(client);

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnected connection handler must finish promptly")
            .unwrap();
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_idle_initial_request_honors_timeout() {
        let (_client, mut server, path) = local_stream_pair("request-timeout");

        let err = read_initial_request_line_with_timeout(&mut server, Duration::from_millis(50))
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_initial_request_enforces_size_limit() {
        let (mut client, mut server, path) = local_stream_pair("request-size-limit");
        client.write_all(b"12345").unwrap();
        client.flush().unwrap();

        let err = read_initial_request_line_with_limits(&mut server, Duration::from_secs(1), 4)
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "api request line is too large");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn windows_initial_request_rejects_invalid_utf8() {
        let (mut client, mut server, path) = local_stream_pair("request-invalid-utf8");
        client.write_all(&[0xff, b'\n']).unwrap();
        client.flush().unwrap();

        let err = read_initial_request_line_with_timeout(&mut server, Duration::from_secs(1))
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(path);
    }
}

fn stream_subscriptions(
    mut stream: LocalStream,
    request_id: String,
    params: crate::api::schema::EventsSubscribeParams,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let event_start_sequence = event_hub.current_sequence();
    // 通知帧（`events.lost`）的 `event` 名不在 `EventKind` 里，只对显式开启的
    // 订阅方下发，严格按 `event` 条目解码每一行的旧客户端不受影响。
    let notices = params.notices;
    let mut subscriptions = Vec::with_capacity(params.subscriptions.len());
    for (index, subscription) in params.subscriptions.into_iter().enumerate() {
        let active = match ActiveSubscription::new(
            subscription,
            &request_id,
            index,
            api_tx,
            event_hub,
            event_start_sequence,
        ) {
            Ok(active) => active,
            Err(mut response) => {
                response.id = request_id;
                if let Err(err) = write_json_line(&mut stream, &response) {
                    if is_connection_closed_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
                return Ok(());
            }
        };
        subscriptions.push(active);
    }

    if let Err(err) = write_json_line(
        &mut stream,
        &SuccessResponse {
            id: request_id.clone(),
            result: ResponseResult::SubscriptionStarted {},
        },
    ) {
        if is_connection_closed_error(&err) {
            return Ok(());
        }
        return Err(err);
    }

    loop {
        if should_stop_connection(&mut stream, running)? {
            return Ok(());
        }

        // 每个轮询间隔只 poll 一轮，每个订阅只 poll 一次。去掉 10 Hz 上限靠的是
        // 单次 poll 就取走本轮全部事件（`ActiveSubscription::poll` 返回 `Vec`），
        // 不是在这里加内层 drain 循环：内层 drain 会无差别重跑连接上的**全部**
        // 订阅，而 `pane.output_matched` / `pane.agent_status_changed` /
        // `pane.scroll_changed` 每次 poll 都是一次对 app/渲染线程的同步往返
        // （`APP_RESPONSE_TIMEOUT`），于是高频事件源会把这些往返按轮数放大——
        // 既加宽乘法路径（连接 × 订阅 × 轮次），又让停机信号被饿死在
        // 单次唤醒内。快照类订阅是边沿触发，多跑一轮也补不回中间态。
        // 每条事件仍是独立的一行 JSON，分帧不变（HSR-03）。
        // 断层且订阅方没开 notices 时（上游 65927cef）：回 `events_lost` 错误、只关
        // 本订阅，不先发本轮残缺的事件；其他连接不受影响。
        let round = match poll_subscriptions_round(&mut subscriptions, api_tx, event_hub, notices) {
            Ok(round) => round,
            Err(error) => {
                write_json_line_allow_disconnect(
                    &mut stream,
                    &ErrorResponse {
                        id: request_id,
                        error,
                    },
                )?;
                return Ok(());
            }
        };
        for event in round {
            if let Err(err) = write_json_line(&mut stream, &event) {
                if is_connection_closed_error(&err) {
                    return Ok(());
                }
                return Err(err);
            }
        }
        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

fn stream_observations(
    mut stream: LocalStream,
    request: Request,
    api_tx: &ApiRequestSender,
    running: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    struct ActiveGuard(Arc<AtomicBool>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let active = ActiveGuard(Arc::new(AtomicBool::new(true)));
    let latest = Arc::new(std::sync::Mutex::new(None::<String>));
    let (sender, receiver) = std::sync::mpsc::channel();
    api_tx
        .send(crate::api::ApiRequestMessage {
            request,
            respond_to: sender,
            response_write_complete: None,
            stream_active: Some(active.0.clone()),
            observation_events: Some(latest.clone()),
        })
        .map_err(|_| std::io::Error::other("观测服务已停止"))?;
    let mut initial = false;
    loop {
        if should_stop_connection(&mut stream, running)? {
            return Ok(());
        }
        if !initial {
            match receiver.recv_timeout(CONNECTION_POLL_INTERVAL) {
                Ok(response) => {
                    write_text_line_allow_disconnect(&mut stream, &response)?;
                    if serde_json::from_str::<serde_json::Value>(&response)
                        .ok()
                        .is_some_and(|v| v.get("error").is_some())
                    {
                        return Ok(());
                    }
                    initial = true;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        let event = latest.lock().ok().and_then(|mut slot| slot.take());
        if let Some(event) = event {
            if let Err(error) = write_text_line(&mut stream, &event) {
                if is_connection_closed_error(&error) {
                    return Ok(());
                }
                return Err(error);
            }
        }
        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

fn write_text_line(stream: &mut LocalStream, value: &str) -> std::io::Result<()> {
    stream.write_all(value.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn write_text_line_allow_disconnect(stream: &mut LocalStream, value: &str) -> std::io::Result<()> {
    match write_text_line(stream, value) {
        Err(err) if is_connection_closed_error(&err) => Ok(()),
        result => result,
    }
}

fn write_json_line<T: serde::Serialize>(
    stream: &mut LocalStream,
    value: &T,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_text_line(stream, &encoded)
}

fn write_json_line_allow_disconnect<T: serde::Serialize>(
    stream: &mut LocalStream,
    value: &T,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_text_line_allow_disconnect(stream, &encoded)
}

pub(super) fn should_stop_connection(
    stream: &mut LocalStream,
    running: &Arc<AtomicBool>,
) -> std::io::Result<bool> {
    if !running.load(Ordering::Relaxed) {
        return Ok(true);
    }

    local_stream_peer_closed(stream)
}

pub(super) fn dispatch_to_app_with_timeout(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
) -> String {
    dispatch_to_app(request, api_tx, timeout, None, None)
}

pub(super) fn dispatch_to_app_with_caller_timeout(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
) -> String {
    dispatch_to_app(
        request,
        api_tx,
        timeout,
        None,
        Some(("timeout", "timed out waiting for agent status")),
    )
}

fn dispatch_to_app(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    timeout_response: Option<(&str, &str)>,
) -> String {
    let request_id = request.id.clone();
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    if let Err(err) = api_tx.send(ApiRequestMessage {
        request,
        respond_to,
        response_write_complete,
        // 观测订阅（stream_observations）自己构造消息并带上这两个字段；上游
        // c411883e 删掉 graphics stream 后，经 dispatch_to_app 的请求都不带。
        stream_active: None,
        observation_events: None,
    }) {
        return error_response_json(
            request_id,
            "server_unavailable",
            format!("failed to dispatch request: {err}"),
        );
    }

    let response = match timeout {
        Some(timeout) => response_rx.recv_timeout(timeout).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for app response after {} ms",
                    timeout.as_millis()
                ),
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "app response channel closed",
            ),
        }),
        None => response_rx
            .recv()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err)),
    };

    match response {
        Ok(response) => response,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::TimedOut {
                if let Some((code, message)) = timeout_response {
                    return error_response_json(request_id, code, message.into());
                }
            }
            error_response_json(
                request_id,
                "server_unavailable",
                format!("request handling failed: {err}"),
            )
        }
    }
}

#[cfg(test)]
#[test]
fn caller_timeout_dispatch_uses_timeout_error() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = dispatch_to_app_with_caller_timeout(
        Request {
            id: "prompt-timeout".into(),
            method: Method::AgentPrompt(crate::api::schema::AgentPromptParams {
                target: "reviewer".into(),
                text: "review this".into(),
                wait: None,
            }),
        },
        &tx,
        Some(Duration::ZERO),
    );
    let error: ErrorResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(error.error.code, "timeout");
}

fn error_response_json(id: String, code: &str, message: String) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"failed to encode error response"}}"#
            .to_string()
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::mpsc;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_test_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    fn read_line(stream: &mut LocalStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, PathBuf) {
        let path = unique_test_path(name);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, path)
    }

    #[test]
    fn ssh_agent_registration_lasts_only_for_the_api_connection() {
        let directory = unique_test_path("agent-lease");
        fs::create_dir(&directory).unwrap();
        let agent = directory.join("upstream");
        let _agent = UnixListener::bind(&agent).unwrap();
        let stable = directory.join("stable");
        let registry =
            crate::platform::ssh_agent::SshAgentRegistry::new(stable.clone(), None).unwrap();
        let (mut client, server, api_path) = local_stream_pair("agent-api");
        let (tx, _rx) = mpsc::unbounded_channel();
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            handle_connection_with_stop(
                server,
                &tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
                None,
                Some(&worker_registry),
            )
            .unwrap();
        });
        write_json_line(
            &mut client,
            &Request {
                id: "agent-lease".into(),
                method: Method::ServerSshAgentRegister(
                    crate::api::schema::ServerSshAgentRegisterParams {
                        socket_path: agent.to_string_lossy().into_owned(),
                    },
                ),
            },
        )
        .unwrap();
        let response: SuccessResponse = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert!(matches!(response.result, ResponseResult::Ok {}));
        assert_eq!(fs::read_link(&stable).unwrap(), agent);
        drop(client);
        worker.join().unwrap();
        assert!(!stable.exists());
        drop(registry);
        fs::remove_file(api_path).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    fn pane_info(
        pane_id: &str,
        agent_status: crate::api::schema::AgentStatus,
    ) -> crate::api::schema::PaneInfo {
        crate::api::schema::PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: "term_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: Some("pi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll: None,
            revision: 0,
        }
    }

    fn spawn_pane_get_responder(
        agent_status: crate::api::schema::AgentStatus,
    ) -> (ApiRequestSender, std::thread::JoinHandle<()>) {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            while let Some(msg) = api_rx.blocking_recv() {
                match msg.request.method {
                    Method::PaneGet(_) => msg
                        .respond_to
                        .send(
                            serde_json::to_string(&SuccessResponse {
                                id: msg.request.id,
                                result: ResponseResult::PaneInfo {
                                    pane: pane_info("pane_1", agent_status),
                                },
                            })
                            .unwrap(),
                        )
                        .unwrap(),
                    Method::EventsWait(_) => msg
                        .respond_to
                        .send(error_response_json(
                            msg.request.id,
                            "unexpected_dispatch",
                            "events.wait should be handled by the api server".into(),
                        ))
                        .unwrap(),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        });
        (api_tx, responder)
    }

    #[test]
    fn socket_path_prefers_explicit_env_override() {
        let _guard = env_lock().lock().unwrap();
        let unique = format!("/tmp/herdr-test-{}.sock", std::process::id());
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &unique);
        assert_eq!(socket_path(), PathBuf::from(&unique));
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn socket_path_defaults_to_config_dir_even_when_xdg_runtime_dir_is_set() {
        let _guard = env_lock().lock().unwrap();
        let config_home = unique_test_path("socket-default-config-home");
        let runtime_dir = unique_test_path("socket-default-runtime");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);

        let expected = config_home
            .join(crate::config::app_dir_name())
            .join("herdr.sock");
        assert_eq!(socket_path(), expected);

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    #[test]
    fn socket_path_uses_named_session_dir() {
        let _guard = env_lock().lock().unwrap();
        let config_home = unique_test_path("socket-named-config-home");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var(crate::session::SESSION_ENV_VAR, "work");
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        let expected = config_home
            .join(crate::config::app_dir_name())
            .join("sessions")
            .join("work")
            .join("herdr.sock");
        assert_eq!(socket_path(), expected);

        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn restrict_socket_permissions_sets_user_only_mode() {
        let dir = unique_test_path("socket-perms");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        restrict_socket_permissions(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_PERMISSION_MODE);

        drop(_listener);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_response_outcome_uses_top_level_error_shape() {
        let ok_with_error_text = r#"{"id":"req","result":{"read":{"text":"user said \"error\": \"timeout\"","revision":1}}}"#;
        let outcome = api_response_details(ok_with_error_text);
        assert_eq!(outcome.outcome, "ok");
        assert_eq!(outcome.error_code, None);

        let timeout = r#"{"id":"req","error":{"code":"timeout","message":"timed out waiting for output match"}}"#;
        let outcome = api_response_details(timeout);
        assert_eq!(outcome.outcome, "timeout");
        assert_eq!(outcome.error_code.as_deref(), Some("timeout"));

        let generic_error =
            r#"{"id":"req","error":{"code":"server_unavailable","message":"boom"}}"#;
        let outcome = api_response_details(generic_error);
        assert_eq!(outcome.outcome, "error");
        assert_eq!(outcome.error_code.as_deref(), Some("server_unavailable"));

        let unparsable = "not json";
        let outcome = api_response_details(unparsable);
        assert_eq!(outcome.outcome, "error");
        assert_eq!(outcome.error_code, None);

        assert_eq!(api_response_outcome(ok_with_error_text), "ok");
        assert_eq!(api_response_outcome(timeout), "timeout");
        assert_eq!(api_response_outcome(generic_error), "error");
    }

    #[test]
    fn api_response_outcome_keeps_the_error_code_but_never_the_message() {
        let rejected = r#"{"id":"usage-report","error":{"code":"usage_binding_required","message":"请先在账号用量页面为此窗格绑定对应厂商账号"}}"#;
        let outcome = api_response_details(rejected);
        assert_eq!(outcome.outcome, "error");
        assert_eq!(
            outcome.error_code.as_deref(),
            Some("usage_binding_required")
        );
    }

    #[test]
    fn removed_pane_graphics_methods_return_unknown_method_without_stream_upgrade() {
        for method in [
            "pane.graphics.info",
            "pane.graphics.set",
            "pane.graphics.clear",
            "pane.graphics.stream",
        ] {
            let (mut client, server, _path) = local_stream_pair("removed-pane-graphics");
            let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
            writeln!(
                client,
                "{{\"id\":\"removed\",\"method\":\"{method}\",\"params\":{{}}}}"
            )
            .unwrap();
            client.flush().unwrap();

            handle_connection(
                server,
                &api_tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
            )
            .unwrap();

            let response = read_line(&mut client);
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], "removed", "{method}");
            assert_eq!(response["error"]["code"], "unknown_method", "{method}");
            assert!(response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(method)));
            assert!(response.get("result").is_none(), "{method}");
            assert!(api_rx.try_recv().is_err(), "{method} reached the app");
        }
    }

    #[test]
    fn unrelated_unknown_method_retains_standard_invalid_request_response() {
        let (mut client, server, _path) = local_stream_pair("unknown-api-request");
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        client
            .write_all(b"{\"id\":\"unknown\",\"method\":\"nope\",\"params\":{}}\n")
            .unwrap();
        client.flush().unwrap();

        handle_connection(
            server,
            &api_tx,
            &EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
        )
        .unwrap();

        let response = read_line(&mut client);
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "unknown");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(api_rx.try_recv().is_err());
    }

    #[test]
    fn ordinary_api_request_still_uses_normal_connection_path() {
        let (mut client, server, _path) = local_stream_pair("ordinary-api-request");
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        client
            .write_all(b"{\"id\":\"ordinary\",\"method\":\"ping\",\"params\":{}}\n")
            .unwrap();
        client.flush().unwrap();

        handle_connection(
            server,
            &api_tx,
            &EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
        )
        .unwrap();

        let response = read_line(&mut client);
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "ordinary");
        assert_eq!(response["result"]["type"], "pong");
    }

    #[test]
    fn ping_request_returns_pong() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let response = handle_request(
            Request {
                id: "req_1".into(),
                method: Method::Ping(crate::api::schema::PingParams::default()),
            },
            &tx,
            Some(ServerCapabilities {
                live_handoff: true,
                detached_server_daemon: true,
                endpoint_protocol_generation: Some(
                    crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION,
                ),
                surface_interest: true,
                health_check: true,
                ssh_agent_registration: false,
            }),
            None,
            None,
        );

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.id, "req_1");
        assert!(matches!(parsed.result, ResponseResult::Pong { .. }));
    }

    #[test]
    fn server_stop_control_bypasses_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let response = handle_request(
            Request {
                id: "priority_stop".into(),
                method: Method::ServerStop(crate::api::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
            None,
        );

        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], "priority_stop");
        assert_eq!(response["result"]["type"], "ok");
        assert!(stop.load(Ordering::Acquire));

        let rejected = handle_request(
            Request {
                id: "after_stop".into(),
                method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
            None,
        );
        let rejected: serde_json::Value = serde_json::from_str(&rejected).unwrap();
        assert_eq!(rejected["error"]["code"], "server_unavailable");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn request_dispatches_to_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = Request {
            id: "req_2".into(),
            method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
        };

        let request_for_thread = request.clone();
        let thread =
            std::thread::spawn(move || handle_request(request_for_thread, &tx, None, None, None));

        let msg = rx.blocking_recv().unwrap();
        assert_eq!(msg.request.id, "req_2");
        msg.respond_to
            .send(
                serde_json::to_string(&SuccessResponse {
                    id: "req_2".into(),
                    result: ResponseResult::Ok {},
                })
                .unwrap(),
            )
            .unwrap();

        let response = thread.join().unwrap();
        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.id, "req_2");
    }

    #[test]
    fn dispatched_request_reports_response_write_completion() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel();
        let (mut client, server, _path) = local_stream_pair("write-ack");
        client
            .write_all(br#"{"id":"req_write","method":"workspace.list","params":{}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let server_thread = std::thread::spawn(move || {
            handle_connection(server, &api_tx, &event_hub, &server_running, None)
        });

        let msg = api_rx.blocking_recv().unwrap();
        let response_write_complete = msg
            .response_write_complete
            .expect("socket-dispatched requests include write completion");
        msg.respond_to
            .send(
                serde_json::to_string(&SuccessResponse {
                    id: msg.request.id,
                    result: ResponseResult::Ok {},
                })
                .unwrap(),
            )
            .unwrap();

        response_write_complete
            .recv_timeout(Duration::from_secs(1))
            .expect("response write completion");
        let response: SuccessResponse = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response.id, "req_write");
        server_thread.join().unwrap().unwrap();
    }

    #[test]
    fn events_wait_agent_status_returns_initial_match() {
        let (api_tx, responder) =
            spawn_pane_get_responder(crate::api::schema::AgentStatus::Blocked);

        let (mut client, server, _path) = local_stream_pair("api-events-wait-initial");
        client
            .write_all(br#"{"id":"wait_1","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":1000}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let event_hub = EventHub::default();
        handle_connection(server, &api_tx, &event_hub, &running, None).unwrap();

        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "wait_1");
        assert_eq!(response["result"]["type"], "wait_matched");
        assert_eq!(
            response["result"]["event"]["data"]["agent_status"],
            "blocked"
        );
        drop(api_tx);
        responder.join().unwrap();
    }

    #[test]
    fn events_wait_agent_status_times_out_server_side() {
        let (api_tx, responder) =
            spawn_pane_get_responder(crate::api::schema::AgentStatus::Unknown);

        let (mut client, server, _path) = local_stream_pair("api-events-wait-timeout");
        client
            .write_all(br#"{"id":"wait_2","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":30}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let event_hub = EventHub::default();
        handle_connection(server, &api_tx, &event_hub, &running, None).unwrap();

        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "wait_2");
        assert_eq!(response["error"]["code"], "timeout");
        assert_eq!(
            response["error"]["message"],
            "timed out waiting for event match"
        );
        drop(api_tx);
        responder.join().unwrap();
    }

    #[test]
    fn events_wait_agent_status_returns_not_found_when_pane_closes() {
        let event_hub = EventHub::default();
        let responder_event_hub = event_hub.clone();
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let mut pane_get_count = 0;
            while let Some(msg) = api_rx.blocking_recv() {
                let Method::PaneGet(_) = msg.request.method else {
                    panic!("unexpected request: {:?}", msg.request.method);
                };
                pane_get_count += 1;
                let response = if pane_get_count == 1 {
                    serde_json::to_string(&SuccessResponse {
                        id: msg.request.id,
                        result: ResponseResult::PaneInfo {
                            pane: pane_info("pane_1", crate::api::schema::AgentStatus::Unknown),
                        },
                    })
                    .unwrap()
                } else {
                    if pane_get_count == 2 {
                        responder_event_hub.push(crate::api::schema::EventEnvelope {
                            event: crate::api::schema::EventKind::PaneClosed,
                            data: crate::api::schema::EventData::PaneClosed {
                                pane_id: "pane_1".into(),
                                workspace_id: "ws_1".into(),
                            },
                        });
                    }
                    error_response_json(
                        msg.request.id,
                        "pane_not_found",
                        "pane pane_1 not found".into(),
                    )
                };
                msg.respond_to.send(response).unwrap();
            }
        });

        let (mut client, server, _path) = local_stream_pair("wait-close");
        client
            .write_all(br#"{"id":"wait_close","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"done"},"timeout_ms":500}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        handle_connection(server, &api_tx, &event_hub, &running, None).unwrap();

        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "wait_close");
        assert_eq!(response["error"]["code"], "pane_not_found");
        assert_eq!(response["error"]["message"], "pane pane_1 not found");
        drop(api_tx);
        responder.join().unwrap();
    }

    #[test]
    fn wait_for_output_stops_when_client_disconnects() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
        let responder = std::thread::spawn(move || {
            let mut notified = false;
            while let Some(msg) = api_rx.blocking_recv() {
                assert!(matches!(msg.request.method, Method::PaneRead(_)));
                if !notified {
                    first_read_tx.send(()).unwrap();
                    notified = true;
                }
                msg.respond_to
                    .send(
                        serde_json::to_string(&SuccessResponse {
                            id: msg.request.id,
                            result: ResponseResult::PaneRead {
                                read: crate::api::schema::PaneReadResult {
                                    pane_id: "pane_1".into(),
                                    workspace_id: "ws_1".into(),
                                    tab_id: "tab_1".into(),
                                    source: crate::api::schema::ReadSource::RecentUnwrapped,
                                    format: crate::api::schema::ReadFormat::Text,
                                    text: String::new(),
                                    revision: 0,
                                    truncated: false,
                                },
                            },
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
        });

        let (mut client, server, _path) = local_stream_pair("api-wait-disconnect");
        client
            .write_all(br#"{"id":"req_wait","method":"pane.wait_for_output","params":{"pane_id":"pane_1","source":"recent","match":{"type":"substring","value":"never"}}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        first_read_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(client);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());

        server_thread.join().unwrap();
        drop(running);
        responder.join().unwrap();
    }

    fn workspace_focused_event(workspace_id: &str) -> crate::api::schema::EventEnvelope {
        crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceFocused,
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: workspace_id.into(),
            },
        }
    }

    fn read_json_line_from(reader: &mut BufReader<LocalStream>) -> serde_json::Value {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read subscription line");
        serde_json::from_str(&line).expect("decode subscription line")
    }

    /// HSR-03 回归（乘法路径）：订阅流在持续事件流下，不得把 pane 快照类订阅
    /// 对 app/渲染线程的同步往返次数按事件率放大。
    ///
    /// 此前 `stream_subscriptions` 在一次唤醒内最多 drain
    /// `MAX_SUBSCRIPTION_DRAIN_ROUNDS` 轮，而每轮都无差别重跑连接上的**全部**
    /// 订阅——只有 event hub 订阅是廉价的内存读，`pane.output_matched` /
    /// `pane.agent_status_changed` / `pane.scroll_changed` 每轮都要走一次
    /// `pane_get`/`pane_read`。于是「一个高频 event 订阅 + 一个 pane 快照订阅」
    /// 的混合连接把每 100 ms 1 次 `pane.get` 放大到最多 64 次。
    ///
    /// 这里让 responder 每处理一次 `pane.get` 就往 hub 里推一条事件，
    /// 确定性地构造出「每轮都非空」的条件：旧实现会跑满上限轮，新实现每个
    /// 轮询间隔只 poll 一次。
    #[test]
    fn subscription_stream_does_not_amplify_pane_dispatches_under_event_load() {
        use interprocess::local_socket::traits::Stream as _;
        use std::sync::atomic::AtomicUsize;

        let pane_get_count = Arc::new(AtomicUsize::new(0));
        let responder_count = Arc::clone(&pane_get_count);
        let event_hub = EventHub::default();
        let responder_hub = event_hub.clone();
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        // 订阅建立的探测请求（seen == 0）回复之后，`stream_subscriptions` 会
        // 立即（不经 sleep）跑循环的第一轮 poll，在响应线程上再发一次
        // `pane.get`。主线程读到 ack 与递增计数器分别发生在客户端 socket 与
        // server 线程两侧，二者之间没有同步点：满载下调度抖动会让第二次 poll
        // 抢在主线程读完计数之前完成，使断言偶发失败（HSR-03 deflake）。
        // 这里让响应线程在应答探测请求之后阻塞，直到主线程确认过「建立时只
        // poll 一次」再放行，把断言钉死在确定的时间点上，而不是赛跑。
        let (probe_checked_tx, probe_checked_rx) = std::sync::mpsc::channel::<()>();
        let responder = std::thread::spawn(move || {
            let mut gate = Some(probe_checked_rx);
            while let Some(msg) = api_rx.blocking_recv() {
                let Method::PaneGet(_) = msg.request.method else {
                    panic!("unexpected request: {:?}", msg.request.method);
                };
                let seen = responder_count.fetch_add(1, Ordering::Relaxed);
                // 订阅建立时的探测不计入负载；之后每次快照查询都补一条事件，
                // 保证「本轮有事件」恒成立。
                if seen > 0 {
                    responder_hub.push(workspace_focused_event(&format!("ws_{seen}")));
                }
                msg.respond_to
                    .send(
                        serde_json::to_string(&SuccessResponse {
                            id: msg.request.id,
                            result: ResponseResult::PaneInfo {
                                // `scroll: None` 与探测快照一致，订阅本身不产出事件，
                                // 计数器量到的就是纯粹的 poll 次数。
                                pane: pane_info("pane_1", crate::api::schema::AgentStatus::Unknown),
                            },
                        })
                        .unwrap(),
                    )
                    .unwrap();
                if let Some(rx) = gate.take() {
                    // 阻塞在这里不会拖慢用例：主线程一读完 ack 就立刻放行，
                    // 超时只是防止断言失败时线程泄漏。
                    let _ = rx.recv_timeout(APP_RESPONSE_TIMEOUT);
                }
            }
        });

        let (mut client, server, path) = local_stream_pair("api-sub-amplify");
        client
            .write_all(
                br#"{"id":"sub_amplify","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"},{"type":"pane.scroll_changed","pane_id":"pane_1"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_recv_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let server_api_tx = api_tx.clone();
        let server_event_hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &server_api_tx,
                &server_event_hub,
                &server_running,
                None,
            );
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(client);
        let ack = read_json_line_from(&mut reader);
        assert_eq!(ack["result"]["type"], "subscription_started");
        assert_eq!(
            pane_get_count.load(Ordering::Relaxed),
            1,
            "订阅建立时只做一次探测快照"
        );
        // 断言过关，放行响应线程处理循环第一轮的 poll；此前它一直卡在探测
        // 请求的回复之后，计数器不会在断言读到之前被第二次 poll 提前推进。
        let _ = probe_checked_tx.send(());

        // 观察窗口：先推一条事件点火，然后放任自流数个轮询间隔。
        event_hub.push(workspace_focused_event("ignition"));
        const OBSERVED_INTERVALS: u32 = 3;
        std::thread::sleep(
            CONNECTION_POLL_INTERVAL * OBSERVED_INTERVALS + CONNECTION_POLL_INTERVAL / 2,
        );

        let dispatches = pane_get_count.load(Ordering::Relaxed);
        // 每个轮询间隔至多 1 次快照查询；留出一个间隔的调度抖动余量。
        let budget = (OBSERVED_INTERVALS + 2) as usize;
        assert!(
            dispatches <= budget,
            "pane 快照订阅每个轮询间隔至多 poll 一次：{OBSERVED_INTERVALS} 个间隔内预算 {budget} 次，实际 {dispatches} 次"
        );

        drop(reader);
        let result = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
        drop(api_tx);
        responder.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    /// 订阅流必须每个轮询间隔量级就回到 `running` 检查。此前一次唤醒内的
    /// drain 循环从不复查停机信号，最坏要等满上限轮（每轮每个 pane 订阅还可能
    /// 各等一次 `APP_RESPONSE_TIMEOUT`）才回到外层。
    #[test]
    fn subscription_stream_stops_within_a_poll_interval_when_server_stops() {
        use interprocess::local_socket::traits::Stream as _;

        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, path) = local_stream_pair("api-sub-stop");
        client
            .write_all(
                br#"{"id":"sub_stop","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_recv_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(client);
        let ack = read_json_line_from(&mut reader);
        assert_eq!(ack["result"]["type"], "subscription_started");

        // 持续有事件可投递时也要能停：这正是旧 drain 循环饿死停机信号的条件。
        for index in 0..64 {
            hub.push(workspace_focused_event(&format!("ws_{index}")));
        }
        running.store(false, Ordering::Relaxed);

        let started = Instant::now();
        let result = done_rx
            .recv_timeout(CONNECTION_POLL_INTERVAL * 20)
            .expect("订阅线程应在一个轮询间隔量级内返回");
        assert!(result.is_ok());
        assert!(
            started.elapsed() < CONNECTION_POLL_INTERVAL * 20,
            "停机信号不得被单次唤醒内的投递饿死，实际 {:?}",
            started.elapsed()
        );

        server_thread.join().unwrap();
        drop(reader);
        let _ = std::fs::remove_file(path);
    }

    /// HSR-03 回归：整批事件必须在一个轮询间隔内投递完。旧行为是每轮 1 条，
    /// 50 条要 50 × 100 ms ≈ 5 s。
    #[test]
    fn subscription_stream_delivers_an_event_burst_within_one_poll_interval() {
        use interprocess::local_socket::traits::Stream as _;

        const BURST: usize = 50;

        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, path) = local_stream_pair("api-sub-burst");
        client
            .write_all(
                br#"{"id":"sub_burst","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_recv_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(client);
        let ack = read_json_line_from(&mut reader);
        assert_eq!(ack["result"]["type"], "subscription_started");

        for index in 0..BURST {
            hub.push(workspace_focused_event(&format!("ws_{index}")));
        }

        let started = Instant::now();
        // HSR-03/APP-006：线上帧带 server 事件序号（纯追加的可选字段）。被测
        // 行为是「一轮内投递完整批事件且序号严格递增」，不是序号的绝对起点——
        // 启动路径将来多推一条事件不该让这条断言变红，所以以首帧为基准。
        let mut previous_sequence: Option<u64> = None;
        for index in 0..BURST {
            let event = read_json_line_from(&mut reader);
            assert_eq!(event["event"], "workspace_focused");
            assert_eq!(event["data"]["workspace_id"], format!("ws_{index}"));
            let sequence = event["sequence"].as_u64().expect("事件帧必须带序号");
            if let Some(previous) = previous_sequence {
                assert_eq!(sequence, previous + 1, "同一批事件的序号必须严格 +1");
            }
            previous_sequence = Some(sequence);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "{BURST} 条事件应在一个轮询间隔内投递完，实际耗时 {elapsed:?}"
        );

        drop(reader);
        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    /// HSR-03/APP-006 端到端回归：断层通知帧必须真的写到订阅连接的线上，
    /// 排在幸存事件之前。单元级只钉住「帧的产生」，这条钉住「帧的投递与分帧」。
    /// 确定性靠两件事：`with_test_capacity` 把环形缓冲调到 4 格，
    /// `push_batch` 一次持锁推完，轮询不可能落在推入中间。
    #[test]
    fn subscription_stream_reports_the_gap_before_the_events_that_survived() {
        use interprocess::local_socket::traits::Stream as _;

        const CAPACITY: usize = 4;
        const PUSHED: usize = 10;

        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, path) = local_stream_pair("api-sub-gap");
        client
            .write_all(
                br#"{"id":"sub_gap","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"notices":true}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_recv_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::with_test_capacity(CAPACITY);
        let hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(client);
        let ack = read_json_line_from(&mut reader);
        assert_eq!(ack["result"]["type"], "subscription_started");

        hub.push_batch(
            (0..PUSHED)
                .map(|index| workspace_focused_event(&format!("ws_{index}")))
                .collect(),
        );

        let notice = read_json_line_from(&mut reader);
        assert_eq!(notice["event"], "events.lost", "断层通知必须是第一行");
        assert_eq!(notice["data"]["from"], 1);
        assert_eq!(notice["data"]["to"], (PUSHED - CAPACITY) as u64);

        for index in (PUSHED - CAPACITY)..PUSHED {
            let event = read_json_line_from(&mut reader);
            assert_eq!(event["event"], "workspace_focused");
            assert_eq!(event["data"]["workspace_id"], format!("ws_{index}"));
            assert_eq!(event["sequence"], index as u64 + 1);
        }

        drop(reader);
        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    /// 未开启 `notices` 的订阅方（所有既有客户端）拿到的仍然只有事件行：
    /// `events.lost` 这个 `EventKind` 之外的 `event` 名不会凭空出现在流上。
    #[test]
    fn subscription_stream_omits_gap_notices_unless_requested() {
        use interprocess::local_socket::traits::Stream as _;

        const CAPACITY: usize = 4;
        const PUSHED: usize = 10;

        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, path) = local_stream_pair("api-sub-no-notice");
        client
            .write_all(
                br#"{"id":"sub_no_notice","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_recv_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::with_test_capacity(CAPACITY);
        let hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(client);
        let ack = read_json_line_from(&mut reader);
        assert_eq!(ack["result"]["type"], "subscription_started");

        hub.push_batch(
            (0..PUSHED)
                .map(|index| workspace_focused_event(&format!("ws_{index}")))
                .collect(),
        );

        for index in (PUSHED - CAPACITY)..PUSHED {
            let event = read_json_line_from(&mut reader);
            assert_eq!(
                event["event"], "workspace_focused",
                "未请求通知帧时第一行就应该是幸存事件"
            );
            assert_eq!(event["data"]["workspace_id"], format!("ws_{index}"));
        }

        drop(reader);
        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    /// HSR-07 回归：客户端在订阅流上追加心跳字节后，订阅必须继续投递事件，
    /// 而不是被探测当成连接关闭。
    #[test]
    fn subscription_stream_survives_client_writes() {
        use interprocess::local_socket::traits::Stream as _;

        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, path) = local_stream_pair("api-sub-heartbeat");
        client
            .write_all(
                br#"{"id":"sub_heartbeat","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        client
            .set_recv_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let hub = event_hub.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let mut reader = BufReader::new(client);
        let ack = read_json_line_from(&mut reader);
        assert_eq!(ack["result"]["type"], "subscription_started");

        reader.get_mut().write_all(b"\n").unwrap();
        reader.get_mut().flush().unwrap();

        hub.push(workspace_focused_event("after_heartbeat"));
        let event = read_json_line_from(&mut reader);
        assert_eq!(event["data"]["workspace_id"], "after_heartbeat");

        drop(reader);
        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_requests_preserve_only_unambiguous_string_ids() {
        let cases = [
            (
                r#"{"id":"mine","method":"pane.report_agent","params":{"pane_id":"w1:p1","status":"working","source":"x"}}"#,
                "mine",
            ),
            (r#"{"id":"escaped\"id","method":"unknown"}"#, "escaped\"id"),
            (r#"{"method":"unknown","params":{"id":"nested"}}"#, ""),
            (r#"{"id":123,"method":"unknown"}"#, ""),
            (
                r#"{"id":"first","id":"second","method":"ping","params":{}}"#,
                "",
            ),
            (r#"{"id":"truncated","method":"ping""#, ""),
            (r#"["not-an-object"]"#, ""),
        ];
        for (request, expected_id) in cases {
            let (api_tx, mut api_rx) = mpsc::unbounded_channel();
            let (mut client, server, path) = local_stream_pair("invalid-request-id");
            writeln!(client, "{request}").unwrap();
            let running = Arc::new(AtomicBool::new(true));
            handle_connection(server, &api_tx, &EventHub::default(), &running, None).unwrap();

            let mut response = String::new();
            BufReader::new(client)
                .read_to_string(&mut response)
                .unwrap();
            let response: ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(response.id, expected_id, "{request}");
            assert_eq!(response.error.code, "invalid_request");
            assert!(response.error.message.starts_with("invalid request: "));
            assert!(
                api_rx.try_recv().is_err(),
                "invalid requests must not dispatch"
            );
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn subscription_setup_errors_preserve_request_id_and_reject_entire_stream() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let event_hub = EventHub::default();
        let responder_event_hub = event_hub.clone();
        let responder = std::thread::spawn(move || {
            let msg = api_rx.blocking_recv().unwrap();
            let Method::PaneGet(params) = msg.request.method else {
                panic!("unexpected request: {:?}", msg.request.method);
            };
            assert_eq!(params.pane_id, "w999:p9");
            responder_event_hub.push(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneClosed,
                data: crate::api::schema::EventData::PaneClosed {
                    pane_id: "w999:p9".into(),
                    workspace_id: "w999".into(),
                },
            });
            msg.respond_to
                .send(error_response_json(
                    msg.request.id,
                    "pane_not_found",
                    "pane w999:p9 not found".into(),
                ))
                .unwrap();
            assert!(
                api_rx.blocking_recv().is_none(),
                "rejection must not start polling"
            );
        });
        let (mut client, server, path) = local_stream_pair("subscription-error-id");
        let request = r#"{"id":"panefold:events","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"},{"type":"pane.closed"},{"type":"pane.agent_status_changed","pane_id":"w999:p9"}]}}"#;
        writeln!(client, "{request}").unwrap();
        let running = Arc::new(AtomicBool::new(true));
        handle_connection(server, &api_tx, &event_hub, &running, None).unwrap();
        drop(api_tx);
        responder.join().unwrap();

        let mut response = String::new();
        BufReader::new(client)
            .read_to_string(&mut response)
            .unwrap();
        let response: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(response.id, "panefold:events");
        assert_eq!(response.error.code, "pane_not_found");
        assert_eq!(response.error.message, "pane w999:p9 not found");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn subscriptions_stop_when_client_disconnects() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-disconnect");
        client
            .write_all(
                br#"{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");
        assert_eq!(ack["id"], "sub_1");

        drop(client);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }

    #[test]
    fn subscriptions_stop_when_server_shuts_down() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-shutdown");
        client
            .write_all(
                br#"{"id":"sub_2","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).unwrap();
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        running.store(false, Ordering::Relaxed);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }
}
