//! 简体中文文案表。键集合必须与 `en.rs` 完全一致（由 `Texts` 结构保证）。

use super::{ChromeTexts, Texts};

pub const TEXTS: Texts = Texts {
    chrome: ChromeTexts {
        close_button: " esc 关闭 ",
        continue_button: " ↵ 继续 ",
    },
};
