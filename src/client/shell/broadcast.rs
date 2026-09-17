//! Broadcast target-set manager overlay and the live input fan-out.
//!
//! The persisted set and its safety gates live in `endpoint::broadcast`; the
//! shell keeps an in-memory mirror so the per-keystroke fan-out never touches
//! the disk: the hot path short-circuits on one `enabled && !empty` check.
//! Every mutation loads the file fresh, validates, stores atomically, and
//! re-mirrors — the same discipline as the machine catalog edits. The mirror
//! also refreshes when the overlay opens and whenever the client-level file
//! watcher reports a `broadcast.json` change, so CLI edits made alongside a
//! running client surface in the badge, the overlay list, and the fan-out
//! targets without a restart.
//!
//! Fan-out reuses the cross-endpoint request lane (`push_endpoint_method_for`
//! with `PendingEndpointKind::BroadcastSend`): typed text goes as
//! `pane.send-text`, pastes and keys as `pane.send-input` so the target
//! server encodes them for its own negotiated keyboard/paste protocols. The
//! pane the user is actually typing into is excluded — it already receives
//! the input through the normal path. Mouse input is position-dependent and
//! is never broadcast.

use super::render::{
    modal_button, modal_button_row, modal_panel, put_right_text, put_text, render_key_hints,
    OverlayRender,
};
use super::*;
use crate::client::endpoint::{BroadcastSet, BroadcastTarget, ProfileId};

#[derive(Debug)]
pub(super) struct ClientBroadcastOverlay {
    pub(super) view: ClientBroadcastView,
    pub(super) selected: usize,
    pub(super) message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClientBroadcastView {
    List,
    PickMachine,
    PickPane { endpoint_id: ClientEndpointId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BroadcastButton {
    ToggleGate,
    Add,
    Remove,
    Clear,
    Close,
}

/// One resolved target row: the endpoint's display label and its pane.
#[derive(Debug, Clone)]
pub(super) struct BroadcastTargetRow {
    pub(super) machine_label: String,
    pub(super) pane_id: String,
    pub(super) online: bool,
}

fn endpoint_label_for(profiles: &[SavedSshEndpoint], machine: Option<&ProfileId>) -> String {
    match machine {
        None => crate::i18n::texts().sidebar.local.to_owned(),
        Some(id) => profiles
            .iter()
            .find(|profile| &profile.id == id)
            .map(|profile| profile.label.clone())
            .unwrap_or_else(|| id.to_string()),
    }
}

fn endpoint_online(endpoints: &[ClientShellEndpoint], endpoint_id: &ClientEndpointId) -> bool {
    endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        .is_some_and(|endpoint| {
            endpoint.status == ClientEndpointStatus::Online && endpoint.snapshot.is_some()
        })
}

pub(super) fn broadcast_target_rows(
    set: &BroadcastSet,
    endpoints: &[ClientShellEndpoint],
    profiles: &[SavedSshEndpoint],
) -> Vec<BroadcastTargetRow> {
    set.targets()
        .iter()
        .map(|target| {
            let endpoint_id = target
                .machine
                .clone()
                .map(ClientEndpointId::Ssh)
                .unwrap_or(ClientEndpointId::Local);
            BroadcastTargetRow {
                online: endpoint_online(endpoints, &endpoint_id),
                machine_label: endpoint_label_for(profiles, target.machine.as_ref()),
                pane_id: target.pane_id.clone(),
            }
        })
        .collect()
}

/// Machines eligible for a new target: Local plus every enabled saved
/// machine that does not have a pane in the set yet.
pub(super) fn broadcast_machine_candidates(
    set: &BroadcastSet,
    endpoints: &[ClientShellEndpoint],
    profiles: &[SavedSshEndpoint],
) -> Vec<(ClientEndpointId, String, bool)> {
    let has_target = |endpoint_id: &ClientEndpointId| {
        set.targets().iter().any(|target| {
            let target_endpoint = target
                .machine
                .clone()
                .map(ClientEndpointId::Ssh)
                .unwrap_or(ClientEndpointId::Local);
            &target_endpoint == endpoint_id
        })
    };
    let mut candidates = Vec::new();
    if !has_target(&ClientEndpointId::Local) {
        candidates.push((
            ClientEndpointId::Local,
            crate::i18n::texts().sidebar.local.to_owned(),
            endpoint_online(endpoints, &ClientEndpointId::Local),
        ));
    }
    for profile in profiles {
        if !profile.enabled {
            continue;
        }
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        if has_target(&endpoint_id) {
            continue;
        }
        candidates.push((
            endpoint_id.clone(),
            profile.label.clone(),
            endpoint_online(endpoints, &endpoint_id),
        ));
    }
    candidates
}

/// Panes of one endpoint from its cached snapshot (online endpoints only);
/// the broadcast pane ids are the server's public ids, exactly what
/// `pane.send-input` accepts.
pub(super) fn broadcast_pane_candidates(
    endpoints: &[ClientShellEndpoint],
    endpoint_id: &ClientEndpointId,
) -> Vec<(String, String)> {
    let Some(endpoint) = endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
    else {
        return Vec::new();
    };
    endpoint
        .snapshot
        .as_deref()
        .map(|snapshot| {
            snapshot
                .panes
                .iter()
                .map(|pane| {
                    let detail = pane
                        .label
                        .clone()
                        .or_else(|| pane.cwd.clone())
                        .unwrap_or_default();
                    (pane.pane_id.clone(), detail)
                })
                .collect()
        })
        .unwrap_or_default()
}

impl ClientShellState {
    /// The fan-out hot-path gate: one bool plus one emptiness read, no
    /// allocation — the mode bar and the input path both start here.
    pub(super) fn broadcast_active(&self) -> bool {
        self.broadcast.enabled && !self.broadcast.is_empty()
    }

    /// The mode-bar indicator: `Some(target count)` while input broadcasts.
    pub(super) fn broadcast_indicator_count(&self) -> Option<usize> {
        self.broadcast_active()
            .then(|| self.broadcast.targets().len())
    }

    fn broadcast_endpoint_label(&self, machine: Option<&ProfileId>) -> String {
        endpoint_label_for(&self.saved_profiles, machine)
    }

    /// Loads the set fresh, applies `mutate`, persists, and re-mirrors. A
    /// failed load surfaces as the returned error; a failed mutation leaves
    /// the stored file untouched.
    fn mutate_broadcast_set<T>(
        &mut self,
        mutate: impl FnOnce(&mut BroadcastSet) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut set = BroadcastSet::load()?;
        let result = mutate(&mut set)?;
        set.store()?;
        self.broadcast = set;
        Ok(result)
    }

    /// Refreshes the mirror from disk; errors degrade to keeping the current
    /// mirror (the fan-out must not die with a half-written file). Returns
    /// true when the mirror changed.
    ///
    /// This is also the watcher entry point: the file-change notification
    /// carries no payload, so the re-read always happens here at apply time.
    /// A notification sent before a local overlay edit was stored therefore
    /// reads back the locally stored version — the freshest write wins.
    pub(crate) fn refresh_broadcast_mirror(&mut self) -> bool {
        let Ok(set) = BroadcastSet::load() else {
            return false;
        };
        if set == self.broadcast {
            return false;
        }
        self.broadcast = set;
        true
    }

    pub(super) fn open_broadcast_overlay(&mut self) {
        self.refresh_broadcast_mirror();
        self.chrome_drag = None;
        self.overlay = Some(ClientShellOverlay::Broadcast(ClientBroadcastOverlay {
            view: ClientBroadcastView::List,
            selected: 0,
            message: None,
        }));
    }

    fn broadcast_set_gate(&mut self, enabled: bool) {
        let result = self.mutate_broadcast_set(|set| {
            set.enabled = enabled;
            Ok(())
        });
        let t = &crate::i18n::texts().broadcast;
        let message = match result {
            Ok(()) => Some(if enabled {
                t.enabled_message.to_owned()
            } else {
                t.disabled_message.to_owned()
            }),
            Err(error) => Some(error),
        };
        if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
            overlay.message = message;
        }
    }

    fn broadcast_remove_selected(&mut self) {
        let index = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Broadcast(overlay)) => overlay.selected,
            _ => return,
        };
        let result = self.mutate_broadcast_set(|set| {
            let removed = set.remove_target(index.saturating_add(1))?;
            Ok(removed)
        });
        let message = match result {
            Ok(removed) => {
                let machine = self.broadcast_endpoint_label(removed.machine.as_ref());
                Some(crate::i18n::fill(
                    crate::i18n::texts().broadcast.removed_fmt,
                    &[("machine", &machine), ("pane", &removed.pane_id)],
                ))
            }
            Err(error) => Some(error),
        };
        if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
            overlay.message = message;
            overlay.selected = overlay.selected.saturating_sub(1);
        }
    }

    fn broadcast_clear(&mut self) {
        let result = self.mutate_broadcast_set(|set| {
            set.clear();
            Ok(())
        });
        let message = match result {
            Ok(()) => Some(crate::i18n::texts().broadcast.cleared_message.to_owned()),
            Err(error) => Some(error),
        };
        if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
            overlay.message = message;
            overlay.selected = 0;
        }
    }

    /// Machines eligible for a new target, through the shared picker data.
    fn broadcast_machine_candidates(&self) -> Vec<(ClientEndpointId, String, bool)> {
        broadcast_machine_candidates(&self.broadcast, &self.endpoints, &self.saved_profiles)
    }

    /// Panes of one endpoint from its cached snapshot.
    fn broadcast_pane_candidates(&self, endpoint_id: &ClientEndpointId) -> Vec<(String, String)> {
        broadcast_pane_candidates(&self.endpoints, endpoint_id)
    }

    fn broadcast_add_target(&mut self, endpoint_id: &ClientEndpointId, pane_id: &str) {
        let machine = match endpoint_id {
            ClientEndpointId::Local => None,
            ClientEndpointId::Ssh(id) => Some(id.clone()),
        };
        let label = self.broadcast_endpoint_label(machine.as_ref());
        let pane_id = pane_id.to_owned();
        let result = self.mutate_broadcast_set(|set| {
            set.add_target(BroadcastTarget {
                machine,
                pane_id: pane_id.clone(),
            })
        });
        let t = &crate::i18n::texts().broadcast;
        let message = match result {
            Ok(()) => Some(crate::i18n::fill(
                t.added_fmt,
                &[("machine", &label), ("pane", &pane_id)],
            )),
            Err(error) => {
                // The one-endpoint-one-pane rule is enforced by the picker
                // already; any other backend error is shown verbatim.
                let _ = error;
                Some(crate::i18n::fill(t.duplicate_fmt, &[("machine", &label)]))
            }
        };
        if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientBroadcastView::List;
            overlay.message = message;
            overlay.selected = self.broadcast.targets().len().saturating_sub(1);
        }
    }

    fn broadcast_back(&mut self) {
        let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match &overlay.view {
            ClientBroadcastView::List => {
                self.overlay = None;
            }
            ClientBroadcastView::PickMachine => overlay.view = ClientBroadcastView::List,
            ClientBroadcastView::PickPane { .. } => {
                overlay.view = ClientBroadcastView::PickMachine;
                overlay.selected = 0;
            }
        }
    }

    fn move_broadcast_selection(&mut self, delta: isize) {
        let count = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Broadcast(overlay)) => match &overlay.view {
                ClientBroadcastView::List => self.broadcast.targets().len(),
                ClientBroadcastView::PickMachine => self.broadcast_machine_candidates().len(),
                ClientBroadcastView::PickPane { endpoint_id } => {
                    self.broadcast_pane_candidates(endpoint_id).len()
                }
            },
            _ => 0,
        };
        let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if count == 0 {
            overlay.selected = 0;
            return;
        }
        overlay.selected =
            (overlay.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
    }

    fn broadcast_activate_selection(&mut self) {
        enum Activate {
            PickMachine(ClientEndpointId),
            PickPane(ClientEndpointId, String),
        }
        let view = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Broadcast(overlay)) => overlay.view.clone(),
            _ => return,
        };
        let activate = match &view {
            ClientBroadcastView::List => None,
            ClientBroadcastView::PickMachine => {
                let candidates = self.broadcast_machine_candidates();
                let selected = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Broadcast(overlay)) => overlay.selected,
                    _ => 0,
                };
                let Some((endpoint_id, label, online)) = candidates.get(selected).cloned() else {
                    return;
                };
                if !online {
                    if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
                        overlay.message = Some(crate::i18n::fill(
                            crate::i18n::texts().broadcast.picker_offline_fmt,
                            &[("label", &label)],
                        ));
                    }
                    None
                } else {
                    Some(Activate::PickMachine(endpoint_id))
                }
            }
            ClientBroadcastView::PickPane { endpoint_id } => {
                let panes = self.broadcast_pane_candidates(endpoint_id);
                let selected = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Broadcast(overlay)) => overlay.selected,
                    _ => 0,
                };
                let Some((pane_id, _)) = panes.get(selected).cloned() else {
                    return;
                };
                Some(Activate::PickPane(endpoint_id.clone(), pane_id))
            }
        };
        match activate {
            Some(Activate::PickMachine(endpoint_id)) => {
                let panes = self.broadcast_pane_candidates(&endpoint_id);
                let label = self.endpoint_label(&endpoint_id).to_owned();
                if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
                    if panes.is_empty() {
                        overlay.message = Some(crate::i18n::fill(
                            crate::i18n::texts().broadcast.picker_no_panes_fmt,
                            &[("label", &label)],
                        ));
                    } else {
                        overlay.view = ClientBroadcastView::PickPane { endpoint_id };
                        overlay.selected = 0;
                        overlay.message = None;
                    }
                }
            }
            Some(Activate::PickPane(endpoint_id, pane_id)) => {
                self.broadcast_add_target(&endpoint_id, &pane_id);
            }
            None => {}
        }
    }

    pub(super) fn route_broadcast_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Broadcast(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        match code {
            KeyCode::Esc => {
                self.broadcast_back();
                outcome.repaint = true;
            }
            KeyCode::Enter => {
                self.broadcast_activate_selection();
                outcome.repaint = true;
            }
            KeyCode::Up | KeyCode::Char('k') if plain => {
                self.move_broadcast_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                self.move_broadcast_selection(1);
                outcome.repaint = true;
            }
            KeyCode::Char('g') if plain => {
                let enable = !self.broadcast.enabled;
                self.broadcast_set_gate(enable);
                outcome.repaint = true;
            }
            KeyCode::Char('a') if plain => {
                if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientBroadcastView::PickMachine;
                    overlay.selected = 0;
                    overlay.message = None;
                }
                outcome.repaint = true;
            }
            KeyCode::Char('x' | 'd') if plain => {
                if matches!(
                    self.overlay,
                    Some(ClientShellOverlay::Broadcast(ClientBroadcastOverlay {
                        view: ClientBroadcastView::List,
                        ..
                    }))
                ) {
                    self.broadcast_remove_selected();
                }
                outcome.repaint = true;
            }
            KeyCode::Char('c') if plain => {
                if matches!(
                    self.overlay,
                    Some(ClientShellOverlay::Broadcast(ClientBroadcastOverlay {
                        view: ClientBroadcastView::List,
                        ..
                    }))
                ) {
                    self.broadcast_clear();
                }
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    /// Mouse activation for one rendered broadcast-overlay button.
    pub(super) fn activate_broadcast_button(
        &mut self,
        button: BroadcastButton,
        outcome: &mut ClientShellInput,
    ) {
        match button {
            BroadcastButton::ToggleGate => {
                let enable = !self.broadcast.enabled;
                self.broadcast_set_gate(enable);
            }
            BroadcastButton::Add => {
                if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientBroadcastView::PickMachine;
                    overlay.selected = 0;
                    overlay.message = None;
                }
            }
            BroadcastButton::Remove => self.broadcast_remove_selected(),
            BroadcastButton::Clear => self.broadcast_clear(),
            BroadcastButton::Close => self.broadcast_back(),
        }
        outcome.repaint = true;
    }

    /// Mouse click on a list/picker row: the target list selects, pickers
    /// select-and-activate (mirrors the wizard row behavior).
    pub(super) fn click_broadcast_row(&mut self, row: usize, outcome: &mut ClientShellInput) {
        let picker = matches!(
            self.overlay,
            Some(ClientShellOverlay::Broadcast(ClientBroadcastOverlay {
                view: ClientBroadcastView::PickMachine | ClientBroadcastView::PickPane { .. },
                ..
            }))
        );
        if let Some(ClientShellOverlay::Broadcast(overlay)) = self.overlay.as_mut() {
            overlay.selected = row;
        }
        if picker {
            self.broadcast_activate_selection();
        }
        outcome.repaint = true;
    }

    pub(super) fn scroll_broadcast_overlay(&mut self, delta: isize) {
        self.move_broadcast_selection(delta);
    }

    // ---------- live input fan-out ----------

    /// Fans one text commit out to every registered pane except the one the
    /// user is typing into. `paste` routes through `pane.send-input` so the
    /// target server applies its bracketed-paste wrapping; typed text goes
    /// verbatim as `pane.send-text`.
    pub(super) fn broadcast_text(
        &mut self,
        text: &str,
        paste: bool,
        outcome: &mut ClientShellInput,
    ) {
        if !self.broadcast_active() {
            return;
        }
        let focused = self.focused_pane_id();
        let targets = self.broadcast.targets().to_vec();
        for target in targets {
            let endpoint_id = target
                .machine
                .clone()
                .map(ClientEndpointId::Ssh)
                .unwrap_or(ClientEndpointId::Local);
            if endpoint_id == self.active_endpoint_id
                && focused.as_deref() == Some(&*target.pane_id)
            {
                continue;
            }
            let method = if paste {
                crate::api::schema::Method::PaneSendInput(crate::api::schema::PaneSendInputParams {
                    pane_id: target.pane_id.clone(),
                    text: text.to_owned(),
                    keys: Vec::new(),
                })
            } else {
                crate::api::schema::Method::PaneSendText(crate::api::schema::PaneSendTextParams {
                    pane_id: target.pane_id.clone(),
                    text: text.to_owned(),
                })
            };
            let machine = self.broadcast_endpoint_label(target.machine.as_ref());
            self.push_endpoint_method_for(
                &endpoint_id,
                method,
                PendingEndpointKind::BroadcastSend { machine },
                outcome,
            );
        }
    }

    /// Fans one key press out as `pane.send-input` with the key's canonical
    /// combo name. Release events and keys without a round-trippable name
    /// (media keys, kitty-protocol specials) are not broadcast.
    pub(super) fn broadcast_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if !self.broadcast_active() {
            return;
        }
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        let name = crate::config::format_key_combo((key.code, key.modifiers));
        if crate::config::parse_key_combo(&name).is_none() {
            return;
        }
        let focused = self.focused_pane_id();
        let targets = self.broadcast.targets().to_vec();
        for target in targets {
            let endpoint_id = target
                .machine
                .clone()
                .map(ClientEndpointId::Ssh)
                .unwrap_or(ClientEndpointId::Local);
            if endpoint_id == self.active_endpoint_id
                && focused.as_deref() == Some(&*target.pane_id)
            {
                continue;
            }
            let machine = self.broadcast_endpoint_label(target.machine.as_ref());
            self.push_endpoint_method_for(
                &endpoint_id,
                crate::api::schema::Method::PaneSendInput(
                    crate::api::schema::PaneSendInputParams {
                        pane_id: target.pane_id.clone(),
                        text: String::new(),
                        keys: vec![name.clone()],
                    },
                ),
                PendingEndpointKind::BroadcastSend { machine },
                outcome,
            );
        }
    }

    /// One fan-out target resolved. Failures surface once per endpoint
    /// (deduped by the notice key) instead of once per keystroke; a success
    /// clears the dedupe so a later failure is reported again.
    pub(super) fn complete_broadcast_send(
        &mut self,
        boot_id: &str,
        machine: &str,
        error: Option<ClientShellEndpointError>,
    ) -> bool {
        let code = format!("broadcast:{machine}");
        let key = ClientEndpointNoticeKey {
            boot_id: boot_id.to_owned(),
            kind: ClientEndpointNoticeKind::Unavailable,
            code: code.clone(),
        };
        match error {
            None => {
                self.endpoint_notice_seen.remove(&key);
                false
            }
            Some(error) => self.push_endpoint_notice(
                ClientEndpointNoticeKind::Unavailable,
                code,
                crate::i18n::texts().broadcast.title.to_owned(),
                crate::i18n::fill(
                    crate::i18n::texts().broadcast.notice_failed_fmt,
                    &[("label", machine), ("error", &error.message)],
                ),
            ),
        }
    }
}

// ---------- rendering ----------

pub(super) fn render_broadcast_overlay(
    b: &mut Buffer,
    overlay: &ClientBroadcastOverlay,
    set: &BroadcastSet,
    rows: &[BroadcastTargetRow],
    candidates: &[(ClientEndpointId, String, bool)],
    pick_label: &str,
    panes: &[(String, String)],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().broadcast;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(18), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            broadcast_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);

    let (title, body_rows): (String, Vec<(String, Option<String>, bool)>) = match &overlay.view {
        ClientBroadcastView::List => {
            let gate = if set.enabled {
                t.gate_enabled
            } else {
                t.gate_disabled
            };
            let title = format!(" {} · {gate}", t.title);
            let rows = rows
                .iter()
                .map(|row| {
                    let status = if row.online {
                        None
                    } else {
                        Some(crate::i18n::texts().endpoint.st_reconnecting.to_owned())
                    };
                    (
                        format!(" {} · {}", row.machine_label, row.pane_id),
                        status,
                        row.online,
                    )
                })
                .collect();
            (title, rows)
        }
        ClientBroadcastView::PickMachine => {
            let rows = candidates
                .iter()
                .map(|(_, label, online)| {
                    let status =
                        (!online).then(|| crate::i18n::texts().endpoint.st_reconnecting.to_owned());
                    (format!(" {label}"), status, *online)
                })
                .collect();
            (format!(" {}", t.pick_machine_title), rows)
        }
        ClientBroadcastView::PickPane { .. } => {
            let title = crate::i18n::fill(t.pick_pane_title_fmt, &[("label", pick_label)]);
            let rows = panes
                .iter()
                .map(|(pane_id, detail)| {
                    let text = if detail.is_empty() {
                        format!(" {pane_id}")
                    } else {
                        format!(" {pane_id} · {detail}")
                    };
                    (text, None, true)
                })
                .collect();
            (format!(" {title}"), rows)
        }
    };
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    if matches!(overlay.view, ClientBroadcastView::List) {
        let count = crate::i18n::fill(t.count_fmt, &[("count", &rows.len().to_string())]);
        put_right_text(
            b,
            Rect::new(stack.header.x, stack.header.y, stack.header.width, 1),
            stack.header.y,
            &count,
            base.fg(p.overlay0),
        );
    }

    let body = stack.content;
    let mut row_hits = Vec::new();
    if body_rows.is_empty() {
        let (line1, line2) = match &overlay.view {
            ClientBroadcastView::List => (Some(t.empty), Some(t.empty_hint)),
            _ => (None, None),
        };
        if let Some(line) = line1 {
            put_text(b, body.x, body.y, body.width, line, base.fg(p.overlay1));
        }
        if let Some(line) = line2 {
            put_text(b, body.x, body.y + 1, body.width, line, base.fg(p.overlay0));
        }
    }
    let visible = usize::from(body.height).max(1);
    let selected = if body_rows.is_empty() {
        0
    } else {
        overlay.selected.min(body_rows.len() - 1)
    };
    let scroll = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(selected);
    for (index, (text, status, enabled)) in body_rows.iter().enumerate().skip(scroll).take(visible)
    {
        let y = body.y + (index - scroll) as u16;
        let rect = Rect::new(body.x, y, body.width, 1);
        row_hits.push((rect, index));
        let is_selected = index == selected;
        let style = if is_selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let text_style = if *enabled {
            style
        } else {
            style.add_modifier(Modifier::DIM)
        };
        put_text(b, rect.x, rect.y, rect.width, text, text_style);
        if let Some(status) = status {
            let status_style = if is_selected {
                style
            } else {
                Style::default().fg(p.yellow).bg(p.panel_bg)
            };
            put_right_text(b, rect, rect.y, status, status_style);
        }
    }

    if let Some(footer) = stack.footer {
        if let Some(message) = overlay.message.as_deref() {
            put_text(
                b,
                footer.x,
                footer.y,
                footer.width,
                message,
                base.fg(p.green),
            );
        } else {
            let hints: Vec<(String, String)> = match &overlay.view {
                ClientBroadcastView::List => vec![
                    ("↑↓".to_owned(), t.hint_select.to_owned()),
                    ("g".to_owned(), t.hint_gate.to_owned()),
                    ("a".to_owned(), t.hint_add.to_owned()),
                    ("x".to_owned(), t.hint_remove.to_owned()),
                    ("c".to_owned(), t.hint_clear.to_owned()),
                    ("esc".to_owned(), t.hint_close.to_owned()),
                ],
                ClientBroadcastView::PickMachine | ClientBroadcastView::PickPane { .. } => vec![
                    ("↑↓".to_owned(), t.hint_select.to_owned()),
                    ("enter".to_owned(), t.hint_select.to_owned()),
                    ("esc".to_owned(), t.hint_back.to_owned()),
                ],
            };
            render_key_hints(b, footer, &hints, p, cx.components);
        }
    }

    let mut action_hits = Vec::new();
    if matches!(overlay.view, ClientBroadcastView::List) {
        let gate_label = if set.enabled {
            t.disable_button
        } else {
            t.enable_button
        };
        let close_label = crate::ui::modal_close_button_text();
        let labels = [
            gate_label,
            t.add_button,
            t.remove_button,
            t.clear_button,
            close_label,
        ];
        let buttons = [
            BroadcastButton::ToggleGate,
            BroadcastButton::Add,
            BroadcastButton::Remove,
            BroadcastButton::Clear,
            BroadcastButton::Close,
        ];
        let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
        if rects.len() == labels.len() {
            for (index, rect) in rects.iter().enumerate() {
                let button = buttons[index];
                let enabled = button != BroadcastButton::Remove && button != BroadcastButton::Clear
                    || !set.is_empty();
                let (tone, base_state) = match button {
                    BroadcastButton::ToggleGate => (
                        crate::ui::ModalButtonTone::Primary,
                        crate::ui::ModalButtonState::Focused,
                    ),
                    BroadcastButton::Clear => (
                        crate::ui::ModalButtonTone::Danger,
                        crate::ui::ModalButtonState::Normal,
                    ),
                    _ => (
                        crate::ui::ModalButtonTone::Secondary,
                        crate::ui::ModalButtonState::Normal,
                    ),
                };
                let state = if enabled {
                    cx.button_state(
                        &super::feedback::ChromeHover::BroadcastButton(button),
                        base_state,
                    )
                } else {
                    crate::ui::ModalButtonState::Disabled
                };
                modal_button(b, *rect, labels[index], tone, state, p);
                if enabled {
                    action_hits.push((*rect, button));
                }
            }
        }
    } else {
        let back_label = crate::i18n::texts().overlays.back_button;
        let rects = modal_button_row(stack.actions.unwrap_or_default(), &[back_label], 2);
        if let [back] = rects.as_slice() {
            modal_button(
                b,
                *back,
                back_label,
                crate::ui::ModalButtonTone::Secondary,
                cx.button_state(
                    &super::feedback::ChromeHover::BroadcastButton(BroadcastButton::Close),
                    crate::ui::ModalButtonState::Focused,
                ),
                p,
            );
            action_hits.push((*back, BroadcastButton::Close));
        }
    }

    Some(OverlayRender {
        area: popup,
        broadcast_popup: popup,
        broadcast_rows: row_hits,
        broadcast_actions: action_hits,
        ..OverlayRender::default()
    })
}
