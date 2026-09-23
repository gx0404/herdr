//! agent 行悬浮层（账号用量卡）：作用域视图、正文 + 底部动作行与浮层挂载。

use super::*;
use crate::ui::kit::hover_card::place_hover_card;

/// 悬浮层作用域：账号与 pane 都来自 `hover_scope`，选中账号沿用页面的高亮
/// （仅当它在悬浮层的账号里）。
pub(super) fn hover_scope(state: &State) -> AccountsScope<'_> {
    let account = state.selected_account.as_deref().filter(|selected| {
        state
            .hover_scope
            .accounts
            .iter()
            .any(|account| account.account_id == *selected)
    });
    AccountsScope {
        accounts: &state.hover_scope.accounts,
        refresh_states: &state.hover_scope.refresh_states,
        provider: state.hover_scope.provider.as_deref(),
        account,
        pane: state.hover_scope.pane.as_deref(),
        pane_label: None,
        chrome: BodyChrome::Hover,
        refreshing: state.hover_scope.refreshing(),
        scroll: state.hover_scope.scroll,
    }
}

/// 悬浮层正文底部的流式动作行（刷新 · 切换账号 · 官方查询说明 · 官方回调 · 绑定账号，
/// 最多两行）：绘制与定高共用这一份。页面的动作在工具栏里，不走这里。
fn body_actions(state: &State, scope: &AccountsScope<'_>) -> Vec<ToolbarItem> {
    let mut items = vec![
        refresh_item(scope, state.now_ms),
        cycle_item(state, scope.provider),
        source_item(),
    ];
    items.extend(callback_item(state, scope));
    // 悬浮层的绑定目标就是 hover 的 pane；页面的绑定入口在绑定行里，不在这里重复。
    if scope.chrome == BodyChrome::Hover && scope.pane.is_some() {
        items.push(ToolbarItem {
            label: tr("Bind account", "绑定账号").to_owned(),
            action: Some(Action::Bind),
        });
    }
    items
}

/// 动作行在 `width` 列里折成几行（最多两行）。
fn action_rows(items: &[ToolbarItem], width: u16) -> u16 {
    let widths = items.iter().map(ToolbarItem::width).collect::<Vec<_>>();
    flow_positions(&widths, width, 2, 2).1
}

/// agent 行悬浮层的账号正文：正文 + 底部的动作行（`body_actions`）。
fn accounts_body(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    items: &[ToolbarItem],
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    // 正文至少留 1 行。
    let rows = action_rows(items, area.width).min(area.height.saturating_sub(1));
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(rows));
    accounts_content(buffer, content, state, scope, palette, hits);
    if rows > 0 {
        toolbar_rows(
            buffer,
            Rect::new(area.x, content.bottom(), area.width, rows),
            items,
            palette,
            hits,
        );
    }
}

/// 卡片的外框与底部两行：上下边框 + 间隔 + 「打开页面」。
const CARD_CHROME_ROWS: u16 = 4;

/// 卡片按内容需要的高度（真机 L5：固定 17 行时内容只有四五行，下方空出一大片）：
/// 外框与底部两行 + 正文 + 动作行。正文与 `accounts_content` 同口径——空态是标题
/// （在途时只有「刷新中…」）与说明、上下各留一行；卡片与表格按滚动真源
/// `account_rows` 计行（表格另加表头）。上限由调用方封顶。
fn content_height(
    state: &State,
    scope: &AccountsScope<'_>,
    items: &[ToolbarItem],
    inner_width: u16,
) -> u16 {
    if !state.usage.enabled {
        return CARD_CHROME_ROWS + 1;
    }
    let body = if scope.accounts.is_empty() {
        usize::from(!scope.refreshing) + 3
    } else {
        account_rows(state, scope.accounts, scope.refresh_states)
            + usize::from(state.usage.format == UsageDisplayFormat::Table)
    };
    u16::try_from(body)
        .unwrap_or(u16::MAX)
        .saturating_add(action_rows(items, inner_width))
        .saturating_add(CARD_CHROME_ROWS)
}

/// 卡片最矮高度：上下边框 2 行 + 正文至少 1 行 + 间隔 1 行 + 「打开页面」1 行。
const MIN_CARD_HEIGHT: u16 = 5;

/// 卡片摆在 Agents 面板旁边时，旁侧至少要这么宽（卡片本身更窄时以卡宽为准）；
/// 再窄正文折行过多，不如退回整屏摆放。
const MIN_SIDE_WIDTH: u16 = 40;

/// 卡片的摆放区域（交给 kit 的 `bounds`，kit 算法不变）。锚点是 Agents 面板里的
/// 行（`panel` 非空）时收窄到面板右侧、与面板隔一列；右侧不够 `MIN_SIDE_WIDTH`
/// 再试左侧（面板停靠在右边）；两侧都不够才退回整屏 `area`。卡片宽 68、远宽于
/// 面板，从行的正下 / 正上方展开会盖住相邻 agent 行：上下扫行时指针落进卡片被
/// hold，得先横向移出、再等离开宽限。CLI 标题锚点（`panel` 为空）用整屏。
fn card_bounds(area: Rect, panel: Rect, card_width: u16) -> Rect {
    if panel.is_empty() {
        return area;
    }
    let need = card_width.clamp(1, MIN_SIDE_WIDTH);
    let right_x = panel.right().saturating_add(1).clamp(area.x, area.right());
    let right = Rect::new(right_x, area.y, area.right() - right_x, area.height);
    if right.width >= need {
        return right;
    }
    let left_end = panel.x.saturating_sub(1).clamp(area.x, area.right());
    let left = Rect::new(area.x, area.y, left_end - area.x, area.height);
    if left.width >= need {
        return left;
    }
    area
}

/// 画可见的 agent 行悬浮层（账号用量卡），返回其矩形；没有可见的悬浮层、或
/// 可用空间矮于 `MIN_CARD_HEIGHT` 时返回空矩形。浮层自己的命中区写进 `hover_hits`。
pub(super) fn hover_layer(
    buffer: &mut Buffer,
    state: &State,
    cx: &ChromeContext<'_>,
    hover_hits: &mut Vec<(Rect, Action)>,
) -> Rect {
    let palette = cx.palette;
    let Some(hover) = state.hover.as_ref().filter(|hover| hover.visible) else {
        return Rect::default();
    };
    match &hover.target {
        HoverTarget::Agent { agent, .. } => {
            // 宽 ≤68、高按内容收缩到 ≤17（屏幕小时各让出 2 格）；定位统一走 kit：
            // 锚点（agent 行 / CLI 标题）下方左对齐 → 放不下上翻 → 两侧都不够取
            // 大侧收缩，永不盖住锚点。宿主只收窄摆放区域（agent 行 → Agents 面板
            // 旁边，见 `card_bounds`）。经典布局与停靠工作台的两条绘制 pass 同源。
            let width = buffer.area.width.saturating_sub(2).min(68);
            let bounds = card_bounds(buffer.area, state.hover_panel, width);
            let scope = hover_scope(state);
            let items = if state.usage.enabled {
                body_actions(state, &scope)
            } else {
                Vec::new()
            };
            // kit 会把宽度收进摆放区域：动作行按实际内宽折行。
            let inner_width = width.min(bounds.width).saturating_sub(2);
            let height = content_height(state, &scope, &items, inner_width)
                .max(MIN_CARD_HEIGHT)
                .min(buffer.area.height.saturating_sub(2).min(17));
            let hover_rect = place_hover_card(hover.anchor, (width, height), bounds);
            // kit 在两侧都不够时会收缩高度；矮到放不下一行正文就整张不画，不留
            // 看不见却独占鼠标输入的命中区。
            if hover_rect.height < MIN_CARD_HEIGHT {
                return Rect::default();
            }
            clear(buffer, hover_rect);
            let texts = &crate::i18n::texts().agent_panel;
            let title = format!(
                " {} ",
                crate::i18n::fill(texts.usage_card_title_fmt, &[("agent", agent)])
            );
            let inner = block(buffer, hover_rect, &title, cx);
            if hover.pinned {
                pinned_marker(buffer, hover_rect, &title, texts.usage_pinned, palette);
            }
            let body = Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(2),
            );
            if state.usage.enabled {
                accounts_body(buffer, body, state, &scope, &items, palette, hover_hits);
            } else {
                // 用量在设置里关闭时不会发请求：照实说明，而不是停在「刷新中…」
                // （钉住入口不看这个开关，指针悬浮则根本不会出现）。
                text(
                    buffer,
                    body,
                    0,
                    tr(
                        "Account usage is disabled in settings.",
                        "账号用量已在设置中关闭。",
                    ),
                    Style::default().fg(palette.overlay0),
                );
            }
            // 底行只留「打开页面」：绑定 / 刷新 / 回调等动作都在正文自带的动作行里，
            // 不再出现两个「绑定账号」（ACC-02）。
            let y = inner.bottom().saturating_sub(1);
            secondary_button(
                buffer,
                Rect::new(inner.x, y, inner.width.min(24), 1),
                tr("Open page", "打开页面"),
                Action::Page(Page::Accounts),
                palette,
                hover_hits,
            );
            hover_rect
        }
    }
}

/// 钉住态标记：写在卡片顶边右侧（`─ 已钉住 ─┐`），放不下（会压到标题）就不画。
fn pinned_marker(buffer: &mut Buffer, card: Rect, title: &str, label: &str, palette: &Palette) {
    let title_width = UnicodeWidthStr::width(title) as u16;
    let width = UnicodeWidthStr::width(label) as u16 + 2;
    // 右上角留一格边框与一格横线；标题从左框后一格开始，两者之间至少隔一格。
    let x = card.right().saturating_sub(width + 2);
    if card.height == 0 || x <= card.x.saturating_add(1 + title_width) {
        return;
    }
    let style = Style::default()
        .fg(palette.accent)
        .bg(palette.panel_bg)
        .add_modifier(Modifier::BOLD);
    buffer.set_stringn(x, card.y, " ", 1, style);
    buffer.set_stringn(x + 1, card.y, label, usize::from(width - 2), style);
    buffer.set_stringn(x + width - 1, card.y, " ", 1, style);
}
