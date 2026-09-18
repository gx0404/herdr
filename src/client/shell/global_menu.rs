use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientGlobalMenuAction {
    Binding(crate::input::KeybindAction),
    Notifications,
    WhatsNew,
    Observation(super::observability::Page),
}

pub(super) fn global_menu_attention(snapshot: &ClientShellSnapshot) -> bool {
    snapshot.update_available.is_some() || snapshot.integration_updates_available
}

pub(super) fn global_menu_items(
    snapshot: &ClientShellSnapshot,
) -> Vec<(&'static str, ClientGlobalMenuAction)> {
    let t = &crate::i18n::texts().global_menu;
    let mut items = vec![
        (
            t.settings,
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Settings),
        ),
        (
            t.machines,
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::ManageMachines),
        ),
        (t.notifications, ClientGlobalMenuAction::Notifications),
        (
            t.keybinds,
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Help),
        ),
        (
            t.reload_config,
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::ReloadConfig),
        ),
    ];
    if snapshot.update_available.is_some() || snapshot.latest_release_notes_available {
        items.push((
            if snapshot.update_available.is_some() {
                t.update_ready
            } else {
                t.whats_new
            },
            ClientGlobalMenuAction::WhatsNew,
        ));
    }
    items.push((
        t.detach,
        ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Detach),
    ));
    items.push((
        super::observability::tr("Monitor", "监控"),
        ClientGlobalMenuAction::Observation(super::observability::Page::Monitor),
    ));
    items
}

impl ClientShellState {
    pub(super) fn activate_global_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(action) = self.snapshot.as_deref().and_then(|snapshot| {
            global_menu_items(snapshot)
                .get(index)
                .map(|(_, action)| *action)
        }) else {
            return;
        };
        if action == ClientGlobalMenuAction::WhatsNew
            && self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.release_notes.as_ref())
                .is_none()
        {
            return;
        }
        self.overlay = None;
        match action {
            ClientGlobalMenuAction::Observation(page) => self.open_observation_page(page, outcome),
            ClientGlobalMenuAction::Binding(binding) => {
                self.record_binding(crate::input::KeybindMatch::Action(binding), outcome)
            }
            ClientGlobalMenuAction::Notifications => self.open_notification_history(),
            ClientGlobalMenuAction::WhatsNew => self.open_release_notes(),
        }
        outcome.repaint = true;
    }
}
