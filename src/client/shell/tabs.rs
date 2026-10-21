use super::*;

const TAB_SCROLL_BUTTON_WIDTH: u16 = 3;
const MIN_TAB_STRIP_WIDTH: u16 =
    MIN_TAB_WIDTH + NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2);
/// 标签右侧给关闭按钮留的列（`× `）：每个标签都留，悬浮出现 × 时标签不跳宽。
const TAB_CLOSE_WIDTH: u16 = 2;

// 入参与经典布局 `ShellRenderState` 的字段一一对应，再包一层结构体只是搬运。
#[allow(clippy::too_many_arguments)]
pub(in crate::client::shell) fn render_tab_bar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    tab_scroll: &mut usize,
    reveal_focused_tab: &mut bool,
    tab_drag_insert_index: Option<usize>,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    visual_bell: bool,
    glyphs: StatusGlyphs,
    stale: bool,
    hits: &mut ShellHitMap,
) {
    let tabs = snapshot
        .tabs
        .iter()
        .filter(|tab| Some(tab.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref())
        .collect::<Vec<_>>();
    render_tab_strip(
        buffer,
        area,
        config,
        tab_scroll,
        reveal_focused_tab,
        TabStripContext {
            tabs: &tabs,
            focused: snapshot.focused_tab_id.as_deref(),
            status: Some(snapshot),
            agents: &snapshot.agents,
            glyphs,
            stale,
            bg: config.palette.panel_bg,
            hover: chrome_hover,
            visual_bell,
            insert_index: tab_drag_insert_index,
        },
        hits,
    );
}

pub(in crate::client::shell) struct TabStripContext<'a> {
    pub tabs: &'a [&'a ClientShellTab],
    pub focused: Option<&'a str>,
    /// 右侧状态段（`tab_bar_right`）的来源：经典布局的标签栏画它；工作台把它挪到
    /// 状态栏，这里传 `None`。
    pub status: Option<&'a ClientShellSnapshot>,
    /// 汇总标签状态字形用的 agent（标签里所有 agent 的汇总桶）。
    pub agents: &'a [crate::protocol::ClientShellAgent],
    pub glyphs: StatusGlyphs,
    /// 快照来自不在线的端点（断线 / 重连中）：状态字形画静态、置灰，与侧栏 stale 行
    /// 同口径。
    pub stale: bool,
    /// 标签条的底色（终端组面板头 / 经典标签栏的面板底色）。
    pub bg: ratatui::style::Color,
    pub hover: Option<&'a super::feedback::ChromeHover>,
    pub visual_bell: bool,
    pub insert_index: Option<usize>,
}

/// 标签的汇总状态桶：标签里所有 agent 按汇总优先级（需要输入 > 失败 > 运行中 >
/// 待查看 > 空闲）取最高；没有 agent 是 `None`（不画字形）。
pub(in crate::client::shell) fn tab_bucket(
    agents: &[crate::protocol::ClientShellAgent],
    tab_id: &str,
) -> crate::harness::AgentBucket {
    crate::harness::AgentBucket::rollup(
        agents
            .iter()
            .filter(|agent| agent.tab_id == tab_id)
            .map(client_agent_bucket),
    )
}

/// 标签内容（字形 + 空格 + 名称）的列宽。
fn tab_content_width(label: &str, glyph: Option<&str>) -> u16 {
    glyph
        .map_or(0, |glyph| display_width(glyph).saturating_add(1))
        .saturating_add(display_width(label))
}

pub(in crate::client::shell) fn render_tab_strip(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    tab_scroll: &mut usize,
    reveal_focused_tab: &mut bool,
    context: TabStripContext<'_>,
    hits: &mut ShellHitMap,
) {
    if area.is_empty() {
        return;
    }
    let TabStripContext {
        tabs,
        focused,
        status,
        agents,
        glyphs,
        stale,
        bg,
        hover: chrome_hover,
        visual_bell,
        insert_index: tab_drag_insert_index,
    } = context;
    let palette = &config.palette;
    let glyphs = if stale {
        StatusGlyphs::fixed(config.status_indicators)
    } else {
        glyphs
    };
    let hover_bg = config.components.hover_bg;
    buffer.set_style(area, Style::default().bg(bg));
    let mouse_chrome = config.mouse_capture;
    let close_width = if mouse_chrome { TAB_CLOSE_WIDTH } else { 0 };
    let labels = tabs.iter().map(|tab| tab_label(tab)).collect::<Vec<_>>();
    let buckets = tabs
        .iter()
        .map(|tab| tab_bucket(agents, &tab.tab_id))
        .collect::<Vec<_>>();
    let tab_glyph = |index: usize| {
        let bucket = buckets[index];
        (bucket != crate::harness::AgentBucket::None).then(|| glyphs.bucket(bucket))
    };
    let desired_widths = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            tab_content_width(label, tab_glyph(index))
                .saturating_add(2)
                .saturating_add(close_width)
                .max(MIN_TAB_WIDTH)
        })
        .collect::<Vec<_>>();
    let content = status.map_or(area, |snapshot| tab_bar_content_area(snapshot, area));
    let new_tab_width = if mouse_chrome { NEW_TAB_WIDTH } else { 0 };
    let desired_total = desired_widths
        .iter()
        .copied()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(tabs.len().saturating_sub(1).min(u16::MAX as usize) as u16)
        .saturating_add(new_tab_width);
    let overflow =
        desired_total > content.width && (!mouse_chrome || content.width >= MIN_TAB_STRIP_WIDTH);
    let available = if overflow && mouse_chrome {
        content
            .width
            .saturating_sub(NEW_TAB_WIDTH)
            .saturating_sub(TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2))
    } else {
        content.width.saturating_sub(new_tab_width)
    };
    let max_scroll = max_tab_scroll(&desired_widths, available);
    if !overflow {
        *tab_scroll = 0;
    } else if *reveal_focused_tab {
        if let Some(focused) = tabs
            .iter()
            .position(|tab| focused == Some(tab.tab_id.as_str()))
        {
            *tab_scroll = centered_tab_scroll(focused, &desired_widths, available).min(max_scroll);
        }
    } else {
        *tab_scroll = (*tab_scroll).min(max_scroll);
    }
    *reveal_focused_tab = false;

    let arrow_style = |enabled: bool, hovered: bool| {
        Style::default()
            .fg(if enabled {
                palette.overlay1
            } else {
                palette.overlay0
            })
            .bg(if hovered { hover_bg } else { bg })
    };
    let mut x = content.x;
    let tab_right = if overflow && mouse_chrome {
        hits.tab_scroll_left = Rect::new(
            content.x,
            content.y,
            TAB_SCROLL_BUTTON_WIDTH.min(content.width),
            1,
        );
        put_text(
            buffer,
            hits.tab_scroll_left.x,
            content.y,
            hits.tab_scroll_left.width,
            " < ",
            arrow_style(
                *tab_scroll > 0,
                matches!(
                    chrome_hover,
                    Some(super::feedback::ChromeHover::TabScrollLeft)
                ),
            ),
        );
        x = hits.tab_scroll_left.right();
        content
            .right()
            .saturating_sub(NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH)
    } else {
        content.right().saturating_sub(new_tab_width)
    };

    let mut first_visible = None;
    let mut last_visible = None;
    for (index, tab) in tabs.iter().enumerate().skip(*tab_scroll) {
        let name = &labels[index];
        let desired = desired_widths[index];
        let remaining = tab_right.saturating_sub(x);
        let width = desired.min(remaining);
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        let active = focused == Some(tab.tab_id.as_str());
        let close_hovered = matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::TabClose(id)) if id == &tab.tab_id
        );
        let hovered = close_hovered
            || matches!(
                chrome_hover,
                Some(super::feedback::ChromeHover::Tab(id)) if id == &tab.tab_id
            );
        // 会话式标签：活动标签与页面标签同一套中性样式（浅一档结构面 + 粗体正文，
        // `kit::tabs::active_tab_style`），悬浮换 hover 底，其余 subtext0 平铺在标签条
        // 底色上（颜色只留给状态字形）。
        let style = if active {
            crate::ui::kit::tabs::active_tab_style(palette)
        } else if hovered {
            Style::default().fg(palette.text).bg(hover_bg)
        } else {
            Style::default()
                .fg(super::chrome::readable_on(
                    palette,
                    bg,
                    &[palette.subtext0, palette.text],
                ))
                .bg(bg)
        };
        for column in rect.x..rect.right() {
            if let Some(cell) = buffer.cell_mut((column, rect.y)) {
                cell.set_symbol(" ").set_style(style);
            }
        }
        // 内容（字形 + 名称）在关闭按钮左侧的区域里居中；被截断的最后一个标签先截名称。
        let complete = width >= desired;
        let show_close = complete && close_width > 0 && (active || hovered);
        let inner = Rect::new(
            rect.x.saturating_add(1),
            rect.y,
            rect.width.saturating_sub(2 + close_width),
            1,
        );
        let glyph = tab_glyph(index);
        let glyph_width = glyph.map_or(0, |glyph| display_width(glyph).saturating_add(1));
        let label_room = inner.width.saturating_sub(glyph_width);
        let label = crate::ui::truncate_end(name, usize::from(label_room));
        let used = glyph_width.saturating_add(display_width(&label));
        let mut cursor = inner.x.saturating_add(inner.width.saturating_sub(used) / 2);
        if let Some(glyph) = glyph.filter(|_| inner.width > glyph_width) {
            let glyph_style = if stale {
                Style::default().fg(palette.overlay0)
            } else {
                crate::ui::design::status_style(buckets[index], palette, &config.components)
            }
            .bg(style.bg.unwrap_or(bg));
            put_text(buffer, cursor, rect.y, 1, glyph, glyph_style);
            cursor = cursor.saturating_add(glyph_width);
        }
        put_text(
            buffer,
            cursor,
            rect.y,
            inner.right().saturating_sub(cursor),
            &label,
            style,
        );
        if show_close {
            let close = Rect::new(
                rect.right().saturating_sub(TAB_CLOSE_WIDTH + 1),
                rect.y,
                TAB_CLOSE_WIDTH,
                1,
            );
            let close_style = if close_hovered {
                style.fg(palette.red).add_modifier(Modifier::BOLD)
            } else {
                style.fg(palette.overlay1).remove_modifier(Modifier::BOLD)
            };
            put_text(buffer, close.x, close.y, 1, "×", close_style);
            hits.tab_close.push((close, tab.tab_id.clone()));
        }
        if visual_bell && active && rect.width > 2 {
            put_text(
                buffer,
                rect.right().saturating_sub(2),
                rect.y,
                1,
                "!",
                style.fg(palette.yellow).add_modifier(Modifier::BOLD),
            );
        }
        hits.tabs.push((rect, tab.tab_id.clone()));
        first_visible.get_or_insert(index);
        last_visible = Some(index);
        x = x.saturating_add(width + 1);
        if width < desired {
            break;
        }
    }

    if overflow && mouse_chrome {
        hits.tab_scroll_right = Rect::new(tab_right, area.y, TAB_SCROLL_BUTTON_WIDTH, 1);
        let can_scroll_right = *tab_scroll < max_scroll;
        put_text(
            buffer,
            hits.tab_scroll_right.x,
            area.y,
            hits.tab_scroll_right.width,
            " > ",
            arrow_style(
                can_scroll_right,
                matches!(
                    chrome_hover,
                    Some(super::feedback::ChromeHover::TabScrollRight)
                ),
            ),
        );
        hits.new_tab = Rect::new(
            hits.tab_scroll_right.right(),
            area.y,
            content
                .right()
                .saturating_sub(hits.tab_scroll_right.right())
                .min(NEW_TAB_WIDTH),
            1,
        );
    } else if mouse_chrome {
        hits.new_tab = Rect::new(
            x.min(content.right()),
            area.y,
            content.right().saturating_sub(x).min(NEW_TAB_WIDTH),
            1,
        );
    }
    if mouse_chrome {
        let new_tab_hovered = matches!(chrome_hover, Some(super::feedback::ChromeHover::NewTab));
        put_text(
            buffer,
            hits.new_tab.x,
            area.y,
            hits.new_tab.width,
            " + ",
            Style::default()
                .fg(if new_tab_hovered {
                    palette.text
                } else {
                    palette.overlay1
                })
                .bg(if new_tab_hovered { hover_bg } else { bg }),
        );
    }

    if first_visible.is_some_and(|index| index > 0) {
        let ellipsis_x = if hits.tab_scroll_left.width > 0 {
            hits.tab_scroll_left.right()
        } else {
            content.x
        };
        put_text(
            buffer,
            ellipsis_x,
            area.y,
            u16::from(ellipsis_x < content.right()),
            "…",
            Style::default().fg(palette.overlay0),
        );
    }
    if last_visible.is_some_and(|index| index + 1 < tabs.len()) {
        let ellipsis_x = if hits.tab_scroll_right.width > 0 {
            hits.tab_scroll_right.x.saturating_sub(1)
        } else {
            content.right().saturating_sub(1)
        };
        put_text(
            buffer,
            ellipsis_x,
            area.y,
            u16::from(ellipsis_x >= content.x && ellipsis_x < content.right()),
            "…",
            Style::default().fg(palette.overlay0),
        );
    }

    if let Some(insert_index) = tab_drag_insert_index {
        if let Some(indicator_x) = tab_drop_indicator_x(hits, tabs, insert_index) {
            put_text(
                buffer,
                indicator_x.min(content.right().saturating_sub(1)),
                area.y,
                1,
                "│",
                Style::default().fg(palette.accent),
            );
        }
    }
    if let Some(snapshot) = status {
        render_tab_bar_status(buffer, area, snapshot, palette);
    }
}

pub(crate) fn tab_bar_status_width(snapshot: &ClientShellSnapshot) -> u16 {
    let content = snapshot.tab_bar_right.iter().fold(0u16, |width, segment| {
        width.saturating_add(display_width(&segment.text))
    });
    let separators = snapshot.tab_bar_right.len().saturating_sub(1);
    content.saturating_add(
        display_width(&snapshot.tab_bar_right_separator)
            .saturating_mul(separators.min(u16::MAX as usize) as u16),
    )
}

fn tab_bar_status_area(snapshot: &ClientShellSnapshot, area: Rect) -> Option<Rect> {
    let width = tab_bar_status_width(snapshot);
    if width == 0 {
        return None;
    }
    let reserved = width.saturating_add(1);
    (area.width.saturating_sub(reserved) >= MIN_TAB_STRIP_WIDTH)
        .then(|| Rect::new(area.right().saturating_sub(width), area.y, width, 1))
}

fn tab_bar_content_area(snapshot: &ClientShellSnapshot, area: Rect) -> Rect {
    let reserved = tab_bar_status_area(snapshot, area)
        .map(|status| status.width.saturating_add(1))
        .unwrap_or(0);
    Rect {
        width: area.width.saturating_sub(reserved),
        ..area
    }
}

fn render_tab_bar_status(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
) {
    let Some(status) = tab_bar_status_area(snapshot, area) else {
        return;
    };
    let separator_width = display_width(&snapshot.tab_bar_right_separator);
    let mut x = status.x;
    for (index, segment) in snapshot.tab_bar_right.iter().enumerate() {
        if index > 0 && separator_width > 0 {
            put_text(
                buffer,
                x,
                area.y,
                separator_width,
                &snapshot.tab_bar_right_separator,
                Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            );
            x = x.saturating_add(separator_width);
        }
        let width = display_width(&segment.text);
        let style = if segment.accent {
            Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.overlay1).bg(palette.panel_bg)
        };
        put_text(buffer, x, area.y, width, &segment.text, style);
        x = x.saturating_add(width);
    }
}

fn tab_drop_indicator_x(
    hits: &ShellHitMap,
    tabs: &[&ClientShellTab],
    insert_index: usize,
) -> Option<u16> {
    let visible = hits
        .tabs
        .iter()
        .filter_map(|(rect, tab_id)| {
            tabs.iter()
                .position(|tab| tab.tab_id == *tab_id)
                .map(|index| (index, *rect))
        })
        .collect::<Vec<_>>();
    let (first_index, first_rect) = *visible.first()?;
    let (last_index, last_rect) = *visible.last()?;
    if insert_index == 0 {
        return Some(if first_index == 0 {
            first_rect.x
        } else {
            hits.tab_scroll_left.right()
        });
    }
    if let Some((_, rect)) = visible.iter().find(|(index, _)| *index == insert_index) {
        return Some(rect.x.saturating_sub(1));
    }
    if insert_index >= tabs.len() {
        return Some(if last_index + 1 >= tabs.len() {
            last_rect.right()
        } else {
            hits.tab_scroll_right.x.saturating_sub(1)
        });
    }
    None
}

fn centered_tab_scroll(focused: usize, widths: &[u16], available: u16) -> usize {
    let mut best = focused;
    let mut best_distance = u16::MAX;
    for start in 0..=focused {
        let before = widths
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .take(focused.saturating_sub(start))
            .fold(0u16, |width, (_, tab)| width.saturating_add(tab + 1));
        if before >= available {
            continue;
        }
        let focused_width = widths[focused].min(available.saturating_sub(before));
        let center = before.saturating_mul(2).saturating_add(focused_width);
        let distance = center.abs_diff(available);
        if distance <= best_distance {
            best_distance = distance;
            best = start;
        }
    }
    best
}

fn max_tab_scroll(widths: &[u16], available: u16) -> usize {
    let Some((&last, preceding)) = widths.split_last() else {
        return 0;
    };
    let mut start = preceding.len();
    let mut used = u32::from(last);
    // Keep the longest fully visible suffix, not merely a sliver of the last tab.
    // An oversized last tab must still be reachable at the start of the strip.
    for width in preceding.iter().rev() {
        let required = used + 1 + u32::from(*width);
        if required > u32::from(available) {
            break;
        }
        used = required;
        start -= 1;
    }
    start
}

fn tab_label(tab: &ClientShellTab) -> String {
    if tab.zoomed {
        format!("{} Z", tab.label)
    } else {
        tab.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;
    use crate::harness::AgentBucket;

    fn agent(tab: &str, status: AgentStatus) -> crate::protocol::ClientShellAgent {
        crate::protocol::ClientShellAgent {
            pane_id: format!("{tab}-pane"),
            workspace_id: "ws".into(),
            tab_id: tab.into(),
            name: Some("claude".into()),
            display_agent: Some("claude".into()),
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
            launch_seq: 1,
            activity: Default::default(),
            insight: Default::default(),
        }
    }

    fn tab(id: &str, number: usize) -> ClientShellTab {
        ClientShellTab {
            tab_id: id.into(),
            workspace_id: "ws".into(),
            number,
            label: number.to_string(),
            custom_label: false,
            zoomed: false,
            focused: false,
            agent_status: AgentStatus::Idle,
        }
    }

    #[test]
    fn tab_bucket_rolls_up_the_most_urgent_agent_in_the_tab() {
        let agents = [
            agent("a", AgentStatus::Working),
            agent("a", AgentStatus::Blocked),
            agent("b", AgentStatus::Idle),
            agent("b", AgentStatus::Working),
            agent("c", AgentStatus::Done),
            agent("d", AgentStatus::Unknown),
        ];
        assert_eq!(tab_bucket(&agents, "a"), AgentBucket::NeedsInput);
        assert_eq!(tab_bucket(&agents, "b"), AgentBucket::Working);
        assert_eq!(tab_bucket(&agents, "c"), AgentBucket::Review);
        assert_eq!(tab_bucket(&agents, "d"), AgentBucket::None, "未知不画字形");
        assert_eq!(tab_bucket(&agents, "none"), AgentBucket::None, "没有 agent");
    }

    /// server 下发的失败锁存（`insight.attention` 为 failed）进入状态桶：失败排在
    /// 运行中之前，但让位于需要输入。
    #[test]
    fn tab_bucket_counts_a_failed_agent_from_the_snapshot_attention() {
        let failed = |tab: &str| {
            let mut agent = agent(tab, AgentStatus::Idle);
            agent.insight.attention = Some(crate::protocol::ClientShellAgentAttention {
                kind: crate::api::schema::AgentAttentionKind::Failed,
                ..Default::default()
            });
            agent
        };
        let agents = [
            failed("a"),
            agent("a", AgentStatus::Working),
            failed("b"),
            agent("b", AgentStatus::Blocked),
        ];
        assert_eq!(tab_bucket(&agents, "a"), AgentBucket::Failed);
        assert_eq!(tab_bucket(&agents, "b"), AgentBucket::NeedsInput);
    }

    /// 会话式标签：有 agent 的标签以汇总字形开头（状态色），没有 agent 的不画字形；
    /// 活动标签 surface1 底加粗并带 `×`；端点离线时字形置灰。
    #[test]
    fn session_tabs_lead_with_the_rolled_up_glyph_and_mark_the_active_tab() {
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let palette = &config.palette;
        let owned = [tab("a", 1), tab("b", 2)];
        let tabs = owned.iter().collect::<Vec<_>>();
        let agents = [agent("a", AgentStatus::Blocked)];
        let render = |stale: bool| {
            let area = Rect::new(0, 0, 40, 1);
            let mut buffer = Buffer::empty(area);
            let mut hits = ShellHitMap::default();
            render_tab_strip(
                &mut buffer,
                area,
                &config,
                &mut 0,
                &mut false,
                TabStripContext {
                    tabs: &tabs,
                    focused: Some("a"),
                    status: None,
                    agents: &agents,
                    glyphs: StatusGlyphs::fixed(config.status_indicators),
                    stale,
                    bg: palette.panel_bg,
                    hover: None,
                    visual_bell: false,
                    insert_index: None,
                },
                &mut hits,
            );
            (buffer, hits)
        };
        let cells = |buffer: &Buffer, rect: Rect| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, rect.y)].clone())
                .collect::<Vec<_>>()
        };

        let (buffer, hits) = render(false);
        let [(active, _), (other, _)] = hits.tabs.as_slice() else {
            panic!("两个标签都画出：{:?}", hits.tabs);
        };
        let active_cells = cells(&buffer, *active);
        let glyph = active_cells
            .iter()
            .find(|cell| cell.symbol() == "◆")
            .expect("活动标签以需要输入的字形开头");
        assert_eq!(glyph.fg, config.components.status_input);
        assert_eq!(glyph.bg, palette.surface1, "字形画在活动标签的底色上");
        let label = active_cells
            .iter()
            .find(|cell| cell.symbol() == "1")
            .expect("标签名");
        assert_eq!(label.bg, palette.surface1, "活动标签 surface1 底");
        assert!(label.modifier.contains(Modifier::BOLD), "活动标签加粗");
        assert!(
            active_cells.iter().any(|cell| cell.symbol() == "×"),
            "活动标签带关闭按钮"
        );
        assert_eq!(hits.tab_close.len(), 1, "只有活动标签登记了关闭按钮");

        let other_cells = cells(&buffer, *other);
        assert!(
            other_cells
                .iter()
                .all(|cell| !["◆", "⠋", "◌", "●", "○", "·"].contains(&cell.symbol())),
            "没有 agent 的标签不画字形"
        );
        let label = other_cells
            .iter()
            .find(|cell| cell.symbol() == "2")
            .expect("标签名");
        assert_eq!(label.bg, palette.panel_bg, "其余标签平铺在标签条底色上");
        assert!(!label.modifier.contains(Modifier::BOLD));

        let (buffer, hits) = render(true);
        let glyph = cells(&buffer, hits.tabs[0].0)
            .into_iter()
            .find(|cell| cell.symbol() == "◆")
            .expect("离线时照画字形");
        assert_eq!(glyph.fg, palette.overlay0, "离线端点的字形置灰");
    }

    #[test]
    fn trailing_scroll_limit_accounts_for_full_widths_and_separators() {
        for (widths, available, expected) in [
            (&[][..], 0, 0),
            (&[8, 13][..], 0, 1),
            (&[8, 13][..], 1, 1),
            (&[8, 13][..], 12, 1),
            (&[8, 13][..], 21, 1),
            (&[8, 13][..], 22, 0),
            (&[8, 13][..], 30, 0),
            (&[8, u16::MAX][..], u16::MAX, 1),
        ] {
            assert_eq!(
                max_tab_scroll(widths, available),
                expected,
                "widths={widths:?}, available={available}"
            );
        }
    }
}
