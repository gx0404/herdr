use super::*;
use ratatui::widgets::{Clear, Widget};

pub(super) fn render_mobile_banner(
    buffer: &mut Buffer,
    area: Rect,
    notice: &ClientVisibleEndpointNotice,
    offset_for_warning: bool,
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
) -> Rect {
    let level = super::feedback::ClientToastLevel::from_notice_kind(notice.key.kind);
    super::notifications::render_mobile_notice_banner(
        buffer,
        area,
        &notice.title,
        Some(&notice.body),
        level.icon(),
        level.color(components),
        offset_for_warning,
        palette,
    )
}

/// Rendered geometry of the lifecycle banner: the banner itself plus the
/// clickable retry/give-up affordances (empty when not applicable).
#[derive(Clone, Copy, Default)]
pub(super) struct LifecycleBanner {
    pub rect: Rect,
    pub retry: Rect,
    pub give_up: Rect,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_lifecycle_banner(
    buffer: &mut Buffer,
    area: Rect,
    label: &str,
    status: ClientEndpointStatus,
    progress: Option<(u32, u64)>,
    show_actions: bool,
    top_offset: u16,
    cx: &super::feedback::ChromeContext<'_>,
) -> LifecycleBanner {
    if area.is_empty() || status == ClientEndpointStatus::Online {
        return LifecycleBanner::default();
    }
    let palette = cx.palette;
    let (symbol, state, color) = endpoint_status_presentation(status, palette, cx.spinner);
    let text = match (status, progress) {
        (ClientEndpointStatus::Reconnecting, Some((attempts, seconds))) => format!(
            "{symbol} {}",
            crate::i18n::fill(
                crate::i18n::texts().machine_auth.banner_reconnecting_fmt,
                &[
                    ("label", label),
                    ("attempt", attempts.to_string().as_str()),
                    ("seconds", seconds.to_string().as_str()),
                ],
            )
        ),
        _ => format!("{symbol} {label} · {state}"),
    };
    let t = &crate::i18n::texts().machine_auth;
    let (retry_label, give_up_label) = if show_actions
        && matches!(
            status,
            ClientEndpointStatus::Reconnecting | ClientEndpointStatus::Attention
        ) {
        (
            Some(t.banner_retry_button),
            (status == ClientEndpointStatus::Reconnecting).then_some(t.banner_give_up_button),
        )
    } else {
        (None, None)
    };
    let buttons_width = retry_label
        .map(|label| unicode_width::UnicodeWidthStr::width(label) + 4)
        .unwrap_or(0)
        + give_up_label
            .map(|label| unicode_width::UnicodeWidthStr::width(label) + 4)
            .unwrap_or(0);
    let width =
        u16::try_from(unicode_width::UnicodeWidthStr::width(text.as_str()) + 2 + buttons_width)
            .unwrap_or(u16::MAX)
            .min(area.width);
    let y = area
        .y
        .saturating_add(top_offset)
        .min(area.bottom().saturating_sub(1));
    let rect = Rect::new(area.right().saturating_sub(width), y, width, 1);
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(palette.surface0));
    super::render::put_text(
        buffer,
        rect.x.saturating_add(1),
        rect.y,
        rect.width.saturating_sub(2),
        &text,
        Style::default().fg(color).bg(palette.surface0),
    );
    let mut banner = LifecycleBanner {
        rect,
        ..LifecycleBanner::default()
    };
    let mut x = rect.right();
    for (label, hover, is_retry) in [
        (
            give_up_label,
            super::feedback::ChromeHover::LifecycleBannerGiveUp,
            false,
        ),
        (
            retry_label,
            super::feedback::ChromeHover::LifecycleBannerRetry,
            true,
        ),
    ]
    .into_iter()
    .filter_map(|(label, hover, is_retry)| label.map(|label| (label, hover, is_retry)))
    {
        let button_width =
            u16::try_from(unicode_width::UnicodeWidthStr::width(label) + 4).unwrap_or(u16::MAX);
        x = x.saturating_sub(button_width);
        if x < rect.x {
            break;
        }
        let button_rect = Rect::new(x, rect.y, button_width.min(rect.right() - x), 1);
        super::render::modal_button(
            buffer,
            button_rect,
            label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(&hover, crate::ui::ModalButtonState::Normal),
            palette,
        );
        if is_retry {
            banner.retry = button_rect;
        } else {
            banner.give_up = button_rect;
        }
    }
    banner
}

pub(super) fn render_notice(
    buffer: &mut Buffer,
    area: Rect,
    notice: &ClientVisibleEndpointNotice,
    top_offset: u16,
    cx: &super::feedback::ChromeContext<'_>,
) -> Rect {
    super::notifications::render_notification_card(
        buffer,
        area,
        &notice.title,
        &notice.body,
        crate::config::ToastHerdrPosition::TopRight,
        top_offset,
        super::feedback::ClientToastLevel::from_notice_kind(notice.key.kind),
        cx,
    )
}
