use super::*;

pub(super) fn dispatch_client_shell_actions(
    actions: Vec<shell::ClientShellAction>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    mut shell: Option<&mut shell::ClientShellState>,
    detached_process_children: &mut Vec<std::process::Child>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Result<(Vec<crossterm::event::MouseEvent>, bool), ClientError> {
    let mut replay_mouse = Vec::new();
    let mut repaint = false;
    for action in actions {
        match action {
            shell::ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
                coalesce,
            } => {
                if let Some(connection) = endpoints.connection(&endpoint_id).filter(|_| {
                    crate::api::api_method_name(&request.method).starts_with("account.")
                        || crate::api::api_method_name(&request.method).starts_with("system.")
                        || (endpoints.active_id() == &endpoint_id
                            && endpoints.active_surface_available())
                }) {
                    let superseded = endpoint_commands.enqueue(
                        endpoint_id,
                        connection.generation,
                        boot_id,
                        request,
                        coalesce,
                    );
                    if let Some(shell) = shell.as_deref_mut() {
                        for request_id in superseded {
                            repaint |= shell.supersede_endpoint_request(&request_id);
                        }
                    }
                } else if let Some(shell) = shell.as_deref_mut() {
                    repaint |= shell.cancel_endpoint_request(&request.id);
                }
            }
            shell::ClientShellAction::EndpointRequest {
                endpoint_id,
                boot_id,
                request,
            } => {
                // Cross-endpoint fire (snippet runs): the target lane is
                // drained immediately; generation fencing in `send_next`
                // rejects stale connections.
                if let Some(connection) = endpoints.connection(&endpoint_id) {
                    let generation = connection.generation;
                    let superseded = endpoint_commands.enqueue(
                        endpoint_id.clone(),
                        generation,
                        boot_id,
                        request,
                        false,
                    );
                    let cancelled = endpoint_commands.send_next(&endpoint_id, endpoints);
                    if let Some(shell) = shell.as_deref_mut() {
                        for request_id in superseded {
                            repaint |= shell.supersede_endpoint_request(&request_id);
                        }
                        for request_id in cancelled {
                            repaint |= shell.cancel_endpoint_request(&request_id);
                        }
                    }
                } else if let Some(shell) = shell.as_deref_mut() {
                    repaint |= shell.cancel_endpoint_request(&request.id);
                }
            }
            shell::ClientShellAction::ClipboardWrite(bytes) => {
                crate::selection::write_osc52_bytes(&bytes);
            }
            shell::ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target,
            } => {
                let _ = event_tx.try_send(ClientLoopEvent::ActivateEndpoint {
                    endpoint_id,
                    target,
                    force: false,
                });
            }
            shell::ClientShellAction::ReconnectEndpoint { endpoint_id } => {
                let _ = event_tx.try_send(ClientLoopEvent::ReconnectEndpoint { endpoint_id });
            }
            shell::ClientShellAction::MachineHostKeyOp {
                cancel,
                ticket,
                op,
                profile,
                reviewed,
            } => {
                let op_tx = event_tx.clone();
                std::thread::spawn(move || {
                    let result = cancel
                        .run(|| match op {
                            shell::MachineHostKeyOp::Scan => {
                                crate::remote::review_profile_host_key(&profile)
                                    .map(shell::MachineHostKeyOutcome::Scanned)
                            }
                            shell::MachineHostKeyOp::Precollect => {
                                let (target, key) = reviewed
                                    .as_ref()
                                    .ok_or_else(|| io::Error::other("请先查看并确认主机指纹"))?;
                                crate::remote::remember_reviewed_host_key(&profile, target, key)
                                    .map(shell::MachineHostKeyOutcome::Precollected)
                            }
                            shell::MachineHostKeyOp::Remove => {
                                crate::remote::remove_profile_host_key(&profile)
                                    .map(|_| shell::MachineHostKeyOutcome::Removed)
                            }
                        })
                        .map_err(|error| error.to_string());
                    let _ = op_tx.blocking_send(ClientLoopEvent::MachineAuth {
                        update: shell::MachineAuthUpdate::HostKeyOpFinished { ticket, op, result },
                    });
                });
            }
            shell::ClientShellAction::StartMachineInteractiveAuth {
                bootstrap,
                pin,
                ticket,
                profile,
                cancel,
            } => {
                let options = shell
                    .as_ref()
                    .and_then(|shell| shell.endpoint_connect_options);
                let auth_tx = event_tx.clone();
                std::thread::spawn(move || {
                    let run = || -> io::Result<endpoint::PreparedEndpointConnection> {
                        let (channel, prompts) = crate::remote::start_interactive_auth_channel(
                            crate::remote::SshAuthApproval::Approved,
                        )?;
                        let prompt_tx = auth_tx.clone();
                        std::thread::spawn(move || {
                            while let Some(prompt) = prompts.recv() {
                                // A dropped prompt declines itself, so a dead
                                // loop fails the ssh attempt fast.
                                if prompt_tx
                                    .blocking_send(ClientLoopEvent::MachineAuthPrompt {
                                        ticket,
                                        prompt: Box::new(prompt),
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                        let connected = crate::remote::connect_saved_ssh_authenticated(
                            &profile,
                            channel,
                            bootstrap,
                            pin.as_ref(),
                            &|step| {
                                let _ = auth_tx.blocking_send(ClientLoopEvent::MachineAuth {
                                    update: shell::MachineAuthUpdate::InteractiveStep {
                                        ticket,
                                        step,
                                    },
                                });
                            },
                        )?;
                        endpoint::prepare_interactive_connection(
                            connected,
                            options.ok_or_else(|| {
                                io::Error::other("终端尺寸尚未准备好，请重试连接")
                            })?,
                            &cancel,
                        )
                    };
                    match cancel.run(run) {
                        Ok(connection) if !cancel.is_cancelled() => {
                            cancel.complete();
                            let _ =
                                auth_tx.blocking_send(ClientLoopEvent::MachineInteractiveReady {
                                    ticket,
                                    connection: Box::new(connection),
                                });
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = auth_tx.blocking_send(ClientLoopEvent::MachineAuth {
                                update: shell::MachineAuthUpdate::InteractiveFinished {
                                    ticket,
                                    result: Err(error.to_string()),
                                },
                            });
                        }
                    }
                });
            }
            shell::ClientShellAction::AnswerMachineAuthPrompt { ticket, answer } => {
                let _ = event_tx.try_send(ClientLoopEvent::MachineAuthAnswer { ticket, answer });
            }
            shell::ClientShellAction::CancelMachineInteractiveAuth { ticket } => {
                let _ = event_tx.try_send(ClientLoopEvent::MachineAuthCancel { ticket });
            }
            shell::ClientShellAction::BootstrapMachine {
                cancel,
                ticket,
                target,
                session,
                options,
            } => {
                let bootstrap_tx = event_tx.clone();
                std::thread::spawn(move || {
                    let send = |update: shell::MachineBootstrapUpdate| {
                        let _ = bootstrap_tx
                            .blocking_send(ClientLoopEvent::MachineBootstrap { ticket, update });
                    };
                    let result = cancel.run(|| {
                        crate::remote::prepare_saved_ssh_unattended(
                            &target,
                            &session,
                            options.as_ref(),
                            &|step| send(shell::MachineBootstrapUpdate::Step(step)),
                        )
                    });
                    send(shell::MachineBootstrapUpdate::Finished(
                        result.map_err(|error| error.to_string()),
                    ));
                });
            }
            shell::ClientShellAction::MachineFsOp {
                cancel,
                ticket,
                profile,
                op,
            } => {
                let fs_tx = event_tx.clone();
                std::thread::spawn(move || {
                    let result = cancel
                        .run(|| run_machine_fs_op(&profile, op).map_err(io::Error::other))
                        .map_err(|error| error.to_string());
                    let _ = fs_tx.blocking_send(ClientLoopEvent::MachineFs { ticket, result });
                });
            }
            shell::ClientShellAction::OpenSafeWebUrl(url) => {
                if crate::app::actions::safe_web_url(&url).is_some() {
                    match crate::platform::open_url(&url) {
                        Ok(Some(child)) => detached_process_children.push(child),
                        Ok(None) => {}
                        Err(err) => warn!(err = %err, url = %url, "failed to open pane URL"),
                    }
                }
            }
            shell::ClientShellAction::ReplayMouse(events) => replay_mouse.extend(events),
        }
    }
    // A source-off-first handoff leaves the registry's committed identity pointing at a
    // deliberately surface-inactive source. Do not drain its retained queue into a server that
    // must reject it; completion below resumes the committed owner's lane.
    if endpoints.active_surface_available() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = shell {
            for request_id in cancelled {
                repaint |= shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    Ok((replay_mouse, repaint))
}

/// Executes one file-browser operation on the worker thread: connect the
/// profile's sftp channel, run the operation, and map the outcome. Uploads
/// read the local file here (with the small-file cap) so the UI never does
/// blocking disk I/O either.
fn run_machine_fs_op(
    profile: &crate::client::endpoint::SavedSshEndpoint,
    op: shell::MachineFsOp,
) -> Result<shell::MachineFsOutcome, String> {
    let fs = crate::remote::RemoteFs::connect(profile).map_err(|error| error.to_string())?;
    match op {
        shell::MachineFsOp::List { path } => fs
            .list_dir(&path)
            .map(|entries| shell::MachineFsOutcome::Entries { entries })
            .map_err(|error| error.to_string()),
        shell::MachineFsOp::Read { path } => fs
            .read_small_file(&path)
            .map(|content| shell::MachineFsOutcome::FileContent { content })
            .map_err(|error| error.to_string()),
        shell::MachineFsOp::Download {
            remote,
            local,
            message,
        } => {
            // 不静默覆盖本地同名文件（HERDR-MACH-020）：目标默认就是远程文件名，
            // 落在客户端 CWD，覆盖掉别人的东西是不可逆的。
            if std::path::Path::new(&local).exists() {
                return Err(crate::i18n::fill(
                    crate::i18n::texts().machine_files.download_exists_fmt,
                    &[("path", &local)],
                ));
            }
            let data = fs
                .read_small_file(&remote)
                .map_err(|error| error.to_string())?;
            // Atomic with private permissions, like `machine fs get`.
            crate::client::endpoint::store_private_json(
                std::path::Path::new(&local),
                &data,
                "downloaded file",
            )?;
            Ok(shell::MachineFsOutcome::Changed { message })
        }
        shell::MachineFsOp::Upload {
            local,
            remote,
            message,
        } => {
            let metadata = std::fs::metadata(&local).map_err(|error| error.to_string())?;
            if !metadata.is_file() {
                return Err(crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_fs_local_not_file_fmt,
                    &[("path", &local)],
                ));
            }
            if metadata.len() > crate::remote::MAX_SMALL_FILE_BYTES {
                return Err(crate::i18n::fill(
                    crate::i18n::texts().cli_errors.machine_fs_too_large_fmt,
                    &[
                        ("size", &metadata.len().to_string()),
                        ("limit", &crate::remote::MAX_SMALL_FILE_BYTES.to_string()),
                    ],
                ));
            }
            let data = std::fs::read(&local).map_err(|error| error.to_string())?;
            fs.write_small_file(&remote, &data)
                .map_err(|error| error.to_string())?;
            Ok(shell::MachineFsOutcome::Changed { message })
        }
        shell::MachineFsOp::Mkdir { path, message } => fs
            .mkdir(&path, true)
            .map(|()| shell::MachineFsOutcome::Changed { message })
            .map_err(|error| error.to_string()),
        shell::MachineFsOp::Rename { from, to, message } => fs
            .rename(&from, &to)
            .map(|()| shell::MachineFsOutcome::Changed { message })
            .map_err(|error| error.to_string()),
        shell::MachineFsOp::Delete {
            path,
            recursive,
            message,
        } => fs
            .delete(&path, recursive)
            .map(|()| shell::MachineFsOutcome::Changed { message })
            .map_err(|error| error.to_string()),
    }
}

pub(super) fn client_shell_resize_message(
    shell: &shell::ClientShellState,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    pixel_mouse: bool,
) -> ClientMessage {
    ClientMessage::ClientShellResize {
        cell_width_px,
        cell_height_px,
        surface_size: shell.surface_size(cols, rows),
        pixel_mouse,
    }
}

pub(super) fn sync_client_shell_keyboard_report_all(
    state: &mut ClientState,
) -> Result<(), ClientError> {
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    let desired = state.pane_keyboard_report_all || shell.host_keyboard_report_all_requested();
    if desired == state.keyboard_report_all_active {
        return Ok(());
    }
    crate::terminal_modes::set_host_kitty_keyboard_report_all(&mut io::stdout(), desired)
        .map_err(ClientError::ConnectionFailed)?;
    state.keyboard_report_all_active = desired;
    Ok(())
}

pub(super) fn clear_endpoint_host_effects(
    state: &mut ClientState,
    host_mouse_capture_active: &std::sync::atomic::AtomicBool,
    host_sgr_pixels_active: &std::sync::atomic::AtomicBool,
) {
    state.endpoint_mouse_capture_requested = false;
    state.endpoint_sgr_pixels_requested = false;
    let enabled = if state.shell.is_some() {
        state.shell_mouse_capture_preference
    } else {
        state.direct_mouse_capture_preference
    };
    let sgr_pixels = super::effective_sgr_pixel_mouse(enabled, false, state.pixel_geometry_exact);
    if enabled != state.mouse_capture_active
        || sgr_pixels != host_sgr_pixels_active.load(std::sync::atomic::Ordering::Acquire)
    {
        let _ = super::set_mouse_capture(enabled, sgr_pixels);
    }
    state.mouse_capture_active = enabled;
    host_mouse_capture_active.store(enabled, std::sync::atomic::Ordering::Release);
    host_sgr_pixels_active.store(sgr_pixels, std::sync::atomic::Ordering::Release);

    state.pane_keyboard_report_all = false;
    let _ = sync_client_shell_keyboard_report_all(state);
    let _ = crate::terminal_effects::write_window_title(&mut std::io::stdout(), None);
}

pub(super) fn apply_client_shell_input_source_changes(
    state: &mut ClientState,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
) {
    let changes = state
        .shell
        .as_mut()
        .map(shell::ClientShellState::take_input_source_changes)
        .unwrap_or_default();
    for active in changes {
        if active {
            prefix_input_source.switch_to_ascii();
        } else {
            prefix_input_source.restore();
        }
    }
}

fn install_pending_activation(
    state: &mut ClientState,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    activation: endpoint::PendingEndpointActivation,
) {
    let retired = activation
        .source_command_lane()
        .map(|source| endpoint_commands.retire_lane(source))
        .unwrap_or_default();
    if let Some(shell) = state.shell.as_mut() {
        for request_id in retired {
            shell.cancel_endpoint_request(&request_id);
        }
    }
    *next_surface_serial = next_surface_serial.saturating_add(1);
    state.freeze_presentation();
    *pending = Some(activation);
}

pub(super) fn begin_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    endpoint_id: endpoint::ClientEndpointId,
    target: Option<shell::ClientEndpointFocusTarget>,
    force: bool,
    now: std::time::Instant,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Result<(), ClientError> {
    if let Some(activation) = pending.as_mut() {
        if activation.can_retarget(&endpoint_id) {
            let retarget_error = activation.retarget(target, endpoints).err();
            if let Some(error) = retarget_error {
                rollback_endpoint_activation(state, endpoints, pending, error, false);
            }
        } else {
            // Once rollback starts, even a request for the original target is a new intent. It
            // replaces the retained successor instead of mutating the transaction being retired.
            let outcome = activation.supersede(endpoint_id, target, endpoints);
            if let endpoint::ActivationRollback::Unavailable(message) = outcome {
                *pending = None;
                present_handoff_unavailable(state, message);
            }
        }
        return Ok(());
    }
    let already_active = !force
        && endpoints.active_id() == &endpoint_id
        && endpoints
            .connection(&endpoint_id)
            .is_some_and(|connection| connection.surface_active);
    if already_active {
        if let (Some(shell), Some(target)) = (state.shell.as_mut(), target) {
            let actions = shell.focus_endpoint_target(target);
            let (_, repaint) = dispatch_client_shell_actions(
                actions,
                endpoint_commands,
                endpoints,
                Some(shell),
                &mut state.detached_process_children,
                event_tx,
            )?;
            if repaint {
                if let Some(frame) = shell.compose(state.reported_size.0, state.reported_size.1) {
                    state.present_frame(frame);
                }
            }
        }
        return Ok(());
    }
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    let resize = client_shell_resize_message(
        shell,
        state.reported_size.0,
        state.reported_size.1,
        state.reported_cell_size.0,
        state.reported_cell_size.1,
        state.pixel_geometry_exact,
    );
    match endpoint::PendingEndpointActivation::begin(
        shell,
        endpoints,
        endpoint_id.clone(),
        target,
        resize,
        *next_surface_serial,
        now,
    ) {
        Ok(activation) => install_pending_activation(
            state,
            endpoint_commands,
            pending,
            next_surface_serial,
            activation,
        ),
        Err(endpoint::ActivationBeginError::Preflight(error)) => {
            if let Some(shell) = state.shell.as_mut() {
                shell.receive_endpoint_unavailable(format!(
                    "{}: {error}",
                    shell.endpoint_label(&endpoint_id)
                ));
            }
        }
        Err(endpoint::ActivationBeginError::Partial { activation, error }) => {
            // A send error is not evidence that its peer did not observe the write. Freeze and
            // retain the lifecycle object before rollback so no source or target output can be
            // projected until one ownership path has been proved again.
            install_pending_activation(
                state,
                endpoint_commands,
                pending,
                next_surface_serial,
                *activation,
            );
            rollback_endpoint_activation(
                state,
                endpoints,
                pending,
                format!(
                    "{}: {error}",
                    state
                        .shell
                        .as_ref()
                        .map(|shell| shell.endpoint_label(&endpoint_id).to_owned())
                        .unwrap_or_else(|| format!("{endpoint_id:?}"))
                ),
                false,
            );
        }
    }
    Ok(())
}

pub(super) fn complete_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
) -> Result<Option<ClientLoopEvent>, ClientError> {
    let sync_endpoint = pending
        .as_ref()
        .and_then(endpoint::PendingEndpointActivation::presentation_sync_endpoint)
        .cloned();
    if let Some(endpoint_id) = sync_endpoint.as_ref() {
        state.replay_host_theme(endpoints, endpoint_id);
    }
    let completion = {
        let Some(activation) = pending.as_mut() else {
            return Ok(None);
        };
        let Some(shell) = state.shell.as_mut() else {
            return Ok(None);
        };
        match activation.complete(shell, endpoints) {
            Ok(completion) => completion,
            Err(error) => {
                shell.receive_endpoint_unavailable(error);
                return Ok(None);
            }
        }
    };

    if matches!(
        completion,
        endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
    ) {
        #[cfg(unix)]
        if let endpoint::ActivationCompletion::AwaitingPresentationSync { previous, endpoint } =
            &completion
        {
            if previous != endpoint {
                state.retire_endpoint_graphics(previous);
            }
        }
        // The coherent target frame can replace the frozen source now, but the registry keeps
        // pane input disabled until a second projection epoch has replayed host modes/effects.
        state.unfreeze_presentation();
        let (cleanup, frame) = {
            let shell = state.shell.as_mut().expect("checked client shell");
            (
                shell.take_pending_graphics_cleanup(),
                shell.compose(state.reported_size.0, state.reported_size.1),
            )
        };
        state.present_graphics(&cleanup);
        if let Some(frame) = frame {
            state.present_frame(frame);
        } else {
            state.flush_pending_graphics();
        }
        return Ok(None);
    }
    if completion == endpoint::ActivationCompletion::AwaitingPresentationEffects {
        return Ok(None);
    }

    let _ = pending.take();
    if let Some(shell) = &mut state.shell {
        shell.renew_workbench_surface();
    }
    endpoints.unfreeze_input();
    let successor = match completion {
        endpoint::ActivationCompletion::RestoredSource {
            error,
            successor: next,
            ..
        } => {
            if next.is_none() {
                if let Some(shell) = state.shell.as_mut() {
                    shell.receive_endpoint_unavailable(error);
                }
            }
            next
        }
        endpoint::ActivationCompletion::Activated => None,
        endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
        | endpoint::ActivationCompletion::AwaitingPresentationEffects => unreachable!(),
    };
    state.unfreeze_presentation();
    if successor.is_none() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = state.shell.as_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    let (cleanup, frame) = {
        let shell = state.shell.as_mut().expect("checked client shell");
        (
            shell.take_pending_graphics_cleanup(),
            shell.compose(state.reported_size.0, state.reported_size.1),
        )
    };
    state.present_graphics(&cleanup);
    if let Some(frame) = frame {
        state.present_frame(frame);
    } else {
        state.flush_pending_graphics();
    }
    if let Some(intent) = successor {
        return Ok(Some(ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: true,
        }));
    }
    Ok(None)
}

pub(super) fn present_handoff_unavailable(state: &mut ClientState, message: String) {
    // An unavailable committed endpoint has no presentation lease. Keep all pane input and late
    // source output blocked, while allowing this client-owned chrome frame through the freeze.
    state.freeze_presentation();
    let frame = state.shell.as_mut().and_then(|shell| {
        shell.receive_endpoint_unavailable(message);
        shell.compose(state.reported_size.0, state.reported_size.1)
    });
    if let Some(frame) = frame {
        state.present_frozen_chrome(frame);
    }
}

pub(super) fn rollback_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    error: String,
    source_release_rejected: bool,
) {
    let Some(activation) = pending.as_mut() else {
        return;
    };
    match activation.rollback(endpoints, error.clone(), source_release_rejected) {
        endpoint::ActivationRollback::Pending => state.freeze_presentation(),
        endpoint::ActivationRollback::Unavailable(message) => {
            *pending = None;
            // No endpoint has been proven safe to present. Keep pane input frozen, but render
            // the client-owned unavailable chrome rather than silently swallowing the error.
            present_handoff_unavailable(state, message);
        }
    }
}

pub(super) fn handle_endpoint_disconnect(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    notice: &str,
) -> bool {
    supervisors.disconnected(endpoint_id, generation, now);
    #[cfg(unix)]
    state.retire_endpoint_graphics(endpoint_id);
    if pending_activation
        .as_ref()
        .is_some_and(|pending| pending.involves_endpoint(endpoint_id))
    {
        let outcome = pending_activation
            .as_mut()
            .expect("checked pending activation")
            .endpoint_disconnected(
                endpoints,
                endpoint_id,
                format!("endpoint connection was lost while activating {notice}"),
            );
        match outcome {
            endpoint::ActivationRollback::Pending => {}
            endpoint::ActivationRollback::Unavailable(error) => {
                *pending_activation = None;
                present_handoff_unavailable(state, error);
            }
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.shell.as_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.mark_endpoint_disconnected(endpoint_id);
        shell.note_endpoint_reconnect_attempt(endpoint_id, now);
        endpoint_was_active.then(|| format!("{} {notice}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    endpoint_was_active
}

pub(super) fn handle_endpoint_attention(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    message: String,
) -> bool {
    endpoints.disconnect(endpoint_id);
    supervisors.record_status(
        endpoint_id,
        generation,
        endpoint::ClientEndpointStatus::Attention,
        now,
    );
    #[cfg(unix)]
    state.retire_endpoint_graphics(endpoint_id);
    if pending_activation
        .as_ref()
        .is_some_and(|pending| pending.involves_endpoint(endpoint_id))
    {
        let outcome = pending_activation
            .as_mut()
            .expect("checked pending activation")
            .endpoint_disconnected(
                endpoints,
                endpoint_id,
                "endpoint reported attention while activating".into(),
            );
        if let endpoint::ActivationRollback::Unavailable(error) = outcome {
            *pending_activation = None;
            present_handoff_unavailable(state, error);
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.shell.as_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Attention);
        endpoint_was_active.then(|| format!("{}: {message}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    endpoint_was_active
}

pub(super) fn install_client_shell_snapshot(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    snapshot: Box<crate::protocol::ClientShellSnapshot>,
    projection_pending: bool,
    endpoints: &mut endpoint::EndpointRegistry,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
) -> Result<(), ClientError> {
    let Some(connection) = endpoints.connection(endpoint_id) else {
        return Ok(());
    };
    let generation = connection.generation;
    let project_snapshot =
        !projection_pending && endpoints.active_id() == endpoint_id && connection.surface_active;
    let (composed, resize, graphics_cleanup) = if let Some(shell) = &mut state.shell {
        let waits_for_selected_surface = projection_pending
            || (endpoints.active_id() == endpoint_id
                && !project_snapshot
                && shell.has_presented_surface());
        let previous_size = shell.surface_size(state.reported_size.0, state.reported_size.1);
        if !waits_for_selected_surface {
            shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Online);
        }
        if project_snapshot {
            shell.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        } else {
            shell.cache_endpoint_snapshot_inactive_for_generation(
                endpoint_id,
                generation,
                snapshot,
            );
        }
        let graphics_cleanup = shell.take_pending_graphics_cleanup();
        let next_size = shell.surface_size(state.reported_size.0, state.reported_size.1);
        (
            shell.compose(state.reported_size.0, state.reported_size.1),
            (previous_size != next_size).then(|| {
                client_shell_resize_message(
                    shell,
                    state.reported_size.0,
                    state.reported_size.1,
                    state.reported_cell_size.0,
                    state.reported_cell_size.1,
                    state.pixel_geometry_exact,
                )
            }),
            graphics_cleanup,
        )
    } else {
        (None, None, Vec::new())
    };
    apply_client_shell_input_source_changes(state, prefix_input_source);
    state.present_graphics(&graphics_cleanup);
    if let Some(resize) = resize {
        endpoints.send_to(endpoint_id, &resize);
    }
    if let Some(frame) = composed {
        if projection_pending {
            state.present_frame(frame);
        } else {
            state.present_frozen_chrome(frame);
        }
    } else {
        state.flush_pending_graphics();
    }
    Ok(())
}

/// 退出 / detach 前把去抖中的 chrome 偏好写掉：否则最后 500 ms 内的布局
/// 变更（Layout 模式按键、拖动分隔线）会丢失。
pub(super) fn flush_client_chrome_preferences(state: &mut ClientState) {
    if let Some(shell) = state.shell.as_mut() {
        shell.flush_chrome_preferences(&mut shell::ClientShellInput::default());
    }
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    frame: Option<FrameData>,
    endpoints: &mut endpoint::EndpointRegistry,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Result<bool, ClientError> {
    apply_client_shell_input_source_changes(state, prefix_input_source);
    if outcome.detach {
        flush_client_chrome_preferences(state);
        let _ = write_to_server(endpoints, &ClientMessage::Detach);
        return Ok(true);
    }
    if outcome.resize {
        let shell = state.shell.as_ref().expect("shell mode remains active");
        let resize = client_shell_resize_message(
            shell,
            state.reported_size.0,
            state.reported_size.1,
            state.reported_cell_size.0,
            state.reported_cell_size.1,
            state.pixel_geometry_exact,
        );
        if let Some(activation) = pending_activation.as_mut() {
            if let Err(error) = activation.update_resize(resize, endpoints) {
                rollback_endpoint_activation(state, endpoints, pending_activation, error, false);
            }
        } else {
            let _ = write_to_server(endpoints, &resize);
        }
    }
    #[cfg(not(windows))]
    if outcome.query_host_appearance {
        query_host_terminal_appearance();
    }
    if outcome.query_host_theme {
        query_host_terminal_theme();
    }
    sync_client_shell_keyboard_report_all(state)?;
    let (replay, dispatch_repaint) = dispatch_client_shell_actions(
        outcome.actions,
        endpoint_commands,
        endpoints,
        state.shell.as_mut(),
        &mut state.detached_process_children,
        event_tx,
    )?;
    let frame = if dispatch_repaint {
        state
            .shell
            .as_mut()
            .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    } else {
        frame
    };
    debug_assert!(
        replay.is_empty(),
        "mouse replay only follows endpoint results"
    );
    let active_endpoint_online = state
        .shell
        .as_ref()
        .is_none_or(|shell| shell.endpoint_is_online(endpoints.active_id()))
        && endpoints.active_surface_available();
    for request in outcome.requests {
        let request = if let Some(shell) = &state.shell {
            let Some(request) = shell.view_request(request) else {
                continue;
            };
            request
        } else {
            request
        };
        if let ClientMessage::ClientShellHostTheme { update } = &request {
            state.record_host_theme_update(update);
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_theme(update.clone(), endpoints) {
                    rollback_endpoint_activation(
                        state,
                        endpoints,
                        pending_activation,
                        error,
                        false,
                    );
                }
                continue;
            }
        }
        // Host focus belongs to a pending target even when the source has gone offline or has
        // already had its surface revoked. Route it before the ordinary source-online gate.
        if let ClientMessage::ClientShellFocus { focused } = request {
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_focus(focused, endpoints) {
                    rollback_endpoint_activation(
                        state,
                        endpoints,
                        pending_activation,
                        error,
                        false,
                    );
                }
                continue;
            }
            if active_endpoint_online {
                write_to_server(endpoints, &ClientMessage::ClientShellFocus { focused })
                    .map_err(ClientError::ConnectionLost)?;
            }
            continue;
        }
        if !active_endpoint_online {
            continue;
        }
        if pending_activation.is_some() {
            // Pane input and non-focus host effects do not cross the frozen handoff boundary.
            continue;
        }
        write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
    }
    if let Some(frame) = frame {
        if pending_activation.is_some() {
            state.present_frame(frame);
        } else {
            state.present_frozen_chrome(frame);
        }
    }
    Ok(false)
}
