//! Pane border glyph sets resolved from `ui.border_style`.
//!
//! Renderers look symbols up in a resolved `BorderGlyphs` table instead of
//! hardcoding one line style. The table is resolved once from config and
//! stored on state, so lookup in the per-cell border loop stays a field read.

use crate::config::BorderStyleConfig;

/// Symbol table for pane borders and other line-drawing chrome.
///
/// Tee names follow the arm direction: `tee_right` is `├` (vertical line with
/// an arm to the right), `tee_down` is `┬`, and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BorderGlyphs {
    pub vertical: &'static str,
    pub horizontal: &'static str,
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub cross: &'static str,
    pub tee_right: &'static str,
    pub tee_left: &'static str,
    pub tee_down: &'static str,
    pub tee_up: &'static str,
}

impl BorderGlyphs {
    pub const SINGLE: Self = Self {
        vertical: "│",
        horizontal: "─",
        top_left: "┌",
        top_right: "┐",
        bottom_left: "└",
        bottom_right: "┘",
        cross: "┼",
        tee_right: "├",
        tee_left: "┤",
        tee_down: "┬",
        tee_up: "┴",
    };

    /// Rounded corners over single-line strokes; tees and crosses have no
    /// rounded variants in Unicode and stay single.
    pub const ROUNDED: Self = Self {
        top_left: "╭",
        top_right: "╮",
        bottom_left: "╰",
        bottom_right: "╯",
        ..Self::SINGLE
    };

    pub const DOUBLE: Self = Self {
        vertical: "║",
        horizontal: "═",
        top_left: "╔",
        top_right: "╗",
        bottom_left: "╚",
        bottom_right: "╝",
        cross: "╬",
        tee_right: "╠",
        tee_left: "╣",
        tee_down: "╦",
        tee_up: "╩",
    };

    pub const THICK: Self = Self {
        vertical: "┃",
        horizontal: "━",
        top_left: "┏",
        top_right: "┓",
        bottom_left: "┗",
        bottom_right: "┛",
        cross: "╋",
        tee_right: "┣",
        tee_left: "┫",
        tee_down: "┳",
        tee_up: "┻",
    };

    /// Resolve the glyph table for a configured border style.
    pub fn for_style(style: BorderStyleConfig) -> Self {
        match style {
            BorderStyleConfig::Single => Self::SINGLE,
            BorderStyleConfig::Rounded => Self::ROUNDED,
            BorderStyleConfig::Double => Self::DOUBLE,
            BorderStyleConfig::Thick => Self::THICK,
        }
    }

    /// Map line connectivity to a glyph. The connectivity mapping itself is
    /// style-independent; only the glyph set changes between styles.
    pub fn line_symbol(self, up: bool, down: bool, left: bool, right: bool) -> &'static str {
        match (up, down, left, right) {
            (true, true, true, true) => self.cross,
            (true, true, true, false) => self.tee_left,
            (true, true, false, true) => self.tee_right,
            (true, false, true, true) => self.tee_up,
            (false, true, true, true) => self.tee_down,
            (true, true, false, false)
            | (true, false, false, false)
            | (false, true, false, false) => self.vertical,
            (false, false, true, true)
            | (false, false, true, false)
            | (false, false, false, true) => self.horizontal,
            (false, true, false, true) => self.top_left,
            (false, true, true, false) => self.top_right,
            (true, false, false, true) => self.bottom_left,
            (true, false, true, false) => self.bottom_right,
            _ => "",
        }
    }

    /// ratatui border set for `Block`-based chrome (client shell panels and
    /// modals). Tees and crosses have no `border::Set` slot and are dropped;
    /// `line_symbol` remains the junction-aware entry point.
    pub fn border_set(self) -> ratatui::symbols::border::Set<'static> {
        ratatui::symbols::border::Set {
            top_left: self.top_left,
            top_right: self.top_right,
            bottom_left: self.bottom_left,
            bottom_right: self.bottom_right,
            vertical_left: self.vertical,
            vertical_right: self.vertical,
            horizontal_top: self.horizontal,
            horizontal_bottom: self.horizontal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_style_matches_the_historical_hardcoded_glyphs() {
        let glyphs = BorderGlyphs::SINGLE;
        assert_eq!(glyphs.line_symbol(true, true, true, true), "┼");
        assert_eq!(glyphs.line_symbol(true, true, true, false), "┤");
        assert_eq!(glyphs.line_symbol(true, true, false, true), "├");
        assert_eq!(glyphs.line_symbol(true, false, true, true), "┴");
        assert_eq!(glyphs.line_symbol(false, true, true, true), "┬");
        assert_eq!(glyphs.line_symbol(true, true, false, false), "│");
        assert_eq!(glyphs.line_symbol(true, false, false, false), "│");
        assert_eq!(glyphs.line_symbol(false, false, true, true), "─");
        assert_eq!(glyphs.line_symbol(false, false, false, true), "─");
        assert_eq!(glyphs.line_symbol(false, true, false, true), "┌");
        assert_eq!(glyphs.line_symbol(false, true, true, false), "┐");
        assert_eq!(glyphs.line_symbol(true, false, false, true), "└");
        assert_eq!(glyphs.line_symbol(true, false, true, false), "┘");
        assert_eq!(glyphs.line_symbol(false, false, false, false), "");
    }

    #[test]
    fn styles_swap_the_glyph_set_not_the_connectivity_mapping() {
        assert_eq!(
            BorderGlyphs::ROUNDED.line_symbol(false, true, false, true),
            "╭"
        );
        assert_eq!(
            BorderGlyphs::ROUNDED.line_symbol(true, true, true, true),
            "┼"
        );
        assert_eq!(
            BorderGlyphs::DOUBLE.line_symbol(false, true, true, false),
            "╗"
        );
        assert_eq!(
            BorderGlyphs::DOUBLE.line_symbol(true, true, true, false),
            "╣"
        );
        assert_eq!(
            BorderGlyphs::THICK.line_symbol(true, false, true, true),
            "┻"
        );
        assert_eq!(
            BorderGlyphs::THICK.line_symbol(true, false, false, false),
            "┃"
        );
    }

    #[test]
    fn for_style_resolves_every_config_variant() {
        assert_eq!(
            BorderGlyphs::for_style(BorderStyleConfig::Single),
            BorderGlyphs::SINGLE
        );
        assert_eq!(
            BorderGlyphs::for_style(BorderStyleConfig::Rounded),
            BorderGlyphs::ROUNDED
        );
        assert_eq!(
            BorderGlyphs::for_style(BorderStyleConfig::Double),
            BorderGlyphs::DOUBLE
        );
        assert_eq!(
            BorderGlyphs::for_style(BorderStyleConfig::Thick),
            BorderGlyphs::THICK
        );
    }

    #[test]
    fn border_set_carries_block_compatible_symbols() {
        let set = BorderGlyphs::DOUBLE.border_set();
        assert_eq!(set.top_left, "╔");
        assert_eq!(set.vertical_left, "║");
        assert_eq!(set.horizontal_bottom, "═");
        assert_eq!(
            BorderGlyphs::for_style(BorderStyleConfig::default()),
            BorderGlyphs::SINGLE
        );
    }
}
