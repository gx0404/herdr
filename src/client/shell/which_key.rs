use std::borrow::Cow;

use super::feedback::ChromeContext;
use super::render::{display_width, panel, put_text};
use super::*;

/// One which-key group: title plus (key cap, description) rows.
pub(super) type WhichKeyGroup = (&'static str, Vec<(String, Cow<'static, str>)>);

/// Which-key rows for prefix mode: (key cap, description) pairs per group,
/// derived from the live keybind help data so user rebinds show up as-is.
/// Navigate-mode entries and unbound ("unset") actions are omitted; prefix
/// entries keep only the right-hand side you actually type next.
pub(super) fn which_key_groups(keybinds: &LiveKeybindConfig) -> Vec<WhichKeyGroup> {
    let prefix_label = crate::config::format_key_combo(keybinds.prefix);
    let navigation_group = crate::i18n::texts().keybinds.group_navigation;
    let unset = crate::i18n::texts().keybinds.unset;
    crate::input::keybind_help_groups(&keybinds.keybinds, keybinds.prefix)
        .into_iter()
        .filter(|(group, _)| *group != navigation_group)
        .filter_map(|(group, entries)| {
            let entries = entries
                .into_iter()
                .filter_map(|(key, label)| {
                    if key == unset {
                        return None;
                    }
                    if key == prefix_label {
                        return Some((key, label));
                    }
                    key.strip_prefix("prefix+")
                        .map(|rhs| (rhs.to_owned(), label))
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then_some((group, entries))
        })
        .collect()
}

struct WhichKeyColumn {
    group: &'static str,
    entries: Vec<(String, Cow<'static, str>)>,
    key_width: u16,
    width: u16,
}

/// Bottom-anchored which-key popup for prefix mode: one column per binding
/// group, keycap + description rows, clipped to what fits.
pub(super) fn render_which_key(
    buffer: &mut Buffer,
    area: Rect,
    keybinds: &LiveKeybindConfig,
    cx: &ChromeContext<'_>,
    occlusion: &mut crate::kitty_graphics::surface::Occlusion,
) {
    let groups = which_key_groups(keybinds);
    if groups.is_empty() || area.width < 12 || area.height < 4 {
        return;
    }
    let p = cx.palette;
    let cap = Style::default()
        .fg(p.accent)
        .bg(p.surface0)
        .add_modifier(Modifier::BOLD);
    let title = Style::default()
        .fg(p.accent)
        .bg(p.panel_bg)
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(p.text).bg(p.panel_bg);

    // One column per group; columns that no longer fit are dropped whole.
    let mut columns: Vec<WhichKeyColumn> = Vec::new();
    let mut used_width = 2u16; // panel borders
    for (group, entries) in groups {
        let key_width = entries
            .iter()
            .map(|(key, _)| display_width(key))
            .max()
            .unwrap_or(0);
        let label_width = entries
            .iter()
            .map(|(_, label)| display_width(label.as_ref()))
            .chain([display_width(group)])
            .max()
            .unwrap_or(0);
        let width = key_width
            .saturating_add(3)
            .saturating_add(label_width)
            .saturating_add(2)
            .max(8);
        if used_width.saturating_add(width) > area.width {
            break;
        }
        used_width = used_width.saturating_add(width);
        columns.push(WhichKeyColumn {
            group,
            entries,
            key_width,
            width,
        });
    }
    if columns.is_empty() {
        return;
    }
    let max_rows = columns
        .iter()
        .map(|column| column.entries.len() as u16 + 1)
        .max()
        .unwrap_or(0);
    let height = max_rows
        .saturating_add(2)
        .min(area.height.saturating_sub(1).max(3));
    let width = used_width.min(area.width);
    let panel_area = Rect::new(
        area.right().saturating_sub(width),
        area.bottom().saturating_sub(height),
        width,
        height,
    );
    let Some(inner) = panel(buffer, panel_area, p.accent, p.panel_bg, cx.glyphs) else {
        return;
    };
    occlusion.cover(panel_area.intersection(buffer.area));
    let mut x = inner.x;
    for column in &columns {
        if x >= inner.right() {
            break;
        }
        let column_area = Rect::new(
            x,
            inner.y,
            column.width.min(inner.right().saturating_sub(x)),
            inner.height,
        );
        put_text(
            buffer,
            column_area.x,
            column_area.y,
            column_area.width,
            column.group,
            title,
        );
        let cap_width = column.key_width.saturating_add(2).min(column_area.width);
        for (offset, (key, label)) in column.entries.iter().enumerate() {
            let y = column_area
                .y
                .saturating_add(1)
                .saturating_add(offset as u16);
            if y >= column_area.bottom() {
                break;
            }
            let cap_text = format!(" {key:<width$} ", width = usize::from(column.key_width));
            put_text(buffer, column_area.x, y, cap_width, &cap_text, cap);
            let label_x = column_area.x.saturating_add(cap_width).saturating_add(1);
            if label_x < column_area.right() {
                put_text(
                    buffer,
                    label_x,
                    y,
                    column_area.right().saturating_sub(label_x),
                    label.as_ref(),
                    text,
                );
            }
        }
        x = x.saturating_add(column.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_keybinds() -> LiveKeybindConfig {
        crate::config::Config::default()
            .live_keybinds_with_diagnostics()
            .map(|(keybinds, _)| keybinds)
            .expect("default keybinds")
    }

    #[test]
    fn which_key_lists_prefix_rhs_from_live_config() {
        let keybinds = live_keybinds();
        let groups = which_key_groups(&keybinds);
        let all: Vec<&str> = groups
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|(key, _)| key.as_str()))
            .collect();
        assert!(all.contains(&"?"), "help rhs listed: {all:?}");
        assert!(all.contains(&"s"), "settings rhs listed: {all:?}");
        assert!(all.contains(&"c"), "new tab rhs listed: {all:?}");
        assert!(all.contains(&"u"), "link hints rhs listed: {all:?}");
        assert!(all.contains(&"1..9"), "indexed range listed: {all:?}");
        assert!(
            all.iter().all(|key| !key.starts_with("prefix+")),
            "no prefix labels remain: {all:?}"
        );
        let navigation = crate::i18n::texts().keybinds.group_navigation;
        assert!(groups.iter().all(|(group, _)| *group != navigation));
    }

    #[test]
    fn which_key_reflects_user_rebinds_and_unset_actions() {
        let config: crate::config::Config = toml::from_str(
            "[keys]\nnew_tab = \"prefix+T\"\nclose_pane = \"\"\nzoom = \"ctrl+z\"\n",
        )
        .expect("config parses");
        let keybinds = config
            .live_keybinds_with_diagnostics()
            .map(|(keybinds, _)| keybinds)
            .expect("live keybinds");
        let groups = which_key_groups(&keybinds);
        let all: Vec<&str> = groups
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|(key, _)| key.as_str()))
            .collect();
        // The rebound action keeps its label and shows the user's key
        // (normalized to shift+t), displacing the default rhs.
        let new_tab_label = crate::i18n::texts().keybinds.new_tab;
        let entry = groups
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .find(|(_, label)| label.as_ref() == new_tab_label)
            .expect("new tab entry");
        assert_ne!(entry.0, "c", "default rhs displaced: {all:?}");
        assert!(!all.contains(&"c"), "no default new-tab rhs: {all:?}");
        assert!(
            !all.contains(&"x"),
            "unset close_pane dropped from which-key: {all:?}"
        );
        assert!(
            !all.contains(&"ctrl+z"),
            "direct-only binding is not a prefix binding: {all:?}"
        );
    }
}
