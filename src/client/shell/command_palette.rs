use super::feedback::ChromeContext;
use super::render::{
    display_width, modal_panel, put_right_text, put_text, render_key_hints, render_search_bar,
    OverlayRender, SearchBar,
};
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// Cap on the persisted most-recently-used command list.
pub(super) const PALETTE_RECENT_LIMIT: usize = 5;

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
    CustomCommand(crate::config::CustomCommandKeybind),
    Category(usize),
    Search,
    Back,
    Observation(super::observability::Page),
    CloseMonitor,
    Arrange,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserView {
    Menu(Option<usize>),
    Search,
}

fn category(item: &ClientPaletteItem) -> usize {
    let id = item.id.as_str();
    if id.starts_with("machine:") || id == "binding:ManageMachines" {
        2
    } else if id.starts_with("observation:") {
        3
    } else if id.starts_with("command:")
        || id.starts_with("snippets:")
        || id.starts_with("scene:")
        || id == "broadcast"
    {
        4
    } else if id.contains("Workspace")
        || id.contains("Worktree")
        || id.contains("Agent")
        || id == "binding:OpenNavigator"
    {
        0
    } else if id.contains("Pane")
        || id.contains("Tab")
        || [
            "binding:CopyMode",
            "binding:EditScrollback",
            "binding:Zoom",
            "binding:SplitVertical",
            "binding:SplitHorizontal",
            "binding:EnterResizeMode",
            "layout",
        ]
        .contains(&id)
    {
        1
    } else if [
        "binding:Settings",
        "binding:ReloadConfig",
        "binding:ToggleSidebar",
    ]
    .contains(&id)
    {
        5
    } else {
        6
    }
}

fn navigation(item: &ClientPaletteItem) -> bool {
    matches!(
        item.action,
        ClientPaletteAction::Category(_) | ClientPaletteAction::Search | ClientPaletteAction::Back
    )
}

/// Command palette overlay state: the item index is built once at open and
/// `recent_ids` snapshots the MRU so ordering stays stable while open.
#[derive(Debug)]
pub(super) struct ClientCommandPaletteOverlay {
    pub(super) view: BrowserView,
    pub(super) reveal: bool,
    pub(super) focus: super::page::PageFocus,
    pub(super) query: TextEditor,
    pub(super) selected: usize,
    /// 指针悬浮行：只由 `Moved` 改写，`selected` 只由键盘与点击改写（MENU-01）。
    pub(super) hovered: Option<usize>,
    pub(super) scroll: usize,
    pub(super) items: Vec<ClientPaletteItem>,
    pub(super) recent_ids: Vec<String>,
    pub(super) aliases: HashMap<String, String>,
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
    let lowered: Vec<(usize, char)> = text_chars
        .iter()
        .enumerate()
        .flat_map(|(index, c)| c.to_lowercase().map(move |ch| (index, ch)))
        .collect();
    let mut query_index = 0;
    let mut indices = Vec::with_capacity(query_chars.len());
    let mut score = 0i64;
    let mut previous_match = None;
    for &(text_index, ch) in &lowered {
        if query_index >= query_chars.len() || ch != query_chars[query_index] {
            continue;
        }
        if indices.last() != Some(&text_index) {
            indices.push(text_index);
        }
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
        let mut rows = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if !matches!(palette.view, BrowserView::Menu(Some(_))) {
            for id in palette.recent_ids.iter().take(PALETTE_RECENT_LIMIT) {
                if let Some(item) = palette
                    .items
                    .iter()
                    .find(|item| &item.id == id && !navigation(item))
                {
                    if seen.insert(item.id.as_str()) {
                        rows.push(ClientPaletteRow {
                            item,
                            match_indices: Vec::new(),
                            recent: true,
                        });
                    }
                }
            }
        }
        for item in &palette.items {
            let show = match palette.view {
                BrowserView::Search => !navigation(item),
                BrowserView::Menu(None) => matches!(
                    item.action,
                    ClientPaletteAction::Category(_) | ClientPaletteAction::Search
                ),
                BrowserView::Menu(Some(group)) => {
                    (!navigation(item) && category(item) == group)
                        || matches!(item.action, ClientPaletteAction::Back)
                }
            };
            if show && seen.insert(item.id.as_str()) {
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
            if navigation(item) {
                return None;
            }
            let aliases = format!(
                "{} {} {} {} {}",
                item.id,
                item.subtitle,
                palette
                    .aliases
                    .get(&item.id)
                    .map(String::as_str)
                    .unwrap_or_default(),
                crate::i18n::en::TEXTS.global_menu.categories[category(item)],
                crate::i18n::zh_cn::TEXTS.global_menu.categories[category(item)]
            );
            fuzzy_match(query, &item.title)
                .map(|(score, indices)| (score + 50, indices))
                .or_else(|| fuzzy_match(query, &aliases).map(|(score, _)| (score, Vec::new())))
                .map(|(score, indices)| {
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

fn palette_binding_items(
    items: &mut Vec<ClientPaletteItem>,
    keybinds: &crate::config::Keybinds,
    t: &crate::i18n::Texts,
) {
    use crate::input::KeybindAction;
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
        palette_binding_items(
            &mut items,
            &self.config.keybinds.keybinds,
            crate::i18n::texts(),
        );
        if let Some(snapshot) = self.snapshot.as_deref() {
            if snapshot.integration_updates_available {
                if let Some(item) = items.iter_mut().find(|item| item.id == "binding:Settings") {
                    item.badge = true;
                }
            }
        }
        for command in self.config.keybinds.keybinds.custom_commands.iter() {
            items.push(ClientPaletteItem {
                id: format!("command:{}", command.command),
                title: command
                    .description
                    .clone()
                    .unwrap_or_else(|| command.command.clone()),
                subtitle: command.label.clone(),
                badge: false,
                action: ClientPaletteAction::CustomCommand(command.clone()),
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
        items.push(ClientPaletteItem {
            id: "observation:monitor".into(),
            title: super::observability::tr(
                "Monitor (system · accounts · settings)",
                "监控（系统 · 账号 · 设置）",
            )
            .into(),
            subtitle: String::new(),
            badge: false,
            action: ClientPaletteAction::Observation(super::observability::Page::Monitor),
        });
        if self.workbench.enabled
            && self
                .workbench
                .dock
                .root
                .contains(&super::dock::PanelId::Monitor)
        {
            // 终端聚焦时 Esc 进终端，命令面板提供显式关闭停靠监控面板的入口。
            items.push(ClientPaletteItem {
                id: "observation:close-monitor".into(),
                title: super::observability::tr("Close monitor panel", "关闭监控面板").into(),
                subtitle: String::new(),
                badge: false,
                action: ClientPaletteAction::CloseMonitor,
            });
        }
        items.push(ClientPaletteItem {
            id: "layout".into(),
            title: super::observability::tr("Arrange panels", "调整面板布局").into(),
            subtitle: String::new(),
            badge: false,
            action: ClientPaletteAction::Arrange,
        });
        for (index, title) in global_menu.categories.iter().enumerate() {
            let count = items.iter().filter(|item| category(item) == index).count();
            let badge = items
                .iter()
                .any(|item| category(item) == index && item.badge);
            items.push(ClientPaletteItem {
                id: format!("category:{index}"),
                title: title.to_string(),
                subtitle: format!("{count}  ›"),
                badge,
                action: ClientPaletteAction::Category(index),
            });
        }
        for (id, title, action) in [
            (
                "search",
                global_menu.command_search,
                ClientPaletteAction::Search,
            ),
            ("back", global_menu.back, ClientPaletteAction::Back),
        ] {
            items.push(ClientPaletteItem {
                id: id.into(),
                title: title.into(),
                subtitle: if id == "search" {
                    self.config
                        .keybinds
                        .keybinds
                        .command_search
                        .label()
                        .unwrap_or_default()
                } else {
                    String::new()
                },
                badge: false,
                action,
            });
        }
        items
    }

    pub(super) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            self.close_command_browser();
        } else {
            self.open_command_browser(BrowserView::Menu(None));
        }
    }

    pub(super) fn open_command_search(&mut self) {
        self.open_command_browser(BrowserView::Search);
    }

    fn open_command_browser(&mut self, view: BrowserView) {
        self.cancel_frozen_selection();
        if !matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            self.browser_return = self.overlay.take().map(Box::new);
        }
        let mut aliases = HashMap::<String, String>::new();
        for texts in [&crate::i18n::en::TEXTS, &crate::i18n::zh_cn::TEXTS] {
            let mut translated = Vec::new();
            palette_binding_items(&mut translated, &self.config.keybinds.keybinds, texts);
            for item in translated {
                let entry = aliases.entry(item.id).or_default();
                entry.push(' ');
                entry.push_str(&item.title);
            }
        }
        self.overlay = Some(ClientShellOverlay::CommandPalette(
            ClientCommandPaletteOverlay {
                focus: if view == BrowserView::Search {
                    super::page::PageFocus::Search
                } else {
                    super::page::PageFocus::Navigation
                },
                view,
                aliases,
                reveal: true,
                query: TextEditor::default(),
                selected: 0,
                hovered: None,
                scroll: 0,
                items: self.build_palette_items(),
                recent_ids: self.palette_recent.clone(),
            },
        ));
    }

    pub(super) fn close_command_browser(&mut self) {
        self.overlay = self.browser_return.take().map(|page| *page);
    }

    pub(super) fn browser_back(&mut self) {
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            if matches!(palette.view, BrowserView::Menu(Some(_))) {
                palette.view = BrowserView::Menu(None);
                palette.query = TextEditor::default();
                palette.selected = 0;
                palette.scroll = 0;
                palette.reveal = true;
                // 行集合整个换了，旧行号立刻失效：不清就会在新列表里把同号的
                // 那一行画成「悬浮」，而指针其实停在别处（MENU-01）。
                palette.hovered = None;
                return;
            }
        }
        self.close_command_browser();
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
        palette.reveal = true;
        palette.selected =
            (palette.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
        // reveal 会在渲染期重算 scroll，指针下面的行可能已经换了。
        palette.hovered = None;
    }

    pub(super) fn set_palette_selection(&mut self, index: usize) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        let changed = palette.selected != index;
        palette.selected = index;
        changed
    }

    /// 指针悬浮行。`None` 表示指针不在任何行上——出界也要写，否则高亮会留在
    /// 鼠标早已离开的那一行（MENU-01）。
    pub(super) fn set_palette_hover(&mut self, hovered: Option<usize>) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        let changed = palette.hovered != hovered;
        palette.hovered = hovered;
        changed
    }

    pub(super) fn scroll_palette(&mut self, delta: isize) {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return;
        };
        palette.reveal = false;
        palette.scroll = palette.scroll.saturating_add_signed(delta);
        // 滚动会把别的行挪到指针下面，旧的 hover 行号立刻失效。
        palette.hovered = None;
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
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            let next = match action {
                ClientPaletteAction::Category(index) => Some(BrowserView::Menu(Some(index))),
                ClientPaletteAction::Search => Some(BrowserView::Search),
                ClientPaletteAction::Back => Some(BrowserView::Menu(None)),
                _ => None,
            };
            if let Some(view) = next {
                palette.view = view;
                palette.selected = 0;
                palette.scroll = 0;
                palette.reveal = true;
                palette.query = TextEditor::default();
                // 点击分类进子菜单之后不会再来一个 `Moved`，旧行号会立刻在新
                // 列表里画出一条假的悬浮行（MENU-01）。
                palette.hovered = None;
                outcome.repaint = true;
                return;
            }
        }
        self.close_command_browser();
        self.palette_recent.retain(|entry| entry != &id);
        self.palette_recent.insert(0, id);
        self.palette_recent.truncate(PALETTE_RECENT_LIMIT);
        self.persist_chrome_preferences(outcome);
        match action {
            ClientPaletteAction::Binding(action) => {
                self.record_binding(crate::input::KeybindMatch::Action(action), outcome)
            }
            ClientPaletteAction::CustomCommand(command) => {
                self.record_binding(crate::input::KeybindMatch::Command(command), outcome);
            }
            ClientPaletteAction::Observation(page) => self.open_observation_page(page, outcome),
            ClientPaletteAction::CloseMonitor => {
                self.close_workbench_panel(super::dock::PanelId::Monitor, outcome);
            }
            ClientPaletteAction::Arrange => {
                self.workbench.arranging = true;
            }
            ClientPaletteAction::Category(_)
            | ClientPaletteAction::Search
            | ClientPaletteAction::Back => {}
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

/// 命令面板的列表窗口：行投影（`recent` 分组占行）与几何一次算好，视图计算
/// 阶段与渲染阶段共用（STATE-04）。
pub(crate) fn palette_window(
    area: Rect,
    page_bounds: Option<Rect>,
    palette: &ClientCommandPaletteOverlay,
) -> Option<crate::client::shell::page::ListWindow> {
    let rows = palette_rows(palette);
    let menu_height = match palette.view {
        BrowserView::Menu(_) if palette.query.as_str().is_empty() => {
            (rows.len().saturating_add(6).min(22)) as u16
        }
        _ => 22,
    };
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| {
            crate::ui::modal_rect(area, crate::ui::ModalSize::Large.with_height(menu_height))
        })?;
    let inner = super::render::panel_inner(outer)?;
    let searching = palette.view == BrowserView::Search || !palette.query.as_str().is_empty();
    let layout = super::page::PageLayout::new(inner, 0, searching, false);
    let mut visual = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if (index == 0 && row.recent) || (index > 0 && rows[index - 1].recent && !row.recent) {
            visual.push(None);
        }
        visual.push(Some(index));
    }
    let selected = palette.selected.min(rows.len().saturating_sub(1));
    let selected_line = visual
        .iter()
        .position(|entry| *entry == Some(selected))
        .unwrap_or(0);
    Some(super::page::list_window(
        layout.content,
        1,
        visual.len(),
        palette.scroll,
        selected_line,
        palette.reveal,
    ))
}

pub(crate) fn render_command_palette(
    b: &mut Buffer,
    palette: &ClientCommandPaletteOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    use super::page::{list_start, PageLayout};
    let p = cx.palette;
    let t = &crate::i18n::texts().global_menu;
    let rows = palette_rows(palette);
    let menu_height = match palette.view {
        BrowserView::Menu(_) if palette.query.as_str().is_empty() => {
            (rows.len().saturating_add(6).min(22)) as u16
        }
        _ => 22,
    };
    let (outer, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Large.with_height(menu_height),
        p.accent,
        cx,
    )?;
    let searching = palette.view == BrowserView::Search || !palette.query.as_str().is_empty();
    let layout = PageLayout::new(inner, 0, searching, false);
    let title = match palette.view {
        BrowserView::Search => t.command_search,
        BrowserView::Menu(None) => t.main_menu,
        BrowserView::Menu(Some(group)) => t.categories[group],
    };
    put_text(
        b,
        layout.header.x,
        layout.header.y,
        layout.header.width,
        title,
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    );
    let cursor = if searching {
        render_search_bar(
            b,
            layout.search,
            &SearchBar {
                focused: palette.focus == super::page::PageFocus::Search || searching,
                query: &palette.query,
                hint: t.search_hint,
                status: None,
                echo_query: false,
                count: Some(rows.len().to_string()),
            },
            p,
        )
    } else {
        None
    };
    let body = layout.content;
    let mut visual = Vec::new();
    let has_recent = rows.first().is_some_and(|row| row.recent);
    for (index, row) in rows.iter().enumerate() {
        if (index == 0 && row.recent) || (index > 0 && rows[index - 1].recent && !row.recent) {
            visual.push(None);
        }
        visual.push(Some(index));
    }
    let selected = palette.selected.min(rows.len().saturating_sub(1));
    let selected_line = visual
        .iter()
        .position(|entry| *entry == Some(selected))
        .unwrap_or(0);
    let scroll = list_start(
        palette.scroll,
        selected_line,
        visual.len(),
        usize::from(body.height),
        palette.reveal,
    );
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            t.no_matches,
            Style::default().fg(p.overlay0),
        );
    }
    for (offset, visual_index) in (scroll..visual.len())
        .take(usize::from(body.height))
        .enumerate()
    {
        let rect = Rect::new(
            body.x,
            body.y + offset as u16,
            body.width.saturating_sub(1),
            1,
        );
        let Some(index) = visual[visual_index] else {
            put_text(
                b,
                rect.x,
                rect.y,
                rect.width,
                if visual_index == 0 && has_recent {
                    t.recent
                } else {
                    title
                },
                Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
            );
            continue;
        };
        let row = &rows[index];
        let chosen = index == selected;
        let style = list_row_style(p, cx.components, chosen, palette.hovered == Some(index));
        b.set_style(rect, style);
        let marker_width = 2.min(rect.width);
        put_text(
            b,
            rect.x,
            rect.y,
            marker_width,
            if chosen { "› " } else { "  " },
            style,
        );
        let badge_width = if row.item.badge { 2 } else { 0 };
        let available = rect.width.saturating_sub(marker_width + badge_width);
        let subtitle_width = if available < 28 || row.item.subtitle.is_empty() {
            0
        } else {
            display_width(&row.item.subtitle)
                .saturating_add(1)
                .min(available / 2)
        };
        let title_width = available.saturating_sub(subtitle_width);
        let mut x = rect.x + marker_width;
        let mut used = 0;
        use unicode_segmentation::UnicodeSegmentation;
        let mut char_index = 0;
        for grapheme in row.item.title.graphemes(true) {
            let width = display_width(grapheme);
            if used + width > title_width {
                break;
            }
            let end = char_index + grapheme.chars().count();
            let matched = row
                .match_indices
                .iter()
                .any(|index| *index >= char_index && *index < end);
            let emphasis = if !chosen && matched {
                style.fg(p.mauve).add_modifier(Modifier::BOLD)
            } else {
                style
            };
            put_text(b, x, rect.y, width, grapheme, emphasis);
            char_index = end;
            x += width;
            used += width;
        }
        if subtitle_width > 0 {
            let subtitle = Rect::new(
                rect.right() - badge_width - subtitle_width,
                rect.y,
                subtitle_width,
                1,
            );
            put_right_text(
                b,
                subtitle,
                rect.y,
                &row.item.subtitle,
                if chosen { style } else { style.fg(p.overlay0) },
            );
        }
        if row.item.badge {
            put_text(
                b,
                rect.right().saturating_sub(2),
                rect.y,
                2,
                " ●",
                if chosen { style } else { style.fg(p.accent) },
            );
        }
        row_hits.push((rect, index));
    }
    if visual.len() > usize::from(body.height) && body.width > 0 && body.height > 0 {
        let y = body.y
            + ((scroll * usize::from(body.height)) / visual.len()).min(usize::from(body.height - 1))
                as u16;
        put_text(
            b,
            body.right() - 1,
            y,
            1,
            "┃",
            Style::default().fg(p.accent),
        );
    }
    render_key_hints(
        b,
        layout.footer,
        &[
            ("enter".into(), t.footer_run.into()),
            ("↑↓".into(), t.footer_select.into()),
            ("/".into(), t.command_search.into()),
            (
                "esc".into(),
                if matches!(palette.view, BrowserView::Menu(Some(_))) {
                    t.back
                } else {
                    t.footer_close
                }
                .into(),
            ),
        ],
        p,
        cx.components,
    );
    Some(OverlayRender {
        area: outer,
        menu_popup: outer,
        menu_search: layout.search,
        menu_rows: row_hits,
        cursor,
        ..OverlayRender::default()
    })
}

impl ClientShellState {
    /// 命令面板 / 全局菜单打开时的鼠标分派：不在该浮层时返回 `false`，
    /// 由 `mouse.rs::handle_mouse` 继续往下走。
    pub(super) fn handle_command_palette_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            return false;
        }
        let row_hit = self
            .hits
            .global_menu_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .copied();
        match mouse.kind {
            MouseEventKind::Moved => {
                // 指针只写 hover：键盘选中不被「鼠标路过」改写，出界也要写
                // None 才不会留下残影（MENU-01）。
                outcome.repaint |= self.set_palette_hover(row_hit.map(|(_, index)| index));
            }
            MouseEventKind::ScrollUp => {
                self.scroll_palette(-(self.config.mouse_scroll_lines as isize));
                outcome.repaint = true;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_palette(self.config.mouse_scroll_lines as isize);
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if super::contains(self.hits.global_launcher, point) {
                    self.toggle_global_menu();
                    outcome.repaint = true;
                } else if let Some((_, index)) = row_hit {
                    // 点击是显式选择：与键盘一样改写 `selected`，再激活。
                    self.set_palette_selection(index);
                    self.activate_palette_item(index, outcome);
                } else if !super::contains(self.hits.menu_popup, point) {
                    self.close_command_browser();
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }
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
