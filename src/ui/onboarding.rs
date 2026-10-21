use ratatui::layout::Rect;

/// Brand title stays language-neutral; translatable onboarding copy lives in
/// `crate::i18n` (`texts().onboarding`). 键位提示取实际配置的前缀与帮助键，不再写死。
pub(crate) const ONBOARDING_TITLE: &str = "  herdr";

pub(crate) fn onboarding_welcome_continue_rect(area: Rect) -> Rect {
    super::widgets::continue_button_rect(area)
}
