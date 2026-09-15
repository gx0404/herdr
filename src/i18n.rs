//! Bilingual UI string tables and the process-wide language selection.
//!
//! The default is `zh-CN`; `en` keeps the upstream wording. The language is
//! chosen once at process start from `HERDR_LANG` (wins) or the `language`
//! key in config.toml, and is re-applied on client config reload. All
//! user-visible chrome text goes through the `Texts` tables so both
//! languages stay complete by construction.

use std::sync::atomic::{AtomicU8, Ordering};

pub mod en;
pub mod zh_cn;

pub const LANG_ENV_VAR: &str = "HERDR_LANG";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Lang {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Lang {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "zh-CN" => Some(Lang::ZhCn),
            "en" => Some(Lang::En),
            _ => None,
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(Lang::ZhCn as u8);

pub fn set_lang(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::ZhCn,
    }
}

/// Test helper: switch the language for the guard's lifetime.
pub fn lang_guard(lang: Lang) -> LangGuard {
    let previous = self::lang();
    set_lang(lang);
    LangGuard(previous)
}

pub struct LangGuard(Lang);

impl Drop for LangGuard {
    fn drop(&mut self) {
        set_lang(self.0);
    }
}

/// Resolve the language before any user-visible output: `HERDR_LANG` wins so
/// a broken config cannot pin the wrong language, then a minimal peek at
/// config.toml's `language` key (full config load happens later, with
/// diagnostics).
pub fn init_early() {
    if let Some(lang) = std::env::var(LANG_ENV_VAR)
        .ok()
        .and_then(|value| Lang::parse(&value))
    {
        set_lang(lang);
        return;
    }
    if let Some(lang) = peek_config_language() {
        set_lang(lang);
    }
}

fn peek_config_language() -> Option<Lang> {
    let content = std::fs::read_to_string(crate::config::config_path()).ok()?;
    let value = content.parse::<toml::Value>().ok()?;
    Lang::parse(value.get("language")?.as_str()?)
}

pub struct ChromeTexts {
    pub close_button: &'static str,
    pub continue_button: &'static str,
}

pub struct Texts {
    pub chrome: ChromeTexts,
}

pub fn texts() -> &'static Texts {
    texts_for(lang())
}

pub fn texts_for(lang: Lang) -> &'static Texts {
    match lang {
        Lang::ZhCn => &zh_cn::TEXTS,
        Lang::En => &en::TEXTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_parses_config_values_and_rejects_others() {
        assert_eq!(Lang::parse("zh-CN"), Some(Lang::ZhCn));
        assert_eq!(Lang::parse(" en "), Some(Lang::En));
        assert_eq!(Lang::parse("zh"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn default_language_is_zh_cn() {
        let _guard = lang_guard(Lang::ZhCn);
        assert_eq!(Lang::default(), Lang::ZhCn);
        assert_eq!(lang(), Lang::ZhCn);
    }

    #[test]
    fn lang_guard_restores_previous_language() {
        let _outer = lang_guard(Lang::ZhCn);
        {
            let _inner = lang_guard(Lang::En);
            assert_eq!(lang(), Lang::En);
        }
        assert_eq!(lang(), Lang::ZhCn);
    }

    #[test]
    fn texts_switch_with_language() {
        assert_eq!(texts_for(Lang::En).chrome.close_button, " esc close ");
        assert_eq!(texts_for(Lang::ZhCn).chrome.close_button, " esc 关闭 ");
        assert_eq!(texts_for(Lang::En).chrome.continue_button, " ↵ continue ");
        assert_eq!(texts_for(Lang::ZhCn).chrome.continue_button, " ↵ 继续 ");
    }

    #[test]
    fn init_early_prefers_env_over_config() {
        let _guard = lang_guard(Lang::ZhCn);
        // Without HERDR_LANG set the config peek decides; this environment
        // may not have a config file, so only assert the env precedence.
        std::env::set_var(LANG_ENV_VAR, "en");
        init_early();
        assert_eq!(lang(), Lang::En);
        std::env::remove_var(LANG_ENV_VAR);
    }
}
