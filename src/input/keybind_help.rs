use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::{
    config::{ActionKeybinds, IndexedKeybind, Keybinds},
    input::TerminalKey,
};

/// 帮助与 which-key 里一个绑定的一个按键。前缀绑定只记右手键（`g`），显示时再展开成
/// 实际的前缀键，这样帮助能写全「ctrl+b g」、which-key（已在前缀模式）只写「g」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelpKey {
    /// 前缀键本身（进入前缀模式；在前缀模式里再按一次是发送前缀）。
    Prefix(String),
    /// 前缀模式下接着按的键。
    Prefixed(String),
    /// 直接按键或固定写法（`esc`、`1..9`）。
    Direct(String),
}

impl HelpKey {
    /// 帮助里的完整按键序列：前缀绑定展开为 `prefix_label` 加右手键，写法统一规范成
    /// 小写口径（`ui::key_labels::display_key`）。
    pub(crate) fn sequence(&self, prefix_label: &str) -> String {
        use crate::ui::key_labels::display_key;
        match self {
            Self::Prefix(key) | Self::Direct(key) => display_key(key).into_owned(),
            Self::Prefixed(rhs) => format!("{} {}", display_key(prefix_label), display_key(rhs)),
        }
    }

    /// 已经按下前缀后接着要按的键（which-key 用）；直接按键返回 `None`。
    pub(crate) fn after_prefix(&self) -> Option<Cow<'_, str>> {
        use crate::ui::key_labels::display_key;
        match self {
            Self::Prefix(key) | Self::Prefixed(key) => Some(display_key(key)),
            Self::Direct(_) => None,
        }
    }

    /// 配置文件里的写法（`prefix+g`）：过滤时也认它，按配置语法搜得到。
    fn config_label(&self) -> Cow<'_, str> {
        match self {
            Self::Prefixed(rhs) => Cow::Owned(format!("prefix+{rhs}")),
            Self::Prefix(key) | Self::Direct(key) => Cow::Borrowed(key),
        }
    }

    /// 是不是前缀键本身。
    pub(crate) fn is_prefix_key(&self) -> bool {
        matches!(self, Self::Prefix(_))
    }
}

/// 帮助分组的稳定身份：分组、过滤与 which-key 按它判断，不拿本地化组名当身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeybindHelpGroupId {
    Global,
    Navigation,
    WorkspacesTabs,
    Panes,
    Custom,
}

impl KeybindHelpGroupId {
    /// 本地化的组标题。
    pub(crate) fn title(self) -> &'static str {
        let t = &crate::i18n::texts().keybinds;
        match self {
            Self::Global => t.group_global,
            Self::Navigation => t.group_navigation,
            Self::WorkspacesTabs => t.group_workspaces_tabs,
            Self::Panes => t.group_panes,
            Self::Custom => t.group_custom,
        }
    }

    /// which-key（前缀模式）里是否列出：导航模式的组不列。
    pub(crate) fn in_which_key(self) -> bool {
        self != Self::Navigation
    }
}

/// 一行帮助：按键（空 = 未设置）与说明。
pub(crate) type KeybindHelpEntry = (Vec<HelpKey>, Cow<'static, str>);
pub(crate) type KeybindHelpGroup = (KeybindHelpGroupId, Vec<KeybindHelpEntry>);

/// The character a key types, for overlay shortcuts and text fields.
pub(crate) fn keybind_help_text_char(key: &TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    // Text the host says the key produced is authoritative: kitty hosts may
    // report Shift+/ as `/` with text `?` and no shifted alternate.
    if let Some(text) = key.generated_text.as_deref() {
        let mut chars = text.chars();
        if let (Some(character), None) = (chars.next(), chars.next()) {
            if !character.is_control() {
                return Some(character);
            }
        }
    }
    if let Some(character) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(character);
    }
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    Some(character)
}

fn entry(keys: Vec<HelpKey>, label: &'static str) -> KeybindHelpEntry {
    (keys, Cow::Borrowed(label))
}

fn direct(keys: &[&str]) -> Vec<HelpKey> {
    keys.iter()
        .map(|key| HelpKey::Direct((*key).to_owned()))
        .collect()
}

/// 一个绑定的按键：前缀绑定只留右手键。
fn binding_key(prefix: bool, label: &str) -> HelpKey {
    if prefix {
        HelpKey::Prefixed(label.strip_prefix("prefix+").unwrap_or(label).to_owned())
    } else {
        HelpKey::Direct(label.to_owned())
    }
}

fn binding_label(bindings: &ActionKeybinds) -> Vec<HelpKey> {
    bindings
        .bindings
        .iter()
        .map(|binding| binding_key(binding.trigger.is_prefix(), &binding.label))
        .collect()
}

fn indexed_label(bindings: &[IndexedKeybind]) -> Vec<HelpKey> {
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bindings.len() {
        if let Some(prefix) = indexed_range_prefix(&bindings[index..]) {
            keys.push(binding_key(
                bindings[index].trigger.is_prefix(),
                &format!("{prefix}1..9"),
            ));
            index += 9;
        } else {
            keys.push(binding_key(
                bindings[index].trigger.is_prefix(),
                &bindings[index].label,
            ));
            index += 1;
        }
    }
    keys
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

/// 每个配置的前缀键各一个键帽（没配时是内置默认前缀）。
fn prefix_keys(prefixes: &[crate::config::KeyCombo]) -> Vec<HelpKey> {
    if prefixes.is_empty() {
        return vec![HelpKey::Prefix(crate::config::format_prefix_combos(
            prefixes,
        ))];
    }
    prefixes
        .iter()
        .map(|combo| HelpKey::Prefix(crate::config::format_key_combo(*combo)))
        .collect()
}

pub(crate) fn keybind_help_groups(
    keybinds: &Keybinds,
    prefixes: &[crate::config::KeyCombo],
) -> Vec<KeybindHelpGroup> {
    let t = &crate::i18n::texts().keybinds;
    let mut groups = vec![
        (
            KeybindHelpGroupId::Global,
            vec![
                entry(prefix_keys(prefixes), t.prefix_mode),
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
            KeybindHelpGroupId::Navigation,
            vec![
                entry(direct(&["esc"]), t.back),
                entry(
                    [
                        binding_label(&keybinds.navigate.workspace_up),
                        binding_label(&keybinds.navigate.workspace_down),
                    ]
                    .concat(),
                    t.workspace_list,
                ),
                entry(
                    [
                        binding_label(&keybinds.navigate.pane_left),
                        binding_label(&keybinds.navigate.pane_down),
                        binding_label(&keybinds.navigate.pane_up),
                        binding_label(&keybinds.navigate.pane_right),
                        direct(&["left", "right"]),
                    ]
                    .concat(),
                    t.move_focus,
                ),
                entry(direct(&["tab", "shift+tab"]), t.cycle_pane),
                entry(direct(&["enter"]), t.open_workspace),
                entry(direct(&["1..9"]), t.switch_workspace),
            ],
        ),
        (
            KeybindHelpGroupId::WorkspacesTabs,
            vec![
                entry(
                    binding_label(&keybinds.workspace_picker),
                    t.workspace_navigation,
                ),
                entry(binding_label(&keybinds.goto), t.session_navigator),
                entry(binding_label(&keybinds.next_attention), t.next_attention),
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
            KeybindHelpGroupId::Panes,
            vec![
                entry(binding_label(&keybinds.split_vertical), t.split_vertical),
                entry(
                    binding_label(&keybinds.split_horizontal),
                    t.split_horizontal,
                ),
                entry(binding_label(&keybinds.close_pane), t.close_pane),
                entry(binding_label(&keybinds.rename_pane), t.rename_pane),
                entry(binding_label(&keybinds.edit_scrollback), t.edit_scrollback),
                entry(binding_label(&keybinds.clear_pane), t.clear_pane),
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
                entry(
                    binding_label(&keybinds.toggle_inspector),
                    t.toggle_inspector,
                ),
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
            KeybindHelpGroupId::Custom,
            keybinds
                .custom_commands
                .iter()
                .map(|binding| {
                    (
                        binding_label(&binding.bindings),
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

/// 按查询过滤：匹配说明或显示出来的按键序列（前缀已展开，`prefix_label` 为实际前缀键），
/// 不区分大小写。
pub(crate) fn filter_keybind_help_groups(
    groups: Vec<KeybindHelpGroup>,
    query: &str,
    prefix_label: &str,
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
                .filter(|(keys, label)| {
                    label.to_lowercase().contains(&query)
                        || keys.iter().any(|key| {
                            key.sequence(prefix_label).to_lowercase().contains(&query)
                                || (query.contains("prefix")
                                    && key.config_label().to_lowercase().contains(&query))
                        })
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then_some((group, entries))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixed(key: &str) -> Vec<HelpKey> {
        vec![HelpKey::Prefixed(key.to_owned())]
    }

    fn groups() -> Vec<KeybindHelpGroup> {
        vec![
            (
                KeybindHelpGroupId::WorkspacesTabs,
                vec![
                    entry(prefixed("w"), "workspace navigation"),
                    entry(prefixed("c"), "new tab"),
                ],
            ),
            (
                KeybindHelpGroupId::Panes,
                vec![
                    entry(prefixed("v"), "split vertical"),
                    entry(direct(&["ctrl+x"]), "close pane"),
                ],
            ),
        ]
    }

    #[test]
    fn text_char_prefers_generated_text_then_alternate_then_code() {
        let parse = |bytes: &str| {
            crate::input::parse_terminal_key_sequence(bytes)
                .unwrap_or_else(|| panic!("{bytes:?} parses"))
        };
        for (bytes, want) in [
            // Shift+/ typing `?` with no shifted alternate.
            ("\x1b[47;2;63u", Some('?')),
            ("\x1b[47u", Some('/')),
            ("/", Some('/')),
            // Portuguese Shift+7: alternate and text agree.
            ("\x1b[55:47;2;47u", Some('/')),
            ("\x1b[55:47;2u", Some('/')),
            ("\x1b[55;2u", Some('7')),
            ("\x1b[47;5u", None),
            ("\x1b[47;3u", None),
        ] {
            assert_eq!(keybind_help_text_char(&parse(bytes)), want, "{bytes:?}");
        }
    }

    #[test]
    fn filter_matches_labels_and_shortcuts_case_insensitively() {
        let filtered = filter_keybind_help_groups(groups(), "WoRk", "ctrl+b");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "workspace navigation");

        let filtered = filter_keybind_help_groups(groups(), "x", "ctrl+b");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "close pane");
        assert!(filter_keybind_help_groups(groups(), "panes", "ctrl+b").is_empty());
        // 按显示出来的完整序列匹配：前缀展开成实际前缀键。
        let filtered = filter_keybind_help_groups(groups(), "ctrl+a c", "ctrl+a");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "new tab");
    }

    #[test]
    fn help_lists_every_configured_prefix() {
        let groups = keybind_help_groups(
            &Keybinds::default(),
            &[
                (KeyCode::Char(' '), KeyModifiers::CONTROL),
                (KeyCode::Char('s'), KeyModifiers::CONTROL),
            ],
        );
        assert_eq!(groups[0].0, KeybindHelpGroupId::Global);
        let global = &groups[0].1;
        assert_eq!(
            global[0].0,
            vec![
                HelpKey::Prefix("ctrl+space".into()),
                HelpKey::Prefix("ctrl+s".into())
            ],
            "每个前缀键各一个键帽"
        );
        assert_eq!(global[0].1, crate::i18n::texts().keybinds.prefix_mode);
    }

    #[test]
    fn prefix_bindings_keep_the_rhs_and_expand_for_display() {
        let config: crate::config::Config =
            toml::from_str("[keys]\nnew_tab = [\"prefix+c\", \"ctrl+t\"]\nclose_pane = \"\"\n")
                .expect("config parses");
        let keybinds = config.keybinds();
        let groups = keybind_help_groups(&keybinds, &[]);
        let label = crate::i18n::texts().keybinds.new_tab;
        let (keys, _) = groups
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .find(|(_, text)| text.as_ref() == label)
            .expect("new tab entry");
        assert_eq!(
            keys,
            &vec![
                HelpKey::Prefixed("c".into()),
                HelpKey::Direct("ctrl+t".into())
            ]
        );
        assert_eq!(keys[0].sequence("ctrl+b"), "ctrl+b c");
        assert_eq!(keys[0].after_prefix().as_deref(), Some("c"));
        assert_eq!(keys[1].sequence("ctrl+b"), "ctrl+t");
        assert_eq!(keys[1].after_prefix(), None);
        let close = crate::i18n::texts().keybinds.close_pane;
        assert!(groups
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .any(|(keys, text)| text.as_ref() == close && keys.is_empty()));
    }
}
