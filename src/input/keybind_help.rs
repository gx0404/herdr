use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::{
    config::{ActionKeybinds, IndexedKeybind, Keybinds},
    input::TerminalKey,
};

pub(crate) type KeybindHelpEntry = (String, Cow<'static, str>);
pub(crate) type KeybindHelpGroup = (&'static str, Vec<KeybindHelpEntry>);

pub(crate) fn keybind_help_text_char(key: &TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    if let Some(character) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(character);
    }
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    Some(character)
}

fn entry(key: impl Into<String>, label: &'static str) -> KeybindHelpEntry {
    (key.into(), Cow::Borrowed(label))
}

fn binding_label(bindings: &ActionKeybinds) -> String {
    bindings
        .label()
        .unwrap_or_else(|| crate::i18n::texts().keybinds.unset.to_owned())
}

fn indexed_label(bindings: &[IndexedKeybind]) -> String {
    if bindings.is_empty() {
        return crate::i18n::texts().keybinds.unset.to_owned();
    }
    let mut parts = Vec::new();
    let mut index = 0;
    while index < bindings.len() {
        if let Some(prefix) = indexed_range_prefix(&bindings[index..]) {
            parts.push(format!("{prefix}1..9"));
            index += 9;
        } else {
            parts.push(bindings[index].label.clone());
            index += 1;
        }
    }
    parts.join(" / ")
}

fn indexed_range_prefix(bindings: &[IndexedKeybind]) -> Option<&str> {
    let run = bindings.get(..9)?;
    let prefix = run[0].label.strip_suffix('1')?;
    for (offset, binding) in run.iter().enumerate() {
        let digit = char::from(b'1' + offset as u8);
        if binding.label.strip_suffix(digit) != Some(prefix) {
            return None;
        }
    }
    Some(prefix)
}

pub(crate) fn keybind_help_groups(
    keybinds: &Keybinds,
    prefix: (crossterm::event::KeyCode, crossterm::event::KeyModifiers),
) -> Vec<KeybindHelpGroup> {
    let t = &crate::i18n::texts().keybinds;
    let mut groups = vec![
        (
            t.group_global,
            vec![
                entry(crate::config::format_key_combo(prefix), t.prefix_mode),
                entry(
                    binding_label(&keybinds.main_menu),
                    crate::i18n::texts().global_menu.main_menu,
                ),
                entry(
                    binding_label(&keybinds.command_search),
                    crate::i18n::texts().global_menu.command_search,
                ),
                entry(binding_label(&keybinds.help), t.keybinds),
                entry(binding_label(&keybinds.settings), t.settings),
                entry(binding_label(&keybinds.manage_machines), t.manage_machines),
                entry(binding_label(&keybinds.detach), t.detach),
                entry(binding_label(&keybinds.reload_config), t.reload_config),
                entry(
                    binding_label(&keybinds.open_notification_target),
                    t.open_notification_target,
                ),
            ],
        ),
        (
            t.group_navigation,
            vec![
                entry("esc", t.back),
                entry(
                    format!(
                        "{} / {}",
                        binding_label(&keybinds.navigate.workspace_up),
                        binding_label(&keybinds.navigate.workspace_down)
                    ),
                    t.workspace_list,
                ),
                entry(
                    format!(
                        "{} / {} / {} / {} / left / right",
                        binding_label(&keybinds.navigate.pane_left),
                        binding_label(&keybinds.navigate.pane_down),
                        binding_label(&keybinds.navigate.pane_up),
                        binding_label(&keybinds.navigate.pane_right)
                    ),
                    t.move_focus,
                ),
                entry("tab / shift+tab", t.cycle_pane),
                entry("enter", t.open_workspace),
                entry("1..9", t.switch_workspace),
            ],
        ),
        (
            t.group_workspaces_tabs,
            vec![
                entry(
                    binding_label(&keybinds.workspace_picker),
                    t.workspace_navigation,
                ),
                entry(binding_label(&keybinds.goto), t.session_navigator),
                entry(binding_label(&keybinds.new_workspace), t.new_workspace),
                entry(binding_label(&keybinds.new_worktree), t.new_worktree),
                entry(binding_label(&keybinds.open_worktree), t.open_worktree),
                entry(
                    binding_label(&keybinds.remove_worktree),
                    t.delete_worktree_checkout,
                ),
                entry(
                    binding_label(&keybinds.rename_workspace),
                    t.rename_workspace,
                ),
                entry(binding_label(&keybinds.close_workspace), t.close_workspace),
                entry(
                    binding_label(&keybinds.previous_workspace),
                    t.previous_workspace,
                ),
                entry(binding_label(&keybinds.next_workspace), t.next_workspace),
                entry(
                    indexed_label(&keybinds.switch_workspace),
                    t.switch_workspace_1_9,
                ),
                entry(binding_label(&keybinds.previous_agent), t.previous_agent),
                entry(binding_label(&keybinds.next_agent), t.next_agent),
                entry(indexed_label(&keybinds.focus_agent), t.focus_agent_1_9),
                entry(binding_label(&keybinds.new_tab), t.new_tab),
                entry(binding_label(&keybinds.rename_tab), t.rename_tab),
                entry(binding_label(&keybinds.previous_tab), t.previous_tab),
                entry(binding_label(&keybinds.next_tab), t.next_tab),
                entry(binding_label(&keybinds.move_tab_previous), t.move_tab_left),
                entry(binding_label(&keybinds.move_tab_next), t.move_tab_right),
                entry(indexed_label(&keybinds.switch_tab), t.switch_tab_1_9),
                entry(binding_label(&keybinds.close_tab), t.close_tab),
            ],
        ),
        (
            t.group_panes,
            vec![
                entry(binding_label(&keybinds.split_vertical), t.split_vertical),
                entry(
                    binding_label(&keybinds.split_horizontal),
                    t.split_horizontal,
                ),
                entry(binding_label(&keybinds.close_pane), t.close_pane),
                entry(binding_label(&keybinds.rename_pane), t.rename_pane),
                entry(binding_label(&keybinds.edit_scrollback), t.edit_scrollback),
                entry(binding_label(&keybinds.copy_mode), t.copy_mode),
                entry(binding_label(&keybinds.link_hints), t.link_hints),
                entry(binding_label(&keybinds.zoom), t.zoom_pane),
                entry(binding_label(&keybinds.resize_mode), t.resize_mode),
                entry(
                    binding_label(&keybinds.resize_pane_left),
                    t.resize_pane_left,
                ),
                entry(
                    binding_label(&keybinds.resize_pane_down),
                    t.resize_pane_down,
                ),
                entry(binding_label(&keybinds.resize_pane_up), t.resize_pane_up),
                entry(
                    binding_label(&keybinds.resize_pane_right),
                    t.resize_pane_right,
                ),
                entry(binding_label(&keybinds.toggle_sidebar), t.toggle_sidebar),
                entry(binding_label(&keybinds.focus_pane_left), t.focus_pane_left),
                entry(binding_label(&keybinds.focus_pane_down), t.focus_pane_down),
                entry(binding_label(&keybinds.focus_pane_up), t.focus_pane_up),
                entry(
                    binding_label(&keybinds.focus_pane_right),
                    t.focus_pane_right,
                ),
                entry(binding_label(&keybinds.cycle_pane_next), t.cycle_pane_next),
                entry(
                    binding_label(&keybinds.cycle_pane_previous),
                    t.cycle_pane_previous,
                ),
                entry(binding_label(&keybinds.last_pane), t.last_pane),
            ],
        ),
    ];

    if !keybinds.custom_commands.is_empty() {
        groups.push((
            t.group_custom,
            keybinds
                .custom_commands
                .iter()
                .map(|binding| {
                    (
                        binding.label.clone(),
                        binding
                            .description
                            .clone()
                            .map(Cow::Owned)
                            .unwrap_or(Cow::Borrowed(t.custom_command)),
                    )
                })
                .collect(),
        ));
    }
    groups
}

pub(crate) fn filter_keybind_help_groups(
    groups: Vec<KeybindHelpGroup>,
    query: &str,
) -> Vec<KeybindHelpGroup> {
    if query.is_empty() {
        return groups;
    }
    let query = query.to_lowercase();
    groups
        .into_iter()
        .filter_map(|(group, entries)| {
            let entries = entries
                .into_iter()
                .filter(|(key, label)| {
                    key.to_lowercase().contains(&query) || label.to_lowercase().contains(&query)
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then_some((group, entries))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> Vec<KeybindHelpGroup> {
        vec![
            (
                "workspaces / tabs",
                vec![entry("w", "workspace navigation"), entry("c", "new tab")],
            ),
            (
                "panes",
                vec![entry("v", "split vertical"), entry("x", "close pane")],
            ),
        ]
    }

    #[test]
    fn filter_matches_labels_and_shortcuts_case_insensitively() {
        let filtered = filter_keybind_help_groups(groups(), "WoRk");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "workspace navigation");

        let filtered = filter_keybind_help_groups(groups(), "x");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "close pane");
        assert!(filter_keybind_help_groups(groups(), "panes").is_empty());
    }
}
