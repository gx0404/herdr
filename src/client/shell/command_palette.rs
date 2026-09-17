use super::feedback::ChromeContext;
use super::render::{
    display_width, modal_panel, put_right_text, put_text, render_key_hints, render_search_bar,
    OverlayRender, SearchBar,
};
use super::*;

/// Cap on the persisted most-recently-used command list.
pub(super) const PALETTE_RECENT_LIMIT: usize = 8;

/// One executable row of the command palette.
#[derive(Debug, Clone)]
pub(super) struct ClientPaletteItem {
    /// Stable identifier for MRU persistence (e.g. "binding:NewTab",
    /// "machine:connect:<profile-id>").
    pub(super) id: String,
    pub(super) title: String,
    /// Dim helper line: the live binding label or a machine target.
    pub(super) subtitle: String,
    pub(super) badge: bool,
    pub(super) action: ClientPaletteAction,
}

#[derive(Debug, Clone)]
pub(super) enum ClientPaletteAction {
    Binding(crate::input::KeybindAction),
    CustomCommand(usize),
    Notifications,
    WhatsNew,
    MachineConnect(crate::client::endpoint::ProfileId),
    MachineSwitch(ClientEndpointId),
    MachineEdit(crate::client::endpoint::ProfileId),
    MachineToggleEnabled(crate::client::endpoint::ProfileId, bool),
    MachineImport,
    SnippetsList,
    SnippetRun,
    SceneSave,
    SceneList,
    Broadcast,
}

/// Command palette overlay state: the item index is built once at open and
/// `recent_ids` snapshots the MRU so ordering stays stable while open.
#[derive(Debug)]
pub(super) struct ClientCommandPaletteOverlay {
    pub(super) query: TextEditor,
    pub(super) selected: usize,
    pub(super) scroll: usize,
    pub(super) items: Vec<ClientPaletteItem>,
    pub(super) recent_ids: Vec<String>,
}

/// One filtered row: the item plus fuzzy-match character positions in its
/// title (for highlight) and whether it came from the MRU section.
pub(super) struct ClientPaletteRow<'a> {
    pub(super) item: &'a ClientPaletteItem,
    pub(super) match_indices: Vec<usize>,
    pub(super) recent: bool,
}

/// Greedy subsequence matcher over characters, case-insensitive. Returns a
/// score (higher is better) and the matched character indices in `text`.
/// Consecutive runs and word-start hits score highest.
pub(super) fn fuzzy_match(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    let query_chars: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query_chars.is_empty() {
        return Some((0, Vec::new()));
    }
    let text_chars: Vec<char> = text.chars().collect();
    let lowered: Vec<char> = text_chars.iter().flat_map(|c| c.to_lowercase()).collect();
    let mut query_index = 0;
    let mut indices = Vec::with_capacity(query_chars.len());
    let mut score = 0i64;
    let mut previous_match = None;
    for (text_index, ch) in lowered.iter().enumerate() {
        if query_index >= query_chars.len() || *ch != query_chars[query_index] {
            continue;
        }
        indices.push(text_index);
        score += 1;
        if previous_match == text_index.checked_sub(1) {
            score += 8;
        }
        let word_start = text_index == 0
            || text_chars[text_index - 1].is_whitespace()
            || matches!(text_chars[text_index - 1], '-' | '_' | '/' | ':' | '.');
        if word_start {
            score += 6;
        }
        previous_match = Some(text_index);
        query_index += 1;
    }
    (query_index == query_chars.len()).then_some((score, indices))
}

/// The rows currently visible in the palette. With an empty query the MRU
/// section leads (deduplicated, capped at open time); a non-empty query
/// fuzzy-filters and sorts by score, stable by declaration order.
pub(super) fn palette_rows(palette: &ClientCommandPaletteOverlay) -> Vec<ClientPaletteRow<'_>> {
    let query = palette.query.as_str().trim();
    if query.is_empty() {
        let mut rows = Vec::with_capacity(palette.items.len());
        let mut seen = std::collections::HashSet::new();
        for id in &palette.recent_ids {
            if let Some(item) = palette.items.iter().find(|item| &item.id == id) {
                if seen.insert(item.id.as_str()) {
                    rows.push(ClientPaletteRow {
                        item,
                        match_indices: Vec::new(),
                        recent: true,
                    });
                }
            }
        }
        for item in &palette.items {
            if seen.insert(item.id.as_str()) {
                rows.push(ClientPaletteRow {
                    item,
                    match_indices: Vec::new(),
                    recent: false,
                });
            }
        }
        return rows;
    }
    let mut scored = palette
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            fuzzy_match(query, &item.title).map(|(score, indices)| {
                (
                    score,
                    index,
                    ClientPaletteRow {
                        item,
                        match_indices: indices,
                        recent: false,
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    scored.sort_by(
        |(left_score, left_index, _), (right_score, right_index, _)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    scored
        .into_iter()
        .map(|(_, _, row)| row)
        .collect::<Vec<_>>()
}

fn palette_binding_items(items: &mut Vec<ClientPaletteItem>, keybinds: &crate::config::Keybinds) {
    use crate::input::KeybindAction;
    let t = &crate::i18n::texts();
    let global_menu = &t.global_menu;
    let keybind_texts = &t.keybinds;
    fn push(
        items: &mut Vec<ClientPaletteItem>,
        id: &str,
        title: &str,
        bindings: &crate::config::ActionKeybinds,
        action: KeybindAction,
    ) {
        items.push(ClientPaletteItem {
            id: format!("binding:{id}"),
            title: title.to_owned(),
            subtitle: bindings.label().unwrap_or_default(),
            badge: false,
            action: ClientPaletteAction::Binding(action),
        });
    }
    push(
        items,
        "Settings",
        global_menu.settings,
        &keybinds.settings,
        KeybindAction::Settings,
    );
    push(
        items,
        "ManageMachines",
        global_menu.machines,
        &keybinds.manage_machines,
        KeybindAction::ManageMachines,
    );
    items.push(ClientPaletteItem {
        id: "notifications".to_owned(),
        title: global_menu.notifications.to_owned(),
        subtitle: String::new(),
        badge: false,
        action: ClientPaletteAction::Notifications,
    });
    push(
        items,
        "Help",
        global_menu.keybinds,
        &keybinds.help,
        KeybindAction::Help,
    );
    push(
        items,
        "ReloadConfig",
        global_menu.reload_config,
        &keybinds.reload_config,
        KeybindAction::ReloadConfig,
    );
    push(
        items,
        "WorkspacePicker",
        keybind_texts.workspace_navigation,
        &keybinds.workspace_picker,
        KeybindAction::WorkspacePicker,
    );
    push(
        items,
        "OpenNavigator",
        keybind_texts.session_navigator,
        &keybinds.goto,
        KeybindAction::OpenNavigator,
    );
    push(
        items,
        "NewWorkspace",
        keybind_texts.new_workspace,
        &keybinds.new_workspace,
        KeybindAction::NewWorkspace,
    );
    push(
        items,
        "NewWorktree",
        keybind_texts.new_worktree,
        &keybinds.new_worktree,
        KeybindAction::NewWorktree,
    );
    push(
        items,
        "OpenWorktree",
        keybind_texts.open_worktree,
        &keybinds.open_worktree,
        KeybindAction::OpenWorktree,
    );
    push(
        items,
        "RemoveWorktree",
        keybind_texts.delete_worktree_checkout,
        &keybinds.remove_worktree,
        KeybindAction::RemoveWorktree,
    );
    push(
        items,
        "RenameWorkspace",
        keybind_texts.rename_workspace,
        &keybinds.rename_workspace,
        KeybindAction::RenameWorkspace,
    );
    push(
        items,
        "CloseWorkspace",
        keybind_texts.close_workspace,
        &keybinds.close_workspace,
        KeybindAction::CloseWorkspace,
    );
    push(
        items,
        "PreviousWorkspace",
        keybind_texts.previous_workspace,
        &keybinds.previous_workspace,
        KeybindAction::PreviousWorkspace,
    );
    push(
        items,
        "NextWorkspace",
        keybind_texts.next_workspace,
        &keybinds.next_workspace,
        KeybindAction::NextWorkspace,
    );
    push(
        items,
        "PreviousAgent",
        keybind_texts.previous_agent,
        &keybinds.previous_agent,
        KeybindAction::PreviousAgent,
    );
    push(
        items,
        "NextAgent",
        keybind_texts.next_agent,
        &keybinds.next_agent,
        KeybindAction::NextAgent,
    );
    push(
        items,
        "NewTab",
        keybind_texts.new_tab,
        &keybinds.new_tab,
        KeybindAction::NewTab,
    );
    push(
        items,
        "RenameTab",
        keybind_texts.rename_tab,
        &keybinds.rename_tab,
        KeybindAction::RenameTab,
    );
    push(
        items,
        "PreviousTab",
        keybind_texts.previous_tab,
        &keybinds.previous_tab,
        KeybindAction::PreviousTab,
    );
    push(
        items,
        "NextTab",
        keybind_texts.next_tab,
        &keybinds.next_tab,
        KeybindAction::NextTab,
    );
    push(
        items,
        "MoveTabPrevious",
        keybind_texts.move_tab_left,
        &keybinds.move_tab_previous,
        KeybindAction::MoveTabPrevious,
    );
    push(
        items,
        "MoveTabNext",
        keybind_texts.move_tab_right,
        &keybinds.move_tab_next,
        KeybindAction::MoveTabNext,
    );
    push(
        items,
        "CloseTab",
        keybind_texts.close_tab,
        &keybinds.close_tab,
        KeybindAction::CloseTab,
    );
    push(
        items,
        "RenamePane",
        keybind_texts.rename_pane,
        &keybinds.rename_pane,
        KeybindAction::RenamePane,
    );
    push(
        items,
        "EditScrollback",
        keybind_texts.edit_scrollback,
        &keybinds.edit_scrollback,
        KeybindAction::EditScrollback,
    );
    push(
        items,
        "CopyMode",
        keybind_texts.copy_mode,
        &keybinds.copy_mode,
        KeybindAction::CopyMode,
    );
    push(
        items,
        "SplitVertical",
        keybind_texts.split_vertical,
        &keybinds.split_vertical,
        KeybindAction::SplitVertical,
    );
    push(
        items,
        "SplitHorizontal",
        keybind_texts.split_horizontal,
        &keybinds.split_horizontal,
        KeybindAction::SplitHorizontal,
    );
    push(
        items,
        "ClosePane",
        keybind_texts.close_pane,
        &keybinds.close_pane,
        KeybindAction::ClosePane,
    );
    push(
        items,
        "Zoom",
        keybind_texts.zoom_pane,
        &keybinds.zoom,
        KeybindAction::Zoom,
    );
    push(
        items,
        "EnterResizeMode",
        keybind_texts.resize_mode,
        &keybinds.resize_mode,
        KeybindAction::EnterResizeMode,
    );
    push(
        items,
        "FocusPaneLeft",
        keybind_texts.focus_pane_left,
        &keybinds.focus_pane_left,
        KeybindAction::FocusPaneLeft,
    );
    push(
        items,
        "FocusPaneDown",
        keybind_texts.focus_pane_down,
        &keybinds.focus_pane_down,
        KeybindAction::FocusPaneDown,
    );
    push(
        items,
        "FocusPaneUp",
        keybind_texts.focus_pane_up,
        &keybinds.focus_pane_up,
        KeybindAction::FocusPaneUp,
    );
    push(
        items,
        "FocusPaneRight",
        keybind_texts.focus_pane_right,
        &keybinds.focus_pane_right,
        KeybindAction::FocusPaneRight,
    );
    push(
        items,
        "CyclePaneNext",
        keybind_texts.cycle_pane_next,
        &keybinds.cycle_pane_next,
        KeybindAction::CyclePaneNext,
    );
    push(
        items,
        "CyclePanePrevious",
        keybind_texts.cycle_pane_previous,
        &keybinds.cycle_pane_previous,
        KeybindAction::CyclePanePrevious,
    );
    push(
        items,
        "LastPane",
        keybind_texts.last_pane,
        &keybinds.last_pane,
        KeybindAction::LastPane,
    );
    push(
        items,
        "LinkHints",
        keybind_texts.link_hints,
        &keybinds.link_hints,
        KeybindAction::LinkHints,
    );
    push(
        items,
        "ToggleSidebar",
        keybind_texts.toggle_sidebar,
        &keybinds.toggle_sidebar,
        KeybindAction::ToggleSidebar,
    );
    push(
        items,
        "Detach",
        global_menu.detach,
        &keybinds.detach,
        KeybindAction::Detach,
    );
}

impl ClientShellState {
    fn build_palette_items(&self) -> Vec<ClientPaletteItem> {
        let mut items = Vec::new();
        // Attention items lead the list so they are visible without scrolling.
        if let Some(snapshot) = self.snapshot.as_deref() {
            if snapshot.update_available.is_some() || snapshot.latest_release_notes_available {
                let global_menu = &crate::i18n::texts().global_menu;
                items.push(ClientPaletteItem {
                    id: "whats_new".to_owned(),
                    title: if snapshot.update_available.is_some() {
                        global_menu.update_ready.to_owned()
                    } else {
                        global_menu.whats_new.to_owned()
                    },
                    subtitle: String::new(),
                    badge: snapshot.update_available.is_some(),
                    action: ClientPaletteAction::WhatsNew,
                });
            }
        }
        palette_binding_items(&mut items, &self.config.keybinds.keybinds);
        if let Some(snapshot) = self.snapshot.as_deref() {
            if snapshot.integration_updates_available {
                if let Some(item) = items.iter_mut().find(|item| item.id == "binding:Settings") {
                    item.badge = true;
                }
            }
        }
        for (index, command) in self
            .config
            .keybinds
            .keybinds
            .custom_commands
            .iter()
            .enumerate()
        {
            items.push(ClientPaletteItem {
                id: format!("command:{}", command.command),
                title: command
                    .description
                    .clone()
                    .unwrap_or_else(|| command.command.clone()),
                subtitle: command.label.clone(),
                badge: false,
                action: ClientPaletteAction::CustomCommand(index),
            });
        }
        let global_menu = &crate::i18n::texts().global_menu;
        for (id, title, action) in [
            (
                "machine:import",
                global_menu.machine_import,
                ClientPaletteAction::MachineImport,
            ),
            (
                "snippets:list",
                global_menu.snippets,
                ClientPaletteAction::SnippetsList,
            ),
            (
                "snippets:run",
                global_menu.snippet_run,
                ClientPaletteAction::SnippetRun,
            ),
            (
                "scene:save",
                global_menu.scene_save,
                ClientPaletteAction::SceneSave,
            ),
            (
                "scene:list",
                global_menu.scene_restore,
                ClientPaletteAction::SceneList,
            ),
            (
                "broadcast",
                global_menu.broadcast,
                ClientPaletteAction::Broadcast,
            ),
        ] {
            items.push(ClientPaletteItem {
                id: id.to_owned(),
                title: title.to_owned(),
                subtitle: String::new(),
                badge: false,
                action,
            });
        }
        for profile in &self.saved_profiles {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let online = self.endpoint_is_online(&endpoint_id);
            let fill = |template: &str| crate::i18n::fill(template, &[("label", &profile.label)]);
            if profile.enabled && online && self.active_endpoint_id != endpoint_id {
                items.push(ClientPaletteItem {
                    id: format!("machine:switch:{}", profile.id.as_str()),
                    title: fill(global_menu.machine_switch_fmt),
                    subtitle: profile.target.clone(),
                    badge: false,
                    action: ClientPaletteAction::MachineSwitch(endpoint_id.clone()),
                });
            }
            if profile.enabled && !online {
                items.push(ClientPaletteItem {
                    id: format!("machine:connect:{}", profile.id.as_str()),
                    title: fill(global_menu.machine_connect_fmt),
                    subtitle: profile.target.clone(),
                    badge: false,
                    action: ClientPaletteAction::MachineConnect(profile.id.clone()),
                });
            }
            items.push(ClientPaletteItem {
                id: format!("machine:toggle:{}", profile.id.as_str()),
                title: fill(if profile.enabled {
                    global_menu.machine_disable_fmt
                } else {
                    global_menu.machine_enable_fmt
                }),
                subtitle: profile.target.clone(),
                badge: false,
                action: ClientPaletteAction::MachineToggleEnabled(
                    profile.id.clone(),
                    !profile.enabled,
                ),
            });
            items.push(ClientPaletteItem {
                id: format!("machine:edit:{}", profile.id.as_str()),
                title: fill(global_menu.machine_edit_fmt),
                subtitle: profile.target.clone(),
                badge: false,
                action: ClientPaletteAction::MachineEdit(profile.id.clone()),
            });
        }
        items
    }

    pub(super) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            self.overlay = None;
        } else {
            let items = self.build_palette_items();
            let recent_ids = self.palette_recent.clone();
            self.overlay = Some(ClientShellOverlay::CommandPalette(
                ClientCommandPaletteOverlay {
                    query: TextEditor::default(),
                    selected: 0,
                    scroll: 0,
                    items,
                    recent_ids,
                },
            ));
        }
    }

    pub(super) fn move_palette_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_ref() else {
            return;
        };
        let count = palette_rows(palette).len();
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return;
        };
        if count == 0 {
            palette.selected = 0;
            palette.scroll = 0;
            return;
        }
        palette.selected =
            (palette.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
    }

    pub(super) fn set_palette_selection(&mut self, index: usize) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        let changed = palette.selected != index;
        palette.selected = index;
        changed
    }

    pub(super) fn scroll_palette(&mut self, delta: isize) {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return;
        };
        palette.scroll = palette.scroll.saturating_add_signed(delta);
    }

    pub(super) fn activate_palette_item(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let (id, action) = {
            let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_ref() else {
                return;
            };
            let rows = palette_rows(palette);
            let Some(row) = rows.get(index) else {
                return;
            };
            (row.item.id.clone(), row.item.action.clone())
        };
        self.overlay = None;
        self.palette_recent.retain(|entry| entry != &id);
        self.palette_recent.insert(0, id);
        self.palette_recent.truncate(PALETTE_RECENT_LIMIT);
        self.persist_chrome_preferences(outcome);
        match action {
            ClientPaletteAction::Binding(action) => {
                self.record_binding(crate::input::KeybindMatch::Action(action), outcome)
            }
            ClientPaletteAction::CustomCommand(index) => {
                if let Some(command) = self
                    .config
                    .keybinds
                    .keybinds
                    .custom_commands
                    .get(index)
                    .cloned()
                {
                    self.record_binding(crate::input::KeybindMatch::Command(command), outcome);
                }
            }
            ClientPaletteAction::Notifications => self.open_notification_history(),
            ClientPaletteAction::WhatsNew => self.open_release_notes(),
            ClientPaletteAction::MachineConnect(profile_id) => {
                self.machine_reconnect(&profile_id, outcome)
            }
            ClientPaletteAction::MachineSwitch(endpoint_id) => {
                self.activate_endpoint(endpoint_id, outcome);
            }
            ClientPaletteAction::MachineEdit(profile_id) => {
                self.open_machine_edit_form(&profile_id)
            }
            ClientPaletteAction::MachineToggleEnabled(profile_id, enabled) => {
                self.machine_set_enabled(&profile_id, enabled)
            }
            ClientPaletteAction::MachineImport => self.open_machine_import_wizard(),
            ClientPaletteAction::SnippetsList => self.open_snippets_overlay(false),
            ClientPaletteAction::SnippetRun => self.open_snippets_overlay(true),
            ClientPaletteAction::SceneSave => self.open_scenes_overlay_saving(),
            ClientPaletteAction::SceneList => self.open_scenes_overlay(),
            ClientPaletteAction::Broadcast => self.open_broadcast_overlay(),
        }
        outcome.repaint = true;
    }
}

pub(crate) fn render_command_palette(
    b: &mut Buffer,
    palette: &ClientCommandPaletteOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().global_menu;
    let (outer, inner) = modal_panel(b, crate::ui::ModalSize::Large, p.accent, cx)?;
    if inner.width < 16 || inner.height < 6 {
        return Some(OverlayRender {
            area: outer,
            ..OverlayRender::default()
        });
    }
    let rows = palette_rows(palette);
    let cursor = render_search_bar(
        b,
        Rect::new(inner.x, inner.y, inner.width, 1),
        &SearchBar {
            focused: true,
            query: &palette.query,
            hint: t.search_hint,
            status: None,
            echo_query: false,
            count: Some(format!("{}", rows.len())),
        },
        p,
    );
    put_text(
        b,
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        &"─".repeat(usize::from(inner.width)),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );
    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(3),
    );
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        if !body.is_empty() {
            put_text(
                b,
                body.x,
                body.y,
                body.width,
                t.no_matches,
                Style::default().fg(p.overlay0).bg(p.panel_bg),
            );
        }
    } else {
        let viewport = usize::from(body.height.max(1));
        let selected = palette.selected.min(rows.len().saturating_sub(1));
        let max_scroll = rows.len().saturating_sub(viewport);
        let scroll = palette
            .scroll
            .max(selected.saturating_sub(viewport.saturating_sub(1)))
            .min(selected)
            .min(max_scroll);
        let recent_marker = format!("{} ", t.recent);
        for (row_offset, index) in (scroll..rows.len()).take(viewport).enumerate() {
            let Some(row) = rows.get(index) else {
                break;
            };
            let rect = Rect::new(
                body.x,
                body.y.saturating_add(row_offset as u16),
                body.width,
                1,
            );
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
            let marker = if row.recent {
                recent_marker.as_str()
            } else {
                "  "
            };
            let marker_width = display_width(marker);
            put_text(
                b,
                rect.x,
                rect.y,
                marker_width.min(rect.width),
                marker,
                style,
            );
            let title_x = rect.x.saturating_add(marker_width);
            let subtitle_width = if row.item.subtitle.is_empty() {
                0
            } else {
                display_width(&row.item.subtitle).saturating_add(2)
            };
            let badge_width = u16::from(row.item.badge) * 2;
            let title_width = rect
                .width
                .saturating_sub(marker_width + subtitle_width + badge_width);
            let mut title_used = 0u16;
            let mut title_x = title_x;
            for (char_index, ch) in row.item.title.chars().enumerate() {
                let ch_width =
                    u16::try_from(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                        .unwrap_or(u16::MAX);
                if title_used.saturating_add(ch_width) > title_width {
                    break;
                }
                let matched = row.match_indices.contains(&char_index);
                let ch_style = if matched && !is_selected {
                    style.fg(p.mauve).add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                put_text(b, title_x, rect.y, ch_width, &ch.to_string(), ch_style);
                title_x = title_x.saturating_add(ch_width);
                title_used = title_used.saturating_add(ch_width);
            }
            if row.item.badge {
                let badge_style = if is_selected {
                    style
                } else {
                    Style::default()
                        .fg(p.accent)
                        .bg(p.panel_bg)
                        .add_modifier(Modifier::BOLD)
                };
                put_text(
                    b,
                    rect.right().saturating_sub(badge_width),
                    rect.y,
                    badge_width,
                    " ●",
                    badge_style,
                );
            }
            if subtitle_width > 0 {
                let subtitle_rect = Rect::new(
                    rect.right()
                        .saturating_sub(subtitle_width.saturating_add(badge_width)),
                    rect.y,
                    subtitle_width,
                    1,
                );
                put_right_text(
                    b,
                    subtitle_rect,
                    rect.y,
                    &row.item.subtitle,
                    if is_selected {
                        style
                    } else {
                        Style::default().fg(p.overlay0).bg(p.panel_bg)
                    },
                );
            }
            row_hits.push((rect, index));
        }
    }
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    render_key_hints(
        b,
        footer,
        &[
            ("enter".to_owned(), t.footer_run.to_owned()),
            ("↑↓".to_owned(), t.footer_select.to_owned()),
            ("esc".to_owned(), t.footer_close.to_owned()),
        ],
        p,
        cx.components,
    );
    Some(OverlayRender {
        area: outer,
        menu_rows: row_hits,
        cursor,
        ..OverlayRender::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_requires_subsequence_in_order() {
        assert!(fuzzy_match("nt", "new tab").is_some());
        assert!(fuzzy_match("tn", "new tab").is_none());
        assert!(fuzzy_match("zz", "new tab").is_none());
        assert_eq!(fuzzy_match("", "anything"), Some((0, Vec::new())));
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_and_reports_char_indices() {
        let (score, indices) = fuzzy_match("NT", "New Tab").expect("match");
        assert_eq!(indices, vec![0, 4]);
        assert!(score > 0);
    }

    #[test]
    fn fuzzy_match_prefers_consecutive_and_word_start_runs() {
        let (run_score, _) = fuzzy_match("tab", "tab").expect("exact");
        let (spread_score, _) = fuzzy_match("tab", "t a   b").expect("spread");
        assert!(run_score > spread_score);
    }

    #[test]
    fn fuzzy_match_handles_cjk_titles() {
        let (_, indices) = fuzzy_match("机器", "连接 机器 面板").expect("cjk match");
        assert_eq!(indices.len(), 2);
        assert!(fuzzy_match("不存在xyz", "连接 机器 面板").is_none());
    }
}
