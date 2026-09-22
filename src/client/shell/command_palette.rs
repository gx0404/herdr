use super::action_table::{
    global_action_state, machine_action_state, ActionCategory, ActionId, ActionTarget, PaletteMode,
    ACTIONS,
};
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
    /// 主菜单分类下标（`GlobalMenuTexts::categories`），由动作表给出。
    pub(super) category: usize,
    pub(super) badge: bool,
    /// 暂时不可用：目录视图里置灰、搜索里不列出，激活是空操作。
    pub(super) enabled: bool,
    /// 二态开关的当前状态（`None` = 不是开关）。
    pub(super) checked: Option<bool>,
    pub(super) action: ClientPaletteAction,
}

#[derive(Debug, Clone)]
pub(super) enum ClientPaletteAction {
    /// 执行动作表里的一个动作。
    Run(ActionId, ActionTarget),
    Category(usize),
    Search,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserView {
    Menu(Option<usize>),
    Search,
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
                if let Some(item) = palette.items.iter().find(|item| {
                    &item.id == id
                        && !navigation(item)
                        && (item.enabled || palette.view != BrowserView::Search)
                }) {
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
                BrowserView::Search => !navigation(item) && item.enabled,
                BrowserView::Menu(None) => matches!(
                    item.action,
                    ClientPaletteAction::Category(_) | ClientPaletteAction::Search
                ),
                BrowserView::Menu(Some(group)) => {
                    (!navigation(item) && item.category == group)
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
            if navigation(item) || !item.enabled {
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
                crate::i18n::en::TEXTS.global_menu.categories[item.category],
                crate::i18n::zh_cn::TEXTS.global_menu.categories[item.category]
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

impl ClientShellState {
    /// 命令面板的条目全集：动作表按声明顺序展开（聚焦对象语义的一条、按机器
    /// 铺开的每台机器一组、自定义命令每条一项），再追加分类与导航项。暂时不可
    /// 用的动作照样列出（目录视图置灰），对当前布局不适用的不列。
    fn build_palette_items(&self) -> Vec<ClientPaletteItem> {
        let texts = crate::i18n::texts();
        let keybinds = &self.config.keybinds.keybinds;
        let cx = self.global_action_context();
        let mut items = Vec::new();
        let mut machines_listed = false;
        for spec in ACTIONS {
            match spec.palette {
                PaletteMode::Hidden => {}
                PaletteMode::Focused => {
                    let state = global_action_state(spec.id, &cx);
                    if !state.visible {
                        continue;
                    }
                    items.push(ClientPaletteItem {
                        id: spec.key.to_owned(),
                        title: spec.title_text(texts, state.alternate).to_owned(),
                        subtitle: spec
                            .binding
                            .and_then(|binding| (binding.keys)(keybinds).label())
                            .unwrap_or_default(),
                        category: spec.category.index(),
                        badge: state.badge,
                        enabled: state.enabled,
                        checked: state.checked,
                        action: ClientPaletteAction::Run(spec.id, ActionTarget::Focused),
                    });
                }
                PaletteMode::PerCommand => {
                    for command in &keybinds.custom_commands {
                        items.push(ClientPaletteItem {
                            id: format!("{}:{}", spec.key, command.command),
                            title: command
                                .description
                                .clone()
                                .unwrap_or_else(|| command.command.clone()),
                            subtitle: command.label.clone(),
                            category: spec.category.index(),
                            badge: false,
                            enabled: true,
                            checked: None,
                            action: ClientPaletteAction::Run(
                                spec.id,
                                ActionTarget::Command(command.clone()),
                            ),
                        });
                    }
                }
                // 机器动作组整组按机器展开一次（机器之间不交错），位置取组里
                // 第一条的声明位置。
                PaletteMode::PerMachine if !machines_listed => {
                    machines_listed = true;
                    self.push_machine_palette_items(&mut items);
                }
                PaletteMode::PerMachine => {}
            }
        }
        for (index, title) in texts.global_menu.categories.iter().enumerate() {
            let count = items.iter().filter(|item| item.category == index).count();
            let badge = items
                .iter()
                .any(|item| item.category == index && item.badge);
            items.push(ClientPaletteItem {
                id: format!("category:{index}"),
                title: title.to_string(),
                subtitle: format!("{count}  ›"),
                category: index,
                badge,
                enabled: true,
                checked: None,
                action: ClientPaletteAction::Category(index),
            });
        }
        let global_menu = &texts.global_menu;
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
                    keybinds.command_search.label().unwrap_or_default()
                } else {
                    String::new()
                },
                category: ActionCategory::Help.index(),
                badge: false,
                enabled: true,
                checked: None,
                action,
            });
        }
        items
    }

    /// 机器动作组按已保存机器展开：与机器行右键菜单同一组条目、同一套可用性。
    fn push_machine_palette_items(&self, items: &mut Vec<ClientPaletteItem>) {
        let texts = crate::i18n::texts();
        for profile in &self.saved_profiles {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let online = self.endpoint_is_online(&endpoint_id);
            let active = self.active_endpoint_id == endpoint_id;
            for spec in ACTIONS
                .iter()
                .filter(|spec| spec.palette == PaletteMode::PerMachine)
            {
                let state = machine_action_state(spec.id, profile.enabled, online, active);
                items.push(ClientPaletteItem {
                    id: format!("{}:{}", spec.key, profile.id.as_str()),
                    title: crate::i18n::fill(
                        spec.title_text(texts, state.alternate),
                        &[("label", &profile.label)],
                    ),
                    subtitle: profile.target.clone(),
                    category: spec.category.index(),
                    badge: state.badge,
                    enabled: state.enabled,
                    checked: state.checked,
                    action: ClientPaletteAction::Run(
                        spec.id,
                        ActionTarget::Machine(endpoint_id.clone()),
                    ),
                });
            }
        }
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
        // 两种语言的标题互为别名：界面是中文时照样能用英文词搜到，反之亦然。
        let mut aliases = HashMap::<String, String>::new();
        for texts in [&crate::i18n::en::TEXTS, &crate::i18n::zh_cn::TEXTS] {
            for spec in ACTIONS
                .iter()
                .filter(|spec| spec.palette == PaletteMode::Focused)
            {
                let entry = aliases.entry(spec.key.to_owned()).or_default();
                for alternate in [false, true] {
                    entry.push(' ');
                    entry.push_str(spec.title_text(texts, alternate));
                }
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
            if !row.item.enabled {
                // 不可用的条目：面板保持打开，什么都不做。
                return;
            }
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
        if let ClientPaletteAction::Run(id, target) = action {
            self.run_action(id, target, outcome);
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
        // 暂时不可用的条目只在目录视图里出现：置灰，选中也不反色成可执行的样子。
        let style = if row.item.enabled {
            style
        } else {
            style.fg(p.overlay0)
        };
        b.set_style(rect, style);
        let marker_width = 2.min(rect.width);
        // 标记列：第 1 格是键盘选中的「›」，第 2 格是开关的勾选态。
        let marker = match (chosen, row.item.checked == Some(true)) {
            (true, true) => "›✓",
            (true, false) => "› ",
            (false, true) => " ✓",
            (false, false) => "  ",
        };
        put_text(b, rect.x, rect.y, marker_width, marker, style);
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
