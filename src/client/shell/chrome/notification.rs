use super::super::*;
use crate::ui::display_width_u16;
use unicode_segmentation::UnicodeSegmentation;

pub(in crate::client::shell) fn body_lines(text: &str, width: u16) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![String::new()];
    let mut used = 0u16;
    for grapheme in text.graphemes(true) {
        let size = display_width_u16(grapheme);
        if grapheme == "\n" || used.saturating_add(size) > width {
            if lines.len() == 2 {
                if let Some(last) = lines.last_mut() {
                    *last = super::settings::truncate_with_ellipsis(&format!("{last}…"), width);
                }
                break;
            }
            lines.push(String::new());
            used = 0;
            if grapheme == "\n" {
                continue;
            }
        }
        if size <= width {
            if let Some(line) = lines.last_mut() {
                line.push_str(grapheme);
            }
            used += size;
        }
    }
    lines
}

pub(in crate::client::shell) fn bucket(
    event: &SemanticNotification,
    endpoints: &[ClientShellEndpoint],
    endpoint: &ClientEndpointId,
) -> crate::harness::AgentBucket {
    use crate::harness::AgentBucket;
    match event.kind {
        SemanticNotificationKind::NeedsAttention => AgentBucket::NeedsInput,
        SemanticNotificationKind::Finished => AgentBucket::Review,
        SemanticNotificationKind::Custom
            if event.agent.is_some()
                && (endpoints
                    .iter()
                    .find(|e| &e.endpoint_id == endpoint)
                    .and_then(|e| e.snapshot.as_deref())
                    .and_then(|s| {
                        s.agents
                            .iter()
                            .find(|a| Some(a.pane_id.as_str()) == event.pane_id.as_deref())
                    })
                    .is_some_and(|a| client_agent_bucket(a) == AgentBucket::Failed)
                    || event.title.contains(crate::i18n::en::TEXTS.notify.failed)
                    || event
                        .title
                        .contains(crate::i18n::zh_cn::TEXTS.notify.failed)) =>
        {
            AgentBucket::Failed
        }
        _ => AgentBucket::None,
    }
}

pub(in crate::client::shell) fn keys_rect(rect: Rect) -> Rect {
    if rect.width < 20 || rect.height < 4 {
        return Rect::default();
    }
    let label = super::command::text(" keys ", " 按键 ");
    let width = display_width_u16(label);
    Rect::new(rect.right() - width - 2, rect.bottom() - 2, width, 1)
}

pub(in crate::client::shell) struct Card<'a> {
    pub title: &'a str,
    pub location: &'a str,
    pub body: &'a str,
    pub bucket: crate::harness::AgentBucket,
    pub status_indicators: crate::config::StatusIndicatorStyle,
    pub age: &'a str,
    pub jump_key: Option<&'a str>,
    pub focused: bool,
}

pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    area: Rect,
    card: &Card<'_>,
    position: crate::config::ToastHerdrPosition,
    offset: u16,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Rect {
    use ratatui::widgets::{Block, Borders, Clear, Widget};
    if area.is_empty() {
        return Rect::default();
    }
    let width = 56.min(area.width);
    let lines = body_lines(card.body, width.saturating_sub(4));
    let height = (4 + u16::from(!card.location.is_empty()) + lines.len() as u16).min(area.height);
    let x = match position {
        crate::config::ToastHerdrPosition::TopLeft
        | crate::config::ToastHerdrPosition::BottomLeft => area.x,
        _ => area.right() - width,
    };
    let y = match position {
        crate::config::ToastHerdrPosition::TopLeft
        | crate::config::ToastHerdrPosition::TopRight => area.y.saturating_add(offset),
        _ => area.bottom().saturating_sub(height + offset),
    }
    .clamp(area.y, area.bottom() - height);
    let rect = Rect::new(x, y, width, height);
    let p = cx.palette;
    Clear.render(rect, buffer);
    let border =
        if card.focused || cx.hovered(&super::super::feedback::ChromeHover::NotificationToast) {
            p.accent
        } else {
            p.surface1
        };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(crate::ui::BorderGlyphs::ROUNDED.border_set())
        .border_style(Style::default().fg(border))
        .style(
            Style::default()
                .bg(p.panel_bg)
                .remove_modifier(Modifier::all()),
        );
    let inner = block.inner(rect);
    block.render(rect, buffer);
    if inner.is_empty() {
        return rect;
    }
    let content = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let icon = crate::ui::design::status_glyph(card.bucket, card.status_indicators, 0, false);
    super::super::render::put_text(
        buffer,
        content.x,
        content.y,
        1,
        icon,
        crate::ui::design::status_style(card.bucket, p, cx.components).bg(p.panel_bg),
    );
    let age_width = display_width_u16(card.age).min(content.width / 3);
    let title_width = content.width.saturating_sub(age_width + 3);
    super::super::render::put_text(
        buffer,
        content.x + 2,
        content.y,
        title_width,
        &super::settings::truncate_with_ellipsis(card.title, title_width),
        crate::ui::design::title(p).bg(p.panel_bg),
    );
    if age_width > 0 {
        super::super::render::put_text(
            buffer,
            content.right() - age_width,
            content.y,
            age_width,
            card.age,
            crate::ui::design::meta(p).bg(p.panel_bg),
        );
    }
    let footer_y = inner.bottom() - 1;
    let mut y = content.y + 1;
    if !card.location.is_empty() && y < footer_y {
        super::super::render::put_text(
            buffer,
            content.x,
            y,
            content.width,
            &super::settings::truncate_with_ellipsis(card.location, content.width),
            crate::ui::design::meta(p).bg(p.panel_bg),
        );
        y += 1;
    }
    for line in lines {
        if y >= footer_y {
            break;
        }
        super::super::render::put_text(
            buffer,
            content.x,
            y,
            content.width,
            &line,
            crate::ui::design::muted(p).bg(p.panel_bg),
        );
        y += 1;
    }
    if height >= 4 {
        let key_rect = keys_rect(rect);
        let hint = if card.focused {
            super::command::text("enter jump · esc dismiss", "enter 跳转 · esc 忽略").to_owned()
        } else {
            card.jump_key
                .map(|key| format!("{key} {}", super::command::text("jump", "跳转")))
                .unwrap_or_else(|| {
                    super::command::text("keys then esc to dismiss", "按键后 esc 忽略").to_owned()
                })
        };
        let room = key_rect.x.saturating_sub(content.x + 1);
        super::super::render::put_text(
            buffer,
            content.x,
            footer_y,
            room,
            &super::settings::truncate_with_ellipsis(&hint, room),
            crate::ui::design::meta(p).bg(p.panel_bg),
        );
        super::super::render::put_text(
            buffer,
            key_rect.x,
            key_rect.y,
            key_rect.width,
            super::command::text(" keys ", " 按键 "),
            crate::ui::design::key_cap(p),
        );
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn message_wrap_is_bounded_and_cjk_safe() {
        assert_eq!(
            body_lines("等待确认\n继续处理", 8),
            vec!["等待确认", "继续处理"]
        );
        let lines = body_lines("许可请求包含中文以及a-long-command-that-does-not-fit", 12);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| display_width_u16(line) <= 12));
        assert!(lines[1].ends_with('…'));
        assert!(body_lines("", 12).is_empty());
    }

    #[test]
    fn narrow_cards_keep_body_location_and_keyboard_controls_on_separate_rows() {
        for width in [60, 80] {
            let palette = Palette::graphite_light();
            let components = crate::app::state::ComponentStyles::from_palette(&palette);
            let cx = super::super::super::feedback::ChromeContext {
                page_bounds: None,
                palette: &palette,
                components: &components,
                glyphs: crate::ui::BorderGlyphs::ROUNDED,
                hover: None,
                spinner: "◌",
                now: std::time::Instant::now(),
            };
            let area = Rect::new(0, 0, width, 20);
            let mut buffer = Buffer::empty(area);
            for body in [
                "",
                "请确认是否修改 src/client/shell/state.rs，当前终端等待权限输入。",
            ] {
                let rect = paint(
                    &mut buffer,
                    area,
                    &Card {
                        title: "codex 需要你的输入",
                        location: "Remote / herdr / main",
                        body,
                        bucket: crate::harness::AgentBucket::NeedsInput,
                        status_indicators: crate::config::StatusIndicatorStyle::Graphite,
                        age: "2m",
                        jump_key: Some("ctrl+b o"),
                        focused: true,
                    },
                    crate::config::ToastHerdrPosition::BottomRight,
                    0,
                    &cx,
                );
                let keys = keys_rect(rect);
                assert_eq!(rect.intersection(area), rect);
                assert!(keys.y > rect.y + 2);
                assert_eq!(buffer[(rect.x + 2, rect.y + 1)].symbol(), "◆");
                assert_eq!(buffer[(rect.x + 2, rect.y + 1)].fg, components.status_input);
            }
        }
    }
}
