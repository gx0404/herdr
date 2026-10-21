use std::borrow::Cow;

use super::feedback::ChromeContext;
use super::render::{display_width, panel, put_text};
use super::*;
use crate::ui::kit::key_cap::{cap_width, paint_cap};

/// which-key 的一行：前缀模式下接着按的键（每个备选一个键帽）与说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WhichKeyEntry {
    pub(super) keys: Vec<String>,
    pub(super) label: Cow<'static, str>,
}

/// which-key 的一组：标题取自稳定的分组身份（`input::keybind_help` 的分组 id），
/// 不再拿本地化组名当身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WhichKeyGroup {
    pub(super) title: &'static str,
    pub(super) entries: Vec<WhichKeyEntry>,
}

/// 备选键帽之间的分隔符（元信息色）。
const KEY_SEPARATOR: &str = "/";

/// 截断标记（元信息色）。
const OVERFLOW: &str = "…";

/// Which-key rows for prefix mode, derived from the live keybind help data so
/// user rebinds show up as-is. 每个绑定单独判断：前缀绑定只留右手键，直接绑定不列；
/// 一个动作配了多个前缀绑定就列多个键帽。导航模式的组按分组 id 排除，未设置的动作
/// 不列；前缀键本身（不论配了几个）列成「发送前缀」。
pub(super) fn which_key_groups(keybinds: &LiveKeybindConfig) -> Vec<WhichKeyGroup> {
    crate::input::keybind_help_groups(&keybinds.keybinds, &keybinds.prefix)
        .into_iter()
        .filter(|(id, _)| id.in_which_key())
        .filter_map(|(id, entries)| {
            let entries = entries
                .into_iter()
                .filter_map(|(keys, label)| {
                    let prefix_row = !keys.is_empty() && keys.iter().all(|key| key.is_prefix_key());
                    let mut shown: Vec<String> = Vec::with_capacity(keys.len());
                    for key in keys.iter().filter_map(|key| key.after_prefix()) {
                        if !shown
                            .iter()
                            .any(|existing| existing.as_str() == key.as_ref())
                        {
                            shown.push(key.into_owned());
                        }
                    }
                    if shown.is_empty() {
                        return None;
                    }
                    let label = if prefix_row {
                        Cow::Borrowed(crate::i18n::texts().mode_bar.prefix_send)
                    } else {
                        label
                    };
                    Some(WhichKeyEntry { keys: shown, label })
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then_some(WhichKeyGroup {
                title: id.title(),
                entries,
            })
        })
        .collect()
}

/// 一行键帽占的列数：每个键帽两侧各一格，备选之间一个 `/`。
fn keys_width(keys: &[String]) -> u16 {
    keys.iter()
        .map(|key| cap_width(key))
        .sum::<u16>()
        .saturating_add(u16::try_from(keys.len().saturating_sub(1)).unwrap_or(u16::MAX))
}

/// 画一行键帽，不越过 `right`；返回画到的列。
fn paint_keys(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    keys: &[String],
    cap: Style,
    separator: Style,
) -> u16 {
    let mut x = x;
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            if x >= right {
                break;
            }
            put_text(buffer, x, y, 1, KEY_SEPARATOR, separator);
            x = x.saturating_add(1);
        }
        x = paint_cap(buffer, x, y, right, key, cap);
    }
    x
}

/// 帮助键在前缀模式里接着要按的写法：前缀绑定写右手键，直接绑定写完整按键。
fn help_key_after_prefix(keybinds: &LiveKeybindConfig) -> Option<String> {
    let binding = keybinds.keybinds.help.bindings.first()?;
    let key = if binding.trigger.is_prefix() {
        binding
            .label
            .strip_prefix("prefix+")
            .unwrap_or(&binding.label)
    } else {
        binding.label.as_str()
    };
    Some(crate::ui::key_labels::display_key(key).into_owned())
}

/// which-key 浮层排布：放得下的组、每组可见的行数，以及是否有组或行被截掉。
/// 渲染与测试共用这一份量法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WhichKeyLayout {
    /// 每个可见组：组下标、按键列宽、整列宽。
    columns: Vec<(usize, u16, u16)>,
    /// 每列最多画几行条目。
    rows: u16,
    pub(super) hidden_groups: usize,
    pub(super) clipped_rows: bool,
    pub(super) panel: Rect,
}

impl WhichKeyLayout {
    pub(super) fn overflow(&self) -> bool {
        self.hidden_groups > 0 || self.clipped_rows
    }

    #[cfg(test)]
    pub(super) fn shown_groups(&self) -> usize {
        self.columns.len()
    }
}

pub(super) fn which_key_layout(groups: &[WhichKeyGroup], area: Rect) -> Option<WhichKeyLayout> {
    if groups.is_empty() || area.width < 12 || area.height < 4 {
        return None;
    }
    let mut columns = Vec::new();
    let mut used_width = 2u16; // panel borders
    for (index, group) in groups.iter().enumerate() {
        let key_width = group
            .entries
            .iter()
            .map(|entry| keys_width(&entry.keys))
            .max()
            .unwrap_or(0);
        let label_width = group
            .entries
            .iter()
            .map(|entry| display_width(entry.label.as_ref()))
            .max()
            .unwrap_or(0);
        let width = key_width
            .saturating_add(1)
            .saturating_add(label_width)
            .saturating_add(2)
            .max(display_width(group.title).saturating_add(2))
            .max(8);
        if used_width.saturating_add(width) > area.width {
            if columns.is_empty() {
                // 连第一组都放不下：按可用宽度裁这一列（说明截断），不整屏消失。
                columns.push((index, key_width, area.width.saturating_sub(2)));
                used_width = area.width;
            }
            break;
        }
        used_width = used_width.saturating_add(width);
        columns.push((index, key_width, width));
    }
    let hidden_groups = groups.len() - columns.len();
    let wanted_rows = columns
        .iter()
        .map(|(index, _, _)| u16::try_from(groups[*index].entries.len()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    // 标题一行 + 条目；浮层上下边框两行，离底部状态行留一行。
    let max_inner = area.height.saturating_sub(1).max(3).saturating_sub(2);
    let clipped_rows = wanted_rows.saturating_add(1) > max_inner;
    let overflow = hidden_groups > 0 || clipped_rows;
    let reserve = u16::from(overflow);
    let rows = wanted_rows.min(max_inner.saturating_sub(1 + reserve));
    let clipped_rows = clipped_rows || rows < wanted_rows;
    let inner_height = (rows + 1 + reserve).min(max_inner).max(1);
    let height = inner_height + 2;
    let width = used_width.min(area.width);
    let panel = Rect::new(
        area.right().saturating_sub(width),
        area.bottom().saturating_sub(height),
        width,
        height,
    );
    Some(WhichKeyLayout {
        columns,
        rows,
        hidden_groups,
        clipped_rows,
        panel,
    })
}

/// Bottom-anchored which-key popup for prefix mode: one column per binding
/// group, keycap + description rows. 放不下的组或行不会静默丢掉：最后一行写
/// 「… 还有 N 组」并给出打开完整快捷键帮助的键。
pub(super) fn render_which_key(
    buffer: &mut Buffer,
    area: Rect,
    keybinds: &LiveKeybindConfig,
    cx: &ChromeContext<'_>,
    occlusion: &mut crate::kitty_graphics::surface::Occlusion,
) {
    let groups = which_key_groups(keybinds);
    let Some(layout) = which_key_layout(&groups, area) else {
        return;
    };
    let p = cx.palette;
    let cap = crate::ui::design::key_cap(p);
    let section = crate::ui::design::section(p).bg(p.panel_bg);
    let text = crate::ui::design::body(p).bg(p.panel_bg);
    let meta = crate::ui::design::meta(p).bg(p.panel_bg);
    let label_style = crate::ui::design::key_label(p).bg(p.panel_bg);

    let Some(inner) = panel(
        buffer,
        layout.panel,
        crate::ui::design::frame(p),
        p.panel_bg,
        crate::ui::BorderGlyphs::ROUNDED,
    ) else {
        return;
    };
    occlusion.cover(layout.panel.intersection(buffer.area));
    let mut x = inner.x;
    for (index, key_width, width) in &layout.columns {
        if x >= inner.right() {
            break;
        }
        let group = &groups[*index];
        let column = Rect::new(
            x,
            inner.y,
            (*width).min(inner.right().saturating_sub(x)),
            inner.height,
        );
        put_text(
            buffer,
            column.x,
            column.y,
            column.width,
            group.title,
            section,
        );
        // 说明列与键帽列之间空一格；列右侧留两格与下一组分开。
        let right = column.right().saturating_sub(2).max(column.x + 1);
        for (offset, entry) in group
            .entries
            .iter()
            .take(usize::from(layout.rows))
            .enumerate()
        {
            let y = column.y.saturating_add(1).saturating_add(offset as u16);
            if y >= inner.bottom() {
                break;
            }
            paint_keys(buffer, column.x, y, right, &entry.keys, cap, meta);
            let label_x = column.x.saturating_add(*key_width).saturating_add(1);
            if label_x < right {
                put_text(
                    buffer,
                    label_x,
                    y,
                    right - label_x,
                    entry.label.as_ref(),
                    text,
                );
            }
        }
        x = x.saturating_add(*width);
    }
    if layout.overflow() && inner.height >= 2 {
        let y = inner.bottom() - 1;
        let right = inner.right();
        put_text(buffer, inner.x, y, 1, OVERFLOW, meta);
        let mut x = inner.x.saturating_add(2);
        let more = if layout.hidden_groups > 0 {
            super::observability::tr("{n} more groups", "还有 {n} 组")
                .replace("{n}", &layout.hidden_groups.to_string())
        } else {
            super::observability::tr("more keys", "还有更多按键").to_owned()
        };
        put_text(buffer, x, y, right.saturating_sub(x), &more, meta);
        x = x.saturating_add(display_width(&more)).saturating_add(2);
        if let Some(help) = help_key_after_prefix(keybinds) {
            if x < right {
                crate::ui::kit::key_cap::paint_hint(
                    buffer,
                    x,
                    y,
                    right,
                    &help,
                    super::observability::tr("all keys", "全部快捷键"),
                    cap,
                    label_style,
                );
            }
        }
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

    fn keys(groups: &[WhichKeyGroup]) -> Vec<&str> {
        groups
            .iter()
            .flat_map(|group| group.entries.iter())
            .flat_map(|entry| entry.keys.iter().map(String::as_str))
            .collect()
    }

    #[test]
    fn which_key_lists_prefix_rhs_from_live_config() {
        let keybinds = live_keybinds();
        let groups = which_key_groups(&keybinds);
        let all = keys(&groups);
        assert!(all.contains(&"?"), "help rhs listed: {all:?}");
        assert!(all.contains(&"s"), "settings rhs listed: {all:?}");
        assert!(all.contains(&"c"), "new tab rhs listed: {all:?}");
        assert!(all.contains(&"u"), "link hints rhs listed: {all:?}");
        assert!(all.contains(&"1..9"), "indexed range listed: {all:?}");
        assert!(
            all.iter().all(|key| !key.contains("prefix+")),
            "no prefix labels remain: {all:?}"
        );
        let navigation = crate::i18n::texts().keybinds.group_navigation;
        assert!(groups.iter().all(|group| group.title != navigation));
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
        let all = keys(&groups);
        let new_tab_label = crate::i18n::texts().keybinds.new_tab;
        let entry = groups
            .iter()
            .flat_map(|group| group.entries.iter())
            .find(|entry| entry.label.as_ref() == new_tab_label)
            .expect("new tab entry");
        assert_ne!(entry.keys, vec!["c".to_owned()], "default rhs displaced");
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

    /// 一个动作配多个绑定时逐个判断：两个前缀绑定各一个键帽，直接绑定不混进来
    /// （以前对拼接后的整串只 strip 一次，会得到「c / prefix+y」）。
    #[test]
    fn which_key_splits_multiple_bindings_per_action() {
        let config: crate::config::Config =
            toml::from_str("[keys]\nnew_tab = [\"ctrl+t\", \"prefix+c\", \"prefix+y\"]\n")
                .expect("config parses");
        let keybinds = config
            .live_keybinds_with_diagnostics()
            .map(|(keybinds, _)| keybinds)
            .expect("live keybinds");
        let groups = which_key_groups(&keybinds);
        let new_tab_label = crate::i18n::texts().keybinds.new_tab;
        let entry = groups
            .iter()
            .flat_map(|group| group.entries.iter())
            .find(|entry| entry.label.as_ref() == new_tab_label)
            .expect("new tab entry");
        assert_eq!(entry.keys, vec!["c".to_owned(), "y".to_owned()]);
    }

    /// 配了多个前缀键时，前缀行照样列出（每个前缀一个键帽，说明是「发送前缀」）；
    /// 以前拿拼接串和主前缀比较，多前缀时这一行会被丢掉。
    #[test]
    fn which_key_keeps_the_prefix_row_with_several_prefixes() {
        let config: crate::config::Config =
            toml::from_str("[keys]\nprefix = [\"ctrl+a\", \"ctrl+s\"]\n").expect("config");
        let keybinds = config
            .live_keybinds_with_diagnostics()
            .map(|(keybinds, _)| keybinds)
            .expect("live keybinds");
        assert_eq!(keybinds.prefix.len(), 2, "两个前缀都生效");
        let groups = which_key_groups(&keybinds);
        let send = crate::i18n::texts().mode_bar.prefix_send;
        let entry = groups
            .iter()
            .flat_map(|group| group.entries.iter())
            .find(|entry| entry.label.as_ref() == send)
            .expect("prefix row");
        assert_eq!(entry.keys, vec!["ctrl+a".to_owned(), "ctrl+s".to_owned()]);
        assert_eq!(
            groups[0].title,
            crate::i18n::texts().keybinds.group_global,
            "组标题取自稳定 id"
        );
    }

    fn render(width: u16, height: u16) -> Vec<String> {
        let keybinds = live_keybinds();
        let palette = Palette::graphite_dark();
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let cx = ChromeContext {
            page_bounds: None,
            palette: &palette,
            components: &components,
            glyphs: crate::ui::BorderGlyphs::ROUNDED,
            hover: None,
            spinner: "◐",
            now: std::time::Instant::now(),
        };
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        render_which_key(&mut buffer, area, &keybinds, &cx, &mut occlusion);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    /// 窄屏放不下所有组时最后一行写明还有几组、按哪个键看全部，不静默丢组。
    #[test]
    fn narrow_which_key_marks_hidden_groups() {
        let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let keybinds = live_keybinds();
        let groups = which_key_groups(&keybinds);
        let layout = which_key_layout(&groups, Rect::new(0, 0, 60, 20)).expect("layout");
        assert!(layout.hidden_groups > 0, "60 列放不下所有组");
        assert_eq!(layout.shown_groups() + layout.hidden_groups, groups.len());
        let rows = render(60, 20);
        let hint = rows
            .iter()
            .find(|row| row.contains('…'))
            .unwrap_or_else(|| panic!("缺少截断提示：{rows:#?}"));
        assert!(
            hint.contains(&format!("{} more groups", layout.hidden_groups)),
            "{hint:?}"
        );
        assert!(hint.contains(" ? "), "给出帮助键：{hint:?}");
        assert!(hint.contains("all keys"), "{hint:?}");

        // 宽屏全部放下：没有截断提示。
        let layout = which_key_layout(&groups, Rect::new(0, 0, 200, 60)).expect("layout");
        assert!(!layout.overflow(), "{layout:?}");
        assert!(!render(200, 60).iter().any(|row| row.contains('…')));
    }

    #[test]
    fn which_key_caps_and_titles_use_the_design_tokens() {
        let keybinds = live_keybinds();
        let palette = Palette::graphite_dark();
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let cx = ChromeContext {
            page_bounds: None,
            palette: &palette,
            components: &components,
            glyphs: crate::ui::BorderGlyphs::ROUNDED,
            hover: None,
            spinner: "◐",
            now: std::time::Instant::now(),
        };
        let area = Rect::new(0, 0, 200, 60);
        let mut buffer = Buffer::empty(area);
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        render_which_key(&mut buffer, area, &keybinds, &cx, &mut occlusion);
        let groups = which_key_groups(&keybinds);
        let layout = which_key_layout(&groups, area).expect("layout");
        let inner = Rect::new(
            layout.panel.x + 1,
            layout.panel.y + 1,
            layout.panel.width - 2,
            layout.panel.height - 2,
        );
        let title = buffer[(inner.x, inner.y)].style();
        assert_eq!(
            title.fg,
            crate::ui::design::section(&palette).fg,
            "组标题用分区标题色"
        );
        assert!(title.add_modifier.contains(Modifier::BOLD));
        let cap = buffer[(inner.x, inner.y + 1)].style();
        assert_eq!(cap.bg, crate::ui::design::key_cap(&palette).bg, "键帽底色");
    }
}
