use ratatui::layout::Rect;

/// Brand title and key labels stay language-neutral; translatable onboarding
/// copy lives in `crate::i18n` (`texts().onboarding`).
pub(crate) const ONBOARDING_TITLE: &str = "  herdr";
pub(crate) const ONBOARDING_PREFIX_LABEL: &str = "ctrl+b";
pub(crate) const ONBOARDING_HELP_LABEL: &str = "?";

pub(crate) fn onboarding_welcome_continue_rect(area: Rect) -> Rect {
    super::widgets::continue_button_rect(area)
}
