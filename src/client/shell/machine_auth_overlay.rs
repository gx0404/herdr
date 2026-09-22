//! Machine connection recovery dialogs: the host-key trust (TOFU)
//! confirmation, the host-key-changed hard blocker, the authentication
//! guide, and the askpass password prompt that services approved interactive
//! authentication. Everything here is approval-gated — no interactive SSH
//! path runs until the user explicitly chooses it — and secrets only ever
//! live in the password editor before travelling straight to the askpass
//! responder; they are never logged, persisted, snapshotted, or rendered.

use super::*;
use crate::client::endpoint::{ProfileId, SavedSshEndpoint};

use super::render::{
    modal_button, modal_button_row, modal_panel, put_text, render_key_hints, OverlayRender,
};

/// Preferred key to display when several were scanned: strongest commonly
/// preferred type first (mirrors the remote layer's own presentation rank).
fn preferred_scanned_key(keys: &[(String, String)]) -> Option<(String, String)> {
    fn rank(key_type: &str) -> u8 {
        match key_type {
            "ssh-ed25519" => 0,
            _ if key_type.starts_with("ecdsa-") => 1,
            "ssh-rsa" => 2,
            _ => 3,
        }
    }
    keys.iter()
        .min_by_key(|(key_type, _)| rank(key_type))
        .cloned()
}

#[derive(Debug)]
pub(super) struct ClientMachineAuthOverlay {
    pub(super) scroll: u16,
    cancel: crate::remote::TaskCancellation,
    return_to: Option<Box<super::machines_overlay::ClientMachinesOverlay>>,
    /// `None` only transiently while a view is being moved between states;
    /// rendering tolerates it defensively.
    pub(super) view: Option<ClientMachineAuthView>,
}

impl Drop for ClientMachineAuthOverlay {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl ClientMachineAuthOverlay {
    fn new(view: ClientMachineAuthView) -> Self {
        Self {
            view: Some(view),
            scroll: 0,
            cancel: crate::remote::TaskCancellation::default(),
            return_to: None,
        }
    }

    /// A worker (known_hosts op or interactive auth) is in flight: drives
    /// the spinner and locks the action buttons.
    pub(super) fn auth_work_running(&self) -> bool {
        match self.view.as_ref() {
            Some(ClientMachineAuthView::HostKeyUnknown(view))
            | Some(ClientMachineAuthView::HostKeyChanged(view)) => view.busy,
            Some(ClientMachineAuthView::AuthGuide(view)) => view.busy,
            // The password dialog waits on the human, not on a worker.
            _ => false,
        }
    }

    /// Ticket of the interactive auth session this overlay belongs to, when
    /// the current view can receive askpass prompts for it.
    fn interactive_ticket(&self) -> Option<u64> {
        match self.view.as_ref() {
            Some(ClientMachineAuthView::AuthGuide(view)) if view.busy && !view.verified => {
                Some(view.ticket)
            }
            Some(ClientMachineAuthView::Password(view)) => Some(view.ticket),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) enum ClientMachineAuthView {
    HostKeyUnknown(ClientHostKeyView),
    HostKeyChanged(ClientHostKeyView),
    AuthGuide(ClientAuthGuideView),
    Password(ClientPasswordView),
}

#[derive(Debug)]
pub(super) struct ClientHostKeyView {
    pub(super) profile: Box<SavedSshEndpoint>,
    pub(super) reviewed: Option<(
        crate::remote::EffectiveHostKeyTarget,
        crate::remote::KnownHostKey,
    )>,
    /// Saved profile this recovery applies to; `None` on the wizard path
    /// (the machine is not persisted yet, so reconnect-style follow-ups and
    /// the process-local trust-once override are unavailable).
    pub(super) profile_id: Option<ProfileId>,
    /// `host` or `host:port` for display.
    pub(super) host_display: String,
    pub(super) host: String,
    pub(super) port: Option<u16>,
    pub(super) fingerprint: Option<(String, String)>,
    pub(super) ticket: u64,
    pub(super) busy: bool,
    pub(super) error: Option<String>,
    pub(super) message: Option<String>,
    /// A trust/remove operation finished successfully; the dialog only
    /// offers to close now.
    pub(super) completed: bool,
}

#[derive(Debug)]
pub(super) struct ClientAuthGuideView {
    pub(super) pin: Option<(
        crate::remote::EffectiveHostKeyTarget,
        crate::remote::KnownHostKey,
    )>,
    /// Profile the interactive attempt runs against; a throwaway profile on
    /// the wizard path (never persisted).
    pub(super) profile: Box<SavedSshEndpoint>,
    /// Set when the profile is saved, enabling reconnect follow-ups.
    pub(super) profile_id: Option<ProfileId>,
    /// Wizard path: success means "run the setup again", not "reconnect".
    pub(super) wizard: bool,
    pub(super) methods: Vec<String>,
    pub(super) identity_file: Option<String>,
    pub(super) ticket: u64,
    pub(super) busy: bool,
    pub(super) verified: bool,
    pub(super) error: Option<String>,
    pub(super) message: Option<String>,
}

#[derive(Debug)]
pub(super) struct ClientPasswordView {
    /// Guide state restored when the prompt is answered or cancelled.
    pub(super) guide: Box<ClientAuthGuideView>,
    pub(super) ticket: u64,
    pub(super) prompt: String,
    pub(super) input: TextEditor,
    pub(super) is_passphrase: bool,
    pub(super) is_host_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MachineAuthButton {
    TrustRemember,
    TrustOnce,
    Abort,
    RemoveRetry,
    InteractiveAuth,
    Precollect,
    CopyFix,
    CopySshAdd,
    PasswordSubmit,
    PasswordCancel,
    Close,
}

fn parse_host_port(target: &str) -> (String, Option<u16>) {
    crate::remote::parse_ssh_host_port(target).unwrap_or_else(|| (target.to_owned(), None))
}

fn host_display(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

impl ClientShellState {
    fn replace_machine_auth_overlay(&mut self, mut next: ClientMachineAuthOverlay) {
        next.return_to = match self.overlay.take() {
            Some(ClientShellOverlay::Machines(page)) => Some(Box::new(page)),
            Some(ClientShellOverlay::MachineAuth(mut previous)) => previous.return_to.take(),
            _ => None,
        };
        self.overlay = Some(ClientShellOverlay::MachineAuth(next));
    }

    fn return_from_machine_auth(&mut self) {
        if let Some(ClientShellOverlay::MachineAuth(mut auth)) = self.overlay.take() {
            self.overlay = auth
                .return_to
                .take()
                .map(|page| ClientShellOverlay::Machines(*page));
        }
    }

    fn next_machine_auth_ticket(&mut self) -> u64 {
        let ticket = self.next_machine_auth_ticket;
        self.next_machine_auth_ticket = self.next_machine_auth_ticket.saturating_add(1);
        ticket
    }

    /// Opens the recovery dialog matching the endpoint's recorded failure
    /// kind. Returns false when the kind has no dedicated dialog.
    pub(super) fn open_machine_auth_for_endpoint(
        &mut self,
        profile_id: &ProfileId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let endpoint_id = ClientEndpointId::Ssh(profile_id.clone());
        let Some(kind) = self.endpoint_connection_error_kind(&endpoint_id).cloned() else {
            return false;
        };
        let Some(profile) = self
            .saved_profiles
            .iter()
            .find(|profile| &profile.id == profile_id)
            .cloned()
        else {
            return false;
        };
        let ticket = self.next_machine_auth_ticket();
        let (host, port) = parse_host_port(&profile.target);
        let port = profile.port.or(port);
        match kind {
            crate::remote::ConnectionErrorKind::HostKeyUnknown { fingerprint } => {
                let fingerprint =
                    fingerprint.map(|fingerprint| (fingerprint.key_type, fingerprint.fingerprint));
                self.open_host_key_unknown_view(
                    Some(profile_id.clone()),
                    Box::new(profile.clone()),
                    host,
                    port,
                    fingerprint,
                    ticket,
                    outcome,
                );
                true
            }
            crate::remote::ConnectionErrorKind::HostKeyChanged => {
                self.replace_machine_auth_overlay(ClientMachineAuthOverlay::new(
                    ClientMachineAuthView::HostKeyChanged(ClientHostKeyView {
                        profile_id: Some(profile_id.clone()),
                        profile: Box::new(profile.clone()),
                        reviewed: None,
                        host_display: host_display(&host, port),
                        host,
                        port,
                        fingerprint: None,
                        ticket,
                        busy: false,
                        error: None,
                        message: None,
                        completed: false,
                    }),
                ));
                outcome.repaint = true;
                true
            }
            crate::remote::ConnectionErrorKind::AuthRequired {
                methods,
                identity_file,
            } => {
                self.open_auth_guide_view(
                    profile,
                    Some(profile_id.clone()),
                    methods,
                    identity_file,
                    ticket,
                    outcome,
                );
                true
            }
            _ => false,
        }
    }

    /// Wizard entry after a failed setup: confirm the presented host key
    /// before retrying (the fingerprint is scanned for display first).
    pub(super) fn open_machine_host_key_review(
        &mut self,
        profile: Box<SavedSshEndpoint>,
        outcome: &mut ClientShellInput,
    ) {
        let ticket = self.next_machine_auth_ticket();
        let (host, port) = parse_host_port(&profile.target);
        let port = profile.port.or(port);
        let id = self
            .saved_profiles
            .iter()
            .any(|current| current.id == profile.id)
            .then(|| profile.id.clone());
        self.open_host_key_unknown_view(id, profile, host, port, None, ticket, outcome);
    }

    /// 表单测试连接报「主机密钥已变化」：打开硬阻断的变更对话框，只提供清掉
    /// 旧密钥后重试（临时档案未落盘，没有重连可做）。
    pub(super) fn open_machine_host_key_changed_review(
        &mut self,
        profile: Box<SavedSshEndpoint>,
        outcome: &mut ClientShellInput,
    ) {
        let ticket = self.next_machine_auth_ticket();
        let (host, port) = parse_host_port(&profile.target);
        let port = profile.port.or(port);
        self.replace_machine_auth_overlay(ClientMachineAuthOverlay::new(
            ClientMachineAuthView::HostKeyChanged(ClientHostKeyView {
                profile_id: None,
                profile,
                reviewed: None,
                host_display: host_display(&host, port),
                host,
                port,
                fingerprint: None,
                ticket,
                busy: false,
                error: None,
                message: None,
                completed: false,
            }),
        ));
        outcome.repaint = true;
    }

    /// Wizard entry after a failed setup: guide an approved interactive
    /// authentication attempt against the throwaway profile.
    pub(super) fn open_machine_auth_guide(
        &mut self,
        profile: Box<SavedSshEndpoint>,
        outcome: &mut ClientShellInput,
    ) {
        let ticket = self.next_machine_auth_ticket();
        let identity_file = profile.identity_file.first().cloned();
        self.open_auth_guide_view(*profile, None, Vec::new(), identity_file, ticket, outcome);
    }

    fn open_auth_guide_view(
        &mut self,
        profile: SavedSshEndpoint,
        profile_id: Option<ProfileId>,
        methods: Vec<String>,
        identity_file: Option<String>,
        ticket: u64,
        outcome: &mut ClientShellInput,
    ) {
        self.replace_machine_auth_overlay(ClientMachineAuthOverlay::new(
            ClientMachineAuthView::AuthGuide(ClientAuthGuideView {
                pin: None,
                profile: Box::new(profile),
                profile_id: profile_id.clone(),
                wizard: profile_id.is_none(),
                methods,
                identity_file,
                ticket,
                busy: false,
                verified: false,
                error: None,
                message: None,
            }),
        ));
        outcome.repaint = true;
    }

    fn open_host_key_unknown_view(
        &mut self,
        profile_id: Option<ProfileId>,
        profile: Box<SavedSshEndpoint>,
        host: String,
        port: Option<u16>,
        fingerprint: Option<(String, String)>,
        ticket: u64,
        outcome: &mut ClientShellInput,
    ) {
        // Without a fingerprint from the failure classification, scan the
        // presented key for display first: trust decisions need the
        // fingerprint on screen.
        let busy = true;
        self.replace_machine_auth_overlay(ClientMachineAuthOverlay::new(
            ClientMachineAuthView::HostKeyUnknown(ClientHostKeyView {
                profile: profile.clone(),
                reviewed: None,
                profile_id,
                host_display: host_display(&host, port),
                host: host.clone(),
                port,
                fingerprint,
                ticket,
                busy,
                error: None,
                message: None,
                completed: false,
            }),
        ));
        if busy {
            outcome.actions.push(ClientShellAction::MachineHostKeyOp {
                cancel: match self.overlay.as_ref() {
                    Some(ClientShellOverlay::MachineAuth(auth)) => auth.cancel.clone(),
                    _ => return,
                },
                ticket,
                op: MachineHostKeyOp::Scan,
                profile,
                reviewed: None,
            });
        }
        outcome.repaint = true;
    }

    /// A new askpass prompt arrived for a running interactive auth session.
    /// Returns false when no live dialog claims the ticket (the caller then
    /// declines the prompt).
    pub(crate) fn show_machine_auth_prompt(&mut self, ticket: u64, prompt: &str) -> bool {
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        if overlay.interactive_ticket() != Some(ticket) {
            return false;
        }
        let lowered = prompt.to_ascii_lowercase();
        let is_passphrase = lowered.contains("passphrase");
        let is_host_key = lowered.contains("yes/no") && lowered.contains("fingerprint");
        overlay.scroll = 0;
        let Some(view) = overlay.view.take() else {
            return false;
        };
        match view {
            ClientMachineAuthView::AuthGuide(guide) => {
                overlay.view = Some(ClientMachineAuthView::Password(ClientPasswordView {
                    guide: Box::new(guide),
                    ticket,
                    prompt: prompt.to_owned(),
                    input: TextEditor::default(),
                    is_passphrase,
                    is_host_key,
                }));
                true
            }
            ClientMachineAuthView::Password(mut password) => {
                password.prompt = prompt.to_owned();
                password.is_passphrase = is_passphrase;
                password.is_host_key = is_host_key;
                password.input.clear();
                overlay.view = Some(ClientMachineAuthView::Password(password));
                true
            }
            other => {
                overlay.view = Some(other);
                false
            }
        }
    }

    /// Progress from a recovery worker (known_hosts op or interactive auth).
    pub(crate) fn handle_machine_auth_update(
        &mut self,
        update: MachineAuthUpdate,
        outcome: &mut ClientShellInput,
    ) {
        match update {
            MachineAuthUpdate::InteractiveStep { ticket, step } => {
                if let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() {
                    let guide = match overlay.view.as_mut() {
                        Some(ClientMachineAuthView::AuthGuide(guide)) => Some(guide),
                        Some(ClientMachineAuthView::Password(password)) => {
                            Some(password.guide.as_mut())
                        }
                        _ => None,
                    };
                    if let Some(guide) = guide.filter(|guide| guide.ticket == ticket && guide.busy)
                    {
                        guide.message =
                            Some(super::machines_overlay::bootstrap_step_label(step).into());
                        outcome.repaint = true;
                    }
                }
            }
            MachineAuthUpdate::HostKeyOpFinished { ticket, op, result } => {
                self.handle_host_key_op_finished(ticket, op, result, outcome)
            }
            MachineAuthUpdate::InteractiveFinished { ticket, result } => {
                self.handle_interactive_auth_finished(ticket, result, outcome)
            }
        }
    }

    fn handle_host_key_op_finished(
        &mut self,
        ticket: u64,
        op: MachineHostKeyOp,
        result: Result<MachineHostKeyOutcome, String>,
        outcome: &mut ClientShellInput,
    ) {
        let t = &crate::i18n::texts().machine_auth;
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match overlay.view.as_mut() {
            Some(ClientMachineAuthView::HostKeyUnknown(view))
            | Some(ClientMachineAuthView::HostKeyChanged(view))
                if view.ticket == ticket =>
            {
                view.busy = false;
                match (op, result) {
                    (MachineHostKeyOp::Scan, Ok(MachineHostKeyOutcome::Scanned(review))) => {
                        let fingerprints = review
                            .keys
                            .iter()
                            .map(|key| (key.key_type.clone(), key.fingerprint.fingerprint.clone()))
                            .collect::<Vec<_>>();
                        view.fingerprint = preferred_scanned_key(&fingerprints);
                        view.reviewed = view
                            .fingerprint
                            .as_ref()
                            .and_then(|(kind, fingerprint)| {
                                review.keys.iter().find(|key| {
                                    &key.key_type == kind
                                        && &key.fingerprint.fingerprint == fingerprint
                                })
                            })
                            .map(|key| (review.target.clone(), key.clone()));
                        view.host = review.target.host.clone();
                        view.port = Some(review.target.port);
                        view.host_display =
                            host_display(&review.target.host, Some(review.target.port));
                        if view.fingerprint.is_none() {
                            view.error = Some(t.fingerprint_unavailable.to_owned());
                        }
                    }
                    (
                        MachineHostKeyOp::Precollect,
                        Ok(MachineHostKeyOutcome::Precollected(count)),
                    ) => {
                        view.completed = true;
                        view.message = Some(crate::i18n::fill(
                            t.trusted_fmt,
                            &[("count", count.to_string().as_str())],
                        ));
                        if let Some(profile_id) = view.profile_id.clone() {
                            outcome.actions.push(ClientShellAction::ReconnectEndpoint {
                                endpoint_id: ClientEndpointId::Ssh(profile_id),
                            });
                        }
                    }
                    (MachineHostKeyOp::Remove, Ok(MachineHostKeyOutcome::Removed)) => {
                        view.completed = true;
                        view.message = Some(t.removed.to_owned());
                        if let Some(profile_id) = view.profile_id.clone() {
                            outcome.actions.push(ClientShellAction::ReconnectEndpoint {
                                endpoint_id: ClientEndpointId::Ssh(profile_id),
                            });
                        }
                    }
                    (_, Ok(_)) => {}
                    (_, Err(error)) => {
                        view.error = Some(crate::i18n::fill(t.failed_fmt, &[("error", &error)]));
                    }
                }
                outcome.repaint = true;
            }
            Some(ClientMachineAuthView::AuthGuide(guide)) if guide.ticket == ticket => {
                guide.busy = false;
                match result {
                    Ok(MachineHostKeyOutcome::Precollected(count)) => {
                        guide.message = Some(crate::i18n::fill(
                            t.precollected_fmt,
                            &[("count", count.to_string().as_str())],
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        guide.error = Some(crate::i18n::fill(t.failed_fmt, &[("error", &error)]));
                    }
                }
                outcome.repaint = true;
            }
            _ => {}
        }
    }

    pub(crate) fn complete_interactive_connection(
        &mut self,
        ticket: u64,
    ) -> Option<SavedSshEndpoint> {
        let (profile, wizard) = match self.overlay.as_ref() {
            Some(ClientShellOverlay::MachineAuth(overlay)) if !overlay.cancel.is_cancelled() => {
                match overlay.view.as_ref() {
                    Some(ClientMachineAuthView::AuthGuide(guide))
                        if guide.ticket == ticket && guide.busy =>
                    {
                        ((*guide.profile).clone(), guide.wizard)
                    }
                    Some(ClientMachineAuthView::Password(password))
                        if password.ticket == ticket =>
                    {
                        ((*password.guide.profile).clone(), password.guide.wizard)
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        let profile = if wizard {
            let id = match self.persist_authenticated_wizard(&profile) {
                Ok(id) => id,
                Err(error) => {
                    self.set_endpoint_error(error);
                    return None;
                }
            };
            self.saved_profiles
                .iter()
                .find(|profile| profile.id == id)?
                .clone()
        } else {
            let current = self
                .saved_profiles
                .iter()
                .find(|current| current.id == profile.id && current.enabled)?;
            if !current.same_connection(&profile) {
                self.set_endpoint_error("连接设置已变化，请重新连接");
                return None;
            }
            current.clone()
        };
        self.return_from_machine_auth();
        self.open_machines_overlay_for(&profile.id);
        Some(profile)
    }

    fn handle_interactive_auth_finished(
        &mut self,
        ticket: u64,
        result: Result<(), String>,
        outcome: &mut ClientShellInput,
    ) {
        let t = &crate::i18n::texts().machine_auth;
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if overlay.cancel.is_cancelled() {
            return;
        }
        // Restore the guide view if the dialog still sits on the password
        // prompt (ssh gave up before an answer arrived).
        if matches!(overlay.view.as_ref(), Some(ClientMachineAuthView::Password(view)) if view.ticket == ticket)
        {
            if let Some(ClientMachineAuthView::Password(view)) = overlay.view.take() {
                overlay.view = Some(ClientMachineAuthView::AuthGuide(*view.guide));
            }
        }
        let Some(ClientMachineAuthView::AuthGuide(guide)) = overlay.view.as_mut() else {
            return;
        };
        if guide.ticket != ticket {
            return;
        }
        guide.busy = false;
        match result {
            Ok(()) => {
                guide.verified = true;
                guide.error = None;
                guide.message = Some(
                    if guide.wizard {
                        t.auth_success_wizard
                    } else {
                        t.auth_success
                    }
                    .to_owned(),
                );
                if !guide.wizard {
                    if let Some(profile_id) = guide.profile_id.clone() {
                        outcome.actions.push(ClientShellAction::ReconnectEndpoint {
                            endpoint_id: ClientEndpointId::Ssh(profile_id),
                        });
                    }
                }
            }
            Err(error) => {
                guide.error = Some(crate::i18n::fill(t.auth_failed_fmt, &[("error", &error)]));
            }
        }
        outcome.repaint = true;
    }

    fn start_interactive_auth(&mut self, outcome: &mut ClientShellInput) {
        let ticket = self.next_machine_auth_ticket();
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(ClientMachineAuthView::AuthGuide(guide)) = overlay.view.as_mut() else {
            return;
        };
        if guide.busy {
            return;
        }
        if overlay.cancel.is_cancelled() || guide.error.is_some() {
            guide.ticket = ticket;
        }
        overlay.cancel = crate::remote::TaskCancellation::default();
        guide.busy = true;
        guide.error = None;
        guide.message = None;
        outcome
            .actions
            .push(ClientShellAction::StartMachineInteractiveAuth {
                bootstrap: guide.wizard,
                pin: guide.pin.clone(),
                cancel: overlay.cancel.clone(),
                ticket: guide.ticket,
                profile: guide.profile.clone(),
            });
        outcome.repaint = true;
    }

    fn start_guide_host_key_op(&mut self, _op: MachineHostKeyOp, outcome: &mut ClientShellInput) {
        let profile = match self.overlay.as_ref() {
            Some(ClientShellOverlay::MachineAuth(overlay)) => match overlay.view.as_ref() {
                Some(ClientMachineAuthView::AuthGuide(guide)) if !guide.busy => {
                    Some(guide.profile.clone())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(profile) = profile {
            self.open_machine_host_key_review(profile, outcome);
        }
    }

    /// Moves the password view back to its guide and returns the session
    /// ticket; `None` when the dialog is not on the password prompt.
    fn fold_password_into_guide(&mut self) -> Option<u64> {
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return None;
        };
        if !matches!(
            overlay.view.as_ref(),
            Some(ClientMachineAuthView::Password(_))
        ) {
            return None;
        }
        let Some(ClientMachineAuthView::Password(view)) = overlay.view.take() else {
            return None;
        };
        let ticket = view.ticket;
        overlay.view = Some(ClientMachineAuthView::AuthGuide(*view.guide));
        Some(ticket)
    }

    fn submit_machine_auth_password(&mut self, outcome: &mut ClientShellInput) {
        let answer = match self.overlay.as_mut() {
            Some(ClientShellOverlay::MachineAuth(overlay)) => match overlay.view.as_mut() {
                Some(ClientMachineAuthView::Password(view)) => {
                    let answer = if view.is_host_key {
                        "yes".to_owned()
                    } else {
                        view.input.as_str().to_owned()
                    };
                    view.input.clear();
                    Some(answer)
                }
                _ => None,
            },
            _ => None,
        };
        let (Some(answer), Some(ticket)) = (answer, self.fold_password_into_guide()) else {
            return;
        };
        outcome
            .actions
            .push(ClientShellAction::AnswerMachineAuthPrompt {
                ticket,
                answer: Some(answer),
            });
        outcome.repaint = true;
    }

    fn decline_machine_auth_password(&mut self, outcome: &mut ClientShellInput) {
        if let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() {
            if let Some(ClientMachineAuthView::Password(view)) = overlay.view.as_mut() {
                view.input.clear();
            }
        }
        let Some(ticket) = self.fold_password_into_guide() else {
            return;
        };
        if let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() {
            overlay.cancel.cancel();
            if let Some(ClientMachineAuthView::AuthGuide(guide)) = overlay.view.as_mut() {
                guide.busy = false;
            }
        }
        outcome
            .actions
            .push(ClientShellAction::AnswerMachineAuthPrompt {
                ticket,
                answer: None,
            });
        outcome
            .actions
            .push(ClientShellAction::CancelMachineInteractiveAuth { ticket });
        outcome.repaint = true;
    }

    fn cancel_machine_interactive_auth(&mut self, outcome: &mut ClientShellInput) {
        let busy_ticket = match self.overlay.as_ref() {
            Some(ClientShellOverlay::MachineAuth(overlay)) => match overlay.view.as_ref() {
                Some(ClientMachineAuthView::AuthGuide(guide)) if guide.busy && !guide.verified => {
                    Some(guide.ticket)
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(ticket) = busy_ticket {
            outcome
                .actions
                .push(ClientShellAction::CancelMachineInteractiveAuth { ticket });
        }
        self.return_from_machine_auth();
        outcome.repaint = true;
    }

    fn copy_machine_fix_command(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(ClientMachineAuthView::AuthGuide(guide)) = overlay.view.as_mut() else {
            return;
        };
        let command = crate::remote::saved_ssh_bootstrap_command(
            &guide.profile.target,
            &guide.profile.session,
        );
        outcome
            .actions
            .push(ClientShellAction::ClipboardWrite(command.into_bytes()));
        guide.message = Some(crate::i18n::texts().machine_auth.auth_copied_fix.to_owned());
        outcome.repaint = true;
    }

    fn copy_machine_ssh_add_command(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(ClientMachineAuthView::AuthGuide(guide)) = overlay.view.as_mut() else {
            return;
        };
        let Some(identity_file) = guide.identity_file.clone() else {
            return;
        };
        outcome.actions.push(ClientShellAction::ClipboardWrite(
            format!("ssh-add {identity_file}").into_bytes(),
        ));
        guide.message = Some(crate::i18n::texts().machine_auth.copied_ssh_add.to_owned());
        outcome.repaint = true;
    }

    fn start_host_key_op(&mut self, op: MachineHostKeyOp, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(view) = (match overlay.view.as_mut() {
            Some(ClientMachineAuthView::HostKeyUnknown(view))
            | Some(ClientMachineAuthView::HostKeyChanged(view)) => Some(view),
            _ => None,
        }) else {
            return;
        };
        if view.busy || view.completed {
            return;
        }
        if op == MachineHostKeyOp::Precollect && view.reviewed.is_none() {
            view.error = Some(
                crate::i18n::texts()
                    .machine_auth
                    .fingerprint_unavailable
                    .into(),
            );
            outcome.repaint = true;
            return;
        }
        view.busy = true;
        view.error = None;
        view.message = None;
        outcome.actions.push(ClientShellAction::MachineHostKeyOp {
            cancel: overlay.cancel.clone(),
            ticket: view.ticket,
            op,
            profile: view.profile.clone(),
            reviewed: view.reviewed.clone(),
        });
        outcome.repaint = true;
    }

    fn trust_host_key_once(&mut self, outcome: &mut ClientShellInput) {
        let prepared = match self.overlay.as_ref() {
            Some(ClientShellOverlay::MachineAuth(overlay)) => match overlay.view.as_ref() {
                Some(ClientMachineAuthView::HostKeyUnknown(view))
                    if !view.busy && !view.completed =>
                {
                    view.reviewed
                        .clone()
                        .zip(view.profile_id.clone())
                        .map(|(pin, id)| ((*view.profile).clone(), pin, id))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((profile, pin, id)) = prepared else {
            return;
        };
        let identity = profile.identity_file.first().cloned();
        let ticket = self.next_machine_auth_ticket();
        self.open_auth_guide_view(profile, Some(id), Vec::new(), identity, ticket, outcome);
        if let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() {
            if let Some(ClientMachineAuthView::AuthGuide(guide)) = overlay.view.as_mut() {
                guide.pin = Some(pin);
            }
        }
        self.start_interactive_auth(outcome);
    }

    /// Mouse activation for one rendered machine-auth dialog button.
    pub(super) fn activate_machine_auth_button(
        &mut self,
        button: MachineAuthButton,
        outcome: &mut ClientShellInput,
    ) {
        match button {
            MachineAuthButton::TrustRemember => {
                self.start_host_key_op(MachineHostKeyOp::Precollect, outcome)
            }
            MachineAuthButton::TrustOnce => self.trust_host_key_once(outcome),
            MachineAuthButton::Abort => {
                self.return_from_machine_auth();
                outcome.repaint = true;
            }
            MachineAuthButton::RemoveRetry => {
                self.start_host_key_op(MachineHostKeyOp::Remove, outcome)
            }
            MachineAuthButton::InteractiveAuth => {
                let prepared = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::MachineAuth(overlay)) => match overlay.view.as_ref() {
                        Some(ClientMachineAuthView::HostKeyUnknown(view)) if !view.busy => {
                            Some(((*view.profile).clone(), view.profile_id.clone()))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((profile, id)) = prepared {
                    let identity = profile.identity_file.first().cloned();
                    let ticket = self.next_machine_auth_ticket();
                    self.open_auth_guide_view(profile, id, Vec::new(), identity, ticket, outcome);
                }
                self.start_interactive_auth(outcome);
            }
            MachineAuthButton::Precollect => {
                self.start_guide_host_key_op(MachineHostKeyOp::Precollect, outcome)
            }
            MachineAuthButton::CopyFix => self.copy_machine_fix_command(outcome),
            MachineAuthButton::CopySshAdd => self.copy_machine_ssh_add_command(outcome),
            MachineAuthButton::PasswordSubmit => self.submit_machine_auth_password(outcome),
            MachineAuthButton::PasswordCancel => self.decline_machine_auth_password(outcome),
            MachineAuthButton::Close => self.cancel_machine_interactive_auth(outcome),
        }
    }

    pub(super) fn insert_machine_auth_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        match overlay.view.as_mut() {
            Some(ClientMachineAuthView::Password(view)) => {
                if view.is_host_key {
                    true
                } else {
                    view.input.insert(text)
                }
            }
            _ => false,
        }
    }

    pub(super) fn scroll_machine_auth(&mut self, delta: i16) {
        if let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_mut() {
            overlay.scroll = overlay
                .scroll
                .saturating_add_signed(delta)
                .min(self.hits.machine_auth_max_scroll.min(u16::MAX as usize) as u16);
        }
    }

    /// Keyboard routing for the machine auth dialogs.
    pub(super) fn route_machine_auth_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if matches!(self.overlay, Some(ClientShellOverlay::MachineAuth(_)))
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
        {
            self.scroll_machine_auth(if key.code == KeyCode::PageUp { -5 } else { 5 });
            outcome.repaint = true;
            return true;
        }
        enum ViewKind {
            HostKeyUnknown {
                locked: bool,
                once_available: bool,
            },
            HostKeyChanged {
                locked: bool,
            },
            AuthGuide {
                busy: bool,
                verified: bool,
                has_identity: bool,
            },
            Password,
        }
        let Some(ClientShellOverlay::MachineAuth(overlay)) = self.overlay.as_ref() else {
            return false;
        };
        let view = match overlay.view.as_ref() {
            Some(ClientMachineAuthView::HostKeyUnknown(view)) => ViewKind::HostKeyUnknown {
                locked: view.busy || view.completed,
                once_available: view.profile_id.is_some(),
            },
            Some(ClientMachineAuthView::HostKeyChanged(view)) => ViewKind::HostKeyChanged {
                locked: view.busy || view.completed,
            },
            Some(ClientMachineAuthView::AuthGuide(guide)) => ViewKind::AuthGuide {
                busy: guide.busy,
                verified: guide.verified,
                has_identity: guide.identity_file.is_some(),
            },
            Some(ClientMachineAuthView::Password(_)) => ViewKind::Password,
            None => return true,
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        match view {
            ViewKind::HostKeyUnknown {
                locked,
                once_available,
            } => {
                if code == KeyCode::Esc {
                    self.return_from_machine_auth();
                } else if !locked && plain {
                    match code {
                        KeyCode::Char('i') => self.activate_machine_auth_button(
                            MachineAuthButton::InteractiveAuth,
                            outcome,
                        ),
                        KeyCode::Char('t') => {
                            self.start_host_key_op(MachineHostKeyOp::Precollect, outcome);
                        }
                        KeyCode::Char('o') if once_available => self.trust_host_key_once(outcome),
                        _ => return true,
                    }
                }
                outcome.repaint = true;
                true
            }
            ViewKind::HostKeyChanged { locked } => {
                if code == KeyCode::Esc {
                    self.return_from_machine_auth();
                } else if !locked && plain && code == KeyCode::Char('r') {
                    self.start_host_key_op(MachineHostKeyOp::Remove, outcome);
                }
                outcome.repaint = true;
                true
            }
            ViewKind::AuthGuide {
                busy,
                verified,
                has_identity,
            } => {
                if code == KeyCode::Esc {
                    self.cancel_machine_interactive_auth(outcome);
                    return true;
                }
                if plain {
                    match code {
                        KeyCode::Char('i') if !busy && !verified => {
                            self.start_interactive_auth(outcome);
                        }
                        KeyCode::Char('p') if !busy && !verified => {
                            self.start_guide_host_key_op(MachineHostKeyOp::Precollect, outcome);
                        }
                        KeyCode::Char('c') if !busy => {
                            self.copy_machine_fix_command(outcome);
                        }
                        KeyCode::Char('a') if !busy && verified && has_identity => {
                            self.copy_machine_ssh_add_command(outcome);
                        }
                        _ => return true,
                    }
                }
                outcome.repaint = true;
                true
            }
            ViewKind::Password => {
                match code {
                    KeyCode::Enter => self.submit_machine_auth_password(outcome),
                    KeyCode::Esc => self.decline_machine_auth_password(outcome),
                    _ => {
                        if let Some(ClientShellOverlay::MachineAuth(overlay)) =
                            self.overlay.as_mut()
                        {
                            if let Some(ClientMachineAuthView::Password(view)) =
                                overlay.view.as_mut()
                            {
                                outcome.repaint |= view.input.handle_key(key).is_some();
                            }
                        }
                    }
                }
                true
            }
        }
    }
}

// ---------- rendering ----------

pub(super) fn render_machine_auth_overlay(
    b: &mut Buffer,
    overlay: &ClientMachineAuthOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    match overlay.view.as_ref() {
        Some(ClientMachineAuthView::HostKeyUnknown(view)) => render_host_key_unknown(b, view, cx),
        Some(ClientMachineAuthView::HostKeyChanged(view)) => render_host_key_changed(b, view, cx),
        Some(ClientMachineAuthView::AuthGuide(view)) => render_auth_guide(b, view, cx),
        Some(ClientMachineAuthView::Password(view)) => {
            render_password_prompt(b, view, overlay.scroll, cx)
        }
        None => None,
    }
}

fn put_line(b: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    put_text(b, x, y, width, &format!(" {text}"), style);
}

#[allow(clippy::too_many_arguments)]
fn status_line(
    b: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    busy: bool,
    error: Option<&str>,
    message: Option<&str>,
    base: Style,
    cx: &super::feedback::ChromeContext<'_>,
) {
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_auth;
    if busy {
        put_line(
            b,
            x,
            y,
            width,
            &format!("{} {}", cx.spinner, t.working),
            base.fg(p.yellow),
        );
    } else if let Some(error) = error {
        put_line(b, x, y, width, error, base.fg(p.red));
    } else if let Some(message) = message {
        put_line(b, x, y, width, message, base.fg(p.green));
    }
}

fn host_key_lines(view: &ClientHostKeyView) -> Vec<String> {
    let t = &crate::i18n::texts().machine_auth;
    let mut lines = vec![crate::i18n::fill(
        t.host_fmt,
        &[("host", &view.host_display)],
    )];
    match view.fingerprint.as_ref() {
        Some((key_type, fingerprint)) => {
            lines.push(crate::i18n::fill(t.key_type_fmt, &[("type", key_type)]));
            lines.push(crate::i18n::fill(
                t.fingerprint_fmt,
                &[("fingerprint", fingerprint)],
            ));
        }
        None if !view.busy => {
            lines.push(t.fingerprint_unavailable.to_owned());
        }
        None => {}
    }
    lines
}

fn render_host_key_unknown(
    b: &mut Buffer,
    view: &ClientHostKeyView,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_auth;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(12),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 6, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_line(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        t.tofu_title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        t.tofu_question,
        base.fg(p.text),
    );
    for (offset, line) in host_key_lines(view).into_iter().take(3).enumerate() {
        put_line(
            b,
            stack.header.x,
            stack.header.y + 2 + offset as u16,
            stack.header.width,
            &line,
            base.fg(p.teal),
        );
    }
    put_line(
        b,
        stack.header.x,
        stack.header.y + 5,
        stack.header.width,
        t.tofu_verify_hint,
        base.fg(p.overlay0),
    );
    status_line(
        b,
        stack.content.x,
        stack.content.y,
        stack.content.width,
        view.busy,
        view.error.as_deref(),
        view.message.as_deref(),
        base,
        cx,
    );
    if let Some(footer) = stack.footer {
        let mut hints = vec![("t".to_owned(), t.hint_trust.to_owned())];
        if view.profile_id.is_some() {
            hints.push(("o".to_owned(), t.hint_trust_once.to_owned()));
        }
        hints.push(("esc".to_owned(), t.hint_abort.to_owned()));
        render_key_hints(b, footer, &hints, p, cx.components);
    }

    let mut action_hits = Vec::new();
    let locked = view.busy || view.completed;
    let (labels, buttons): (Vec<&str>, Vec<MachineAuthButton>) = if view.completed {
        (vec![t.close_button], vec![MachineAuthButton::Abort])
    } else if view.reviewed.is_none() && !view.busy {
        (
            vec![t.auth_interactive_button, t.abort_button],
            vec![MachineAuthButton::InteractiveAuth, MachineAuthButton::Abort],
        )
    } else if view.profile_id.is_some() {
        (
            vec![t.trust_remember_button, t.trust_once_button, t.abort_button],
            vec![
                MachineAuthButton::TrustRemember,
                MachineAuthButton::TrustOnce,
                MachineAuthButton::Abort,
            ],
        )
    } else {
        (
            vec![t.trust_remember_button, t.abort_button],
            vec![MachineAuthButton::TrustRemember, MachineAuthButton::Abort],
        )
    };
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let (tone, base_state) = match button {
                MachineAuthButton::TrustRemember => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let lockable = button != MachineAuthButton::Abort;
            let state = if locked && lockable {
                crate::ui::ModalButtonState::Disabled
            } else {
                cx.button_state(
                    &super::feedback::ChromeHover::MachineAuthButton(button),
                    base_state,
                )
            };
            modal_button(b, *rect, labels[index], tone, state, p);
            if !(locked && lockable) {
                action_hits.push((*rect, button));
            }
        }
    }
    Some(OverlayRender {
        area: popup,
        machine_auth_actions: action_hits,
        ..OverlayRender::default()
    })
}

fn render_host_key_changed(
    b: &mut Buffer,
    view: &ClientHostKeyView,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_auth;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Medium.with_height(11), p.red, cx)?;
    if inner.width < 20 || inner.height < 7 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 5, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_line(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        t.changed_title,
        base.fg(p.red).add_modifier(Modifier::BOLD),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        t.changed_warning,
        base.fg(p.text),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 2,
        stack.header.width,
        t.changed_reinstall_hint,
        base.fg(p.overlay1),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 3,
        stack.header.width,
        t.changed_mitm_hint,
        base.fg(p.red),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 4,
        stack.header.width,
        &crate::i18n::fill(t.host_fmt, &[("host", &view.host_display)]),
        base.fg(p.teal),
    );
    status_line(
        b,
        stack.content.x,
        stack.content.y,
        stack.content.width,
        view.busy,
        view.error.as_deref(),
        view.message.as_deref(),
        base,
        cx,
    );
    if let Some(footer) = stack.footer {
        render_key_hints(
            b,
            footer,
            &[
                ("r".to_owned(), t.hint_remove_retry.to_owned()),
                ("esc".to_owned(), t.hint_abort.to_owned()),
            ],
            p,
            cx.components,
        );
    }

    let mut action_hits = Vec::new();
    let locked = view.busy || view.completed;
    let (labels, buttons): (Vec<&str>, Vec<MachineAuthButton>) = if view.completed {
        (vec![t.close_button], vec![MachineAuthButton::Abort])
    } else {
        (
            vec![t.remove_retry_button, t.abort_button],
            vec![MachineAuthButton::RemoveRetry, MachineAuthButton::Abort],
        )
    };
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let (tone, base_state) = match button {
                MachineAuthButton::RemoveRetry => (
                    crate::ui::ModalButtonTone::Danger,
                    crate::ui::ModalButtonState::Focused,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let lockable = button != MachineAuthButton::Abort;
            let state = if locked && lockable {
                crate::ui::ModalButtonState::Disabled
            } else {
                cx.button_state(
                    &super::feedback::ChromeHover::MachineAuthButton(button),
                    base_state,
                )
            };
            modal_button(b, *rect, labels[index], tone, state, p);
            if !(locked && lockable) {
                action_hits.push((*rect, button));
            }
        }
    }
    Some(OverlayRender {
        area: popup,
        machine_auth_actions: action_hits,
        ..OverlayRender::default()
    })
}

fn render_auth_guide(
    b: &mut Buffer,
    view: &ClientAuthGuideView,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_auth;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(12),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 5, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_line(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        t.auth_title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let methods = if view.methods.is_empty() {
        "–".to_owned()
    } else {
        view.methods.join(", ")
    };
    put_line(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &crate::i18n::fill(t.auth_methods_fmt, &[("methods", &methods)]),
        base.fg(p.text),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 2,
        stack.header.width,
        &match view.identity_file.as_deref() {
            Some(path) => crate::i18n::fill(t.auth_identity_fmt, &[("path", path)]),
            None => t.auth_no_identity.to_owned(),
        },
        base.fg(p.text),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 3,
        stack.header.width,
        t.auth_hint,
        base.fg(p.overlay1),
    );
    if view.identity_file.is_some() && !view.verified {
        put_line(
            b,
            stack.header.x,
            stack.header.y + 4,
            stack.header.width,
            t.passphrase_agent_hint,
            base.fg(p.overlay0),
        );
    }
    status_line(
        b,
        stack.content.x,
        stack.content.y,
        stack.content.width,
        view.busy,
        view.error.as_deref(),
        view.message.as_deref(),
        base,
        cx,
    );
    if let Some(footer) = stack.footer {
        render_key_hints(
            b,
            footer,
            &[
                ("i".to_owned(), t.hint_interactive.to_owned()),
                ("p".to_owned(), t.hint_precollect.to_owned()),
                ("c".to_owned(), t.hint_copy_fix.to_owned()),
                ("esc".to_owned(), t.hint_close.to_owned()),
            ],
            p,
            cx.components,
        );
    }

    let mut action_hits = Vec::new();
    let mut labels: Vec<&str> = Vec::new();
    let mut buttons: Vec<MachineAuthButton> = Vec::new();
    if view.verified {
        if view.identity_file.is_some() {
            labels.push(t.copy_ssh_add_button);
            buttons.push(MachineAuthButton::CopySshAdd);
        }
        labels.push(t.close_button);
        buttons.push(MachineAuthButton::Close);
    } else {
        labels.push(t.auth_interactive_button);
        buttons.push(MachineAuthButton::InteractiveAuth);
        labels.push(t.auth_precollect_button);
        buttons.push(MachineAuthButton::Precollect);
        labels.push(t.auth_copy_fix_button);
        buttons.push(MachineAuthButton::CopyFix);
        labels.push(t.close_button);
        buttons.push(MachineAuthButton::Close);
    }
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let (tone, base_state) = match button {
                MachineAuthButton::InteractiveAuth => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let lockable = matches!(
                button,
                MachineAuthButton::InteractiveAuth | MachineAuthButton::Precollect
            );
            let state = if view.busy && lockable {
                crate::ui::ModalButtonState::Disabled
            } else {
                cx.button_state(
                    &super::feedback::ChromeHover::MachineAuthButton(button),
                    base_state,
                )
            };
            modal_button(b, *rect, labels[index], tone, state, p);
            if !(view.busy && lockable) {
                action_hits.push((*rect, button));
            }
        }
    }
    Some(OverlayRender {
        area: popup,
        machine_auth_actions: action_hits,
        ..OverlayRender::default()
    })
}

fn render_ssh_host_prompt(
    b: &mut Buffer,
    view: &ClientPasswordView,
    scroll: u16,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_auth;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large, p.accent, cx)?;
    let layout = super::page::PageLayout::new(inner, 0, false, true);
    put_text(
        b,
        layout.header.x,
        layout.header.y,
        layout.header.width,
        t.tofu_title,
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    );
    let clean = view
        .prompt
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .collect::<String>();
    let lines = clean
        .lines()
        .map(|line| (line.width(), ratatui::text::Line::from(line.to_owned())))
        .collect::<Vec<_>>();
    let metrics = crate::ui::display_lines_scroll_metrics(&lines, scroll, layout.content);
    Paragraph::new(lines.into_iter().map(|(_, line)| line).collect::<Vec<_>>())
        .style(Style::default().fg(p.teal))
        .wrap(Wrap { trim: false })
        .scroll((
            usize::from(scroll).min(metrics.max_offset_from_bottom) as u16,
            0,
        ))
        .render(layout.content, b);
    let labels = [t.trust_remember_button, t.abort_button];
    let buttons = [
        MachineAuthButton::PasswordSubmit,
        MachineAuthButton::PasswordCancel,
    ];
    let rects = modal_button_row(layout.actions, &labels, 2);
    let mut hits = Vec::new();
    for ((rect, label), button) in rects.iter().zip(labels).zip(buttons) {
        modal_button(
            b,
            *rect,
            label,
            if button == MachineAuthButton::PasswordSubmit {
                crate::ui::ModalButtonTone::Primary
            } else {
                crate::ui::ModalButtonTone::Secondary
            },
            crate::ui::ModalButtonState::Normal,
            p,
        );
        hits.push((*rect, button));
    }
    render_key_hints(
        b,
        layout.footer,
        &[
            ("Enter".into(), t.hint_trust.into()),
            (
                "PgUp/PgDn".into(),
                crate::i18n::texts().global_menu.footer_select.into(),
            ),
            ("Esc".into(), t.hint_abort.into()),
        ],
        p,
        cx.components,
    );
    Some(OverlayRender {
        area: popup,
        machine_auth_actions: hits,
        machine_auth_max_scroll: metrics.max_offset_from_bottom,
        ..OverlayRender::default()
    })
}

fn render_password_prompt(
    b: &mut Buffer,
    view: &ClientPasswordView,
    scroll: u16,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    if view.is_host_key {
        return render_ssh_host_prompt(b, view, scroll, cx);
    }
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_auth;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(10),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 7 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 4, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_line(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        t.password_title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_line(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &view.prompt,
        base.fg(p.teal),
    );

    // Masked input: the secret is never drawn — one bullet per character
    // and the cursor sits after the last bullet.
    let input_rect = Rect::new(
        stack.header.x + 1,
        stack.header.y + 2,
        stack.header.width.saturating_sub(2),
        1,
    );
    let field = crate::ui::input_field_style(p);
    b.set_style(input_rect, field);
    let masked_len = view
        .input
        .as_str()
        .chars()
        .count()
        .min(usize::from(input_rect.width.saturating_sub(2)));
    let masked = "•".repeat(masked_len);
    put_text(
        b,
        input_rect.x + 1,
        input_rect.y,
        input_rect.width.saturating_sub(2),
        &masked,
        field,
    );
    let cursor_x = input_rect
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(masked_len).unwrap_or(u16::MAX))
        .min(input_rect.right().saturating_sub(1));
    let cursor = Some(crate::protocol::CursorState {
        x: cursor_x,
        y: input_rect.y,
        visible: true,
        shape: 0,
    });
    put_line(
        b,
        stack.header.x,
        stack.header.y + 3,
        stack.header.width,
        t.password_hidden_note,
        base.fg(p.overlay0),
    );
    if let Some(footer) = stack.footer {
        let footer_text = if view.is_passphrase {
            t.passphrase_agent_hint
        } else {
            ""
        };
        put_text(
            b,
            footer.x,
            footer.y,
            footer.width,
            footer_text,
            base.fg(p.overlay0),
        );
    }

    let mut action_hits = Vec::new();
    let labels = [t.password_submit_button, t.password_cancel_button];
    let buttons = [
        MachineAuthButton::PasswordSubmit,
        MachineAuthButton::PasswordCancel,
    ];
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let (tone, base_state) = match button {
                MachineAuthButton::PasswordSubmit => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let state = cx.button_state(
                &super::feedback::ChromeHover::MachineAuthButton(button),
                base_state,
            );
            modal_button(b, *rect, labels[index], tone, state, p);
            action_hits.push((*rect, button));
        }
    }
    Some(OverlayRender {
        area: popup,
        machine_auth_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}

/// Structured failure presentation for the machine detail card: the kind
/// sentence plus the suggested next step.
pub(super) fn failure_kind_presentation(
    kind: &crate::remote::ConnectionErrorKind,
) -> (&'static str, &'static str) {
    let t = &crate::i18n::texts().machine_auth;
    use crate::remote::ConnectionErrorKind as Kind;
    match kind {
        Kind::Dns => (t.kind_dns, t.next_retry),
        Kind::Timeout => (t.kind_timeout, t.next_retry),
        Kind::AuthDenied => (t.kind_auth_denied, t.next_auth_denied),
        Kind::HostKeyUnknown { .. } => (t.kind_host_key_unknown, t.next_host_key_unknown),
        Kind::HostKeyChanged => (t.kind_host_key_changed, t.next_host_key_changed),
        Kind::AuthRequired { .. } => (t.kind_auth_required, t.next_auth_required),
        Kind::RemoteInstallRequired => (t.kind_remote_install_required, t.next_install),
        Kind::RemoteInstallFailed => (t.kind_remote_install_failed, t.next_install),
        Kind::Protocol => (t.kind_protocol, t.next_protocol),
        Kind::Other => (t.kind_other, t.next_retry),
    }
}

/// Whether the failure kind has a dedicated recovery dialog behind the
/// detail card's review button.
pub(super) fn failure_kind_has_review(kind: &crate::remote::ConnectionErrorKind) -> bool {
    matches!(
        kind,
        crate::remote::ConnectionErrorKind::HostKeyUnknown { .. }
            | crate::remote::ConnectionErrorKind::HostKeyChanged
            | crate::remote::ConnectionErrorKind::AuthRequired { .. }
    )
}
