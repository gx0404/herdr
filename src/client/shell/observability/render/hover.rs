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

/// agent 行悬浮层的账号正文：正文 + 底部流式动作行（刷新 · 切换账号 · 官方查询说明 ·
/// 官方回调 · 绑定账号，最多两行）。页面的动作在工具栏里，不走这里。
fn accounts_body(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
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
    let widths = items.iter().map(ToolbarItem::width).collect::<Vec<_>>();
    let (_, rows) = flow_positions(&widths, area.width, 2, 2);
    // 正文至少留 1 行。
    let rows = rows.min(area.height.saturating_sub(1));
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(rows));
    accounts_content(buffer, content, state, scope, palette, hits);
    if rows > 0 {
        toolbar_rows(
            buffer,
            Rect::new(area.x, content.bottom(), area.width, rows),
            &items,
            palette,
            hits,
        );
    }
}

/// 画可见的 agent 行悬浮层（账号用量卡），返回其矩形；没有可见的悬浮层时
/// 返回空矩形。浮层自己的命中区写进 `hover_hits`。
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
            // 尺寸不变（宽 ≤68、高 ≤17，屏幕小时各让出 2 格）；定位统一走 kit：
            // 锚点（agent 行 / CLI 标题）下方左对齐 → 放不下上翻 → 两侧都不够取
            // 大侧收缩，永不盖住锚点。经典布局与停靠工作台的两条绘制 pass 同源。
            let size = (
                buffer.area.width.saturating_sub(2).min(68),
                buffer.area.height.saturating_sub(2).min(17),
            );
            let hover_rect = place_hover_card(hover.anchor, size, buffer.area);
            if hover_rect.is_empty() {
                return Rect::default();
            }
            clear(buffer, hover_rect);
            let inner = block(
                buffer,
                hover_rect,
                &format!(" {} · {} ", agent, tr("Account usage", "账号用量")),
                cx,
            );
            accounts_body(
                buffer,
                Rect::new(
                    inner.x,
                    inner.y,
                    inner.width,
                    inner.height.saturating_sub(2),
                ),
                state,
                &hover_scope(state),
                palette,
                hover_hits,
            );
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
