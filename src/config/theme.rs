use serde::Deserialize;
use tracing::warn;

use super::model::{ColorDepth, ColorDepthConfig};

/// Default ratio used to mix the selection background toward white/black.
pub const DEFAULT_SELECTION_MIX_RATIO: f32 = 0.28;

pub const THEME_NAMES: &[&str] = &[
    "catppuccin",
    "catppuccin-latte",
    "terminal",
    "tokyo-night",
    "tokyo-night-day",
    "dracula",
    "nord",
    "gruvbox",
    "gruvbox-light",
    "one-dark",
    "one-light",
    "solarized",
    "solarized-light",
    "kanagawa",
    "kanagawa-lotus",
    "rose-pine",
    "rose-pine-dawn",
    "vesper",
];

pub(crate) fn canonical_theme_name(name: &str) -> Option<&'static str> {
    match name.to_lowercase().replace([' ', '_'], "-").as_str() {
        "catppuccin" | "catppuccin-mocha" => Some("catppuccin"),
        "catppuccin-latte" | "latte" | "light" => Some("catppuccin-latte"),
        "terminal" => Some("terminal"),
        "tokyo-night" | "tokyonight" => Some("tokyo-night"),
        "tokyo-night-day" | "tokyo-day" | "tokyonight-day" => Some("tokyo-night-day"),
        "dracula" => Some("dracula"),
        "nord" => Some("nord"),
        "gruvbox" | "gruvbox-dark" => Some("gruvbox"),
        "gruvbox-light" => Some("gruvbox-light"),
        "one-dark" | "onedark" => Some("one-dark"),
        "one-light" | "onelight" => Some("one-light"),
        "solarized" | "solarized-dark" => Some("solarized"),
        "solarized-light" => Some("solarized-light"),
        "kanagawa" => Some("kanagawa"),
        "kanagawa-lotus" | "lotus" => Some("kanagawa-lotus"),
        "rose-pine" | "rosepine" => Some("rose-pine"),
        "rose-pine-dawn" | "rosepine-dawn" | "dawn" => Some("rose-pine-dawn"),
        "vesper" => Some("vesper"),
        _ => None,
    }
}

/// Theme configuration: pick a built-in or override individual tokens.
///
/// ```toml
/// [theme]
/// name = "tokyo-night"  # built-in: catppuccin, terminal, dracula, nord, etc.
///
/// [theme.custom]        # override individual tokens on top of the base
/// accent = "#f5c2e7"
/// red = "#ff6188"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Built-in theme name. Default: "catppuccin".
    pub name: Option<String>,
    /// Follow host terminal light/dark appearance and switch between theme names.
    pub auto_switch: bool,
    /// Theme name used when `auto_switch` selects a dark appearance.
    pub dark_name: Option<String>,
    /// Theme name used when `auto_switch` selects a light appearance.
    pub light_name: Option<String>,
    /// Custom overrides — applied on top of the selected base theme.
    pub custom: Option<CustomThemeColors>,
    /// Component-level token overrides — applied on top of the resolved palette.
    pub components: Option<ThemeComponentsConfig>,
}

impl ThemeConfig {
    pub(crate) fn diagnostics(&self) -> Vec<String> {
        let valid = THEME_NAMES.join(", ");
        let mut diagnostics: Vec<String> = [
            ("theme.name", self.name.as_deref(), "catppuccin"),
            ("theme.dark_name", self.dark_name.as_deref(), "catppuccin"),
            (
                "theme.light_name",
                self.light_name.as_deref(),
                "catppuccin-latte",
            ),
        ]
        .into_iter()
        .filter_map(|(field, value, fallback)| {
            let value = value?;
            canonical_theme_name(value).is_none().then(|| {
                format!(
                    "unknown theme name {field} = {value:?}; using {fallback:?}; valid themes: {valid}"
                )
            })
        })
        .collect();

        if let Some(custom) = &self.custom {
            for (field, value) in custom_theme_color_fields(custom) {
                diagnostics.extend(unknown_color_diagnostic(&field, value));
            }
            if let Some(light) = &custom.light {
                for (field, value) in mode_theme_color_fields(light, "theme.custom.light") {
                    diagnostics.extend(unknown_color_diagnostic(&field, value));
                }
            }
            if let Some(dark) = &custom.dark {
                for (field, value) in mode_theme_color_fields(dark, "theme.custom.dark") {
                    diagnostics.extend(unknown_color_diagnostic(&field, value));
                }
            }
        }
        if let Some(components) = &self.components {
            for (field, value) in component_color_fields(components) {
                diagnostics.extend(unknown_color_diagnostic(&field, value));
            }
            if let Some(ratio) = components.selection_mix_ratio {
                if !(0.0..=1.0).contains(&ratio) {
                    diagnostics.push(format!(
                        "theme.components.selection_mix_ratio = {ratio} is outside the 0.0..=1.0 range; using {DEFAULT_SELECTION_MIX_RATIO}"
                    ));
                }
            }
        }

        diagnostics
    }
}

/// Report an unparsable configured color with its field and value.
/// Parsing itself keeps the historical cyan fallback.
pub(crate) fn unknown_color_diagnostic(field: &str, value: &str) -> Option<String> {
    try_parse_color(value)
        .is_none()
        .then(|| format!("unknown color {field} = {value:?}; using cyan fallback"))
}

macro_rules! color_field_entries {
    ($prefix:expr, $colors:expr, $($field:ident),*) => {
        [$( (format!("{}.{}", $prefix, stringify!($field)), $colors.$field.as_deref()) ),*]
            .into_iter()
            .filter_map(|(field, value)| value.map(|value| (field, value)))
            .collect::<Vec<_>>()
    };
}

fn custom_theme_color_fields(custom: &CustomThemeColors) -> Vec<(String, &str)> {
    color_field_entries!(
        "theme.custom",
        custom,
        accent,
        panel_bg,
        sidebar_bg,
        active_row_bg,
        selection_bg,
        surface0,
        surface1,
        surface_dim,
        overlay0,
        overlay1,
        text,
        subtext0,
        mauve,
        green,
        yellow,
        red,
        blue,
        teal,
        peach
    )
}

fn mode_theme_color_fields<'a>(
    custom: &'a ModeThemeColors,
    prefix: &str,
) -> Vec<(String, &'a str)> {
    color_field_entries!(
        prefix,
        custom,
        accent,
        panel_bg,
        sidebar_bg,
        active_row_bg,
        selection_bg,
        surface0,
        surface1,
        surface_dim,
        overlay0,
        overlay1,
        text,
        subtext0,
        mauve,
        green,
        yellow,
        red,
        blue,
        teal,
        peach
    )
}

fn component_color_fields(components: &ThemeComponentsConfig) -> Vec<(String, &str)> {
    color_field_entries!(
        "theme.components",
        components,
        pane_border_focused,
        pane_border_unfocused,
        scrollbar_thumb,
        scrollbar_track,
        mode_bar_accent,
        toast_border_success,
        toast_border_info,
        toast_border_error,
        hover_bg
    )
}

/// Component-level token overrides on top of the resolved semantic palette.
/// All fields optional — unset fields fall back to their semantic token.
///
/// ```toml
/// [theme.components]
/// pane_border_focused = "#89b4fa"
/// scrollbar_thumb = "#7f849c"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeComponentsConfig {
    /// Border color of the focused pane. Fallback: accent.
    pub pane_border_focused: Option<String>,
    /// Border color of unfocused panes. Fallback: overlay0.
    pub pane_border_unfocused: Option<String>,
    /// Scrollbar thumb color. Fallback: overlay1 focused, overlay0 unfocused.
    pub scrollbar_thumb: Option<String>,
    /// Scrollbar track color. Fallback: overlay0 focused, surface_dim unfocused.
    pub scrollbar_track: Option<String>,
    /// Mode bar accent color. Fallback: accent.
    pub mode_bar_accent: Option<String>,
    /// Success toast border color. Fallback: green.
    pub toast_border_success: Option<String>,
    /// Info toast border color. Fallback: blue.
    pub toast_border_info: Option<String>,
    /// Error toast border color. Fallback: red.
    pub toast_border_error: Option<String>,
    /// Hover background of menu and overlay list rows. Fallback: surface1.
    pub hover_bg: Option<String>,
    /// Selection background mix ratio toward white/black, 0.0..=1.0. Default: 0.28.
    pub selection_mix_ratio: Option<f32>,
}

/// Per-token color overrides. All fields optional — only set what you want to change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CustomThemeColors {
    pub accent: Option<String>,
    pub panel_bg: Option<String>,
    pub sidebar_bg: Option<String>,
    pub active_row_bg: Option<String>,
    pub selection_bg: Option<String>,
    pub surface0: Option<String>,
    pub surface1: Option<String>,
    pub surface_dim: Option<String>,
    pub overlay0: Option<String>,
    pub overlay1: Option<String>,
    pub text: Option<String>,
    pub subtext0: Option<String>,
    pub mauve: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub blue: Option<String>,
    pub teal: Option<String>,
    pub peach: Option<String>,
    /// Overrides applied when `auto_switch` selects a light appearance.
    pub light: Option<ModeThemeColors>,
    /// Overrides applied when `auto_switch` selects a dark appearance.
    pub dark: Option<ModeThemeColors>,
}

/// Per-token color overrides for one auto-switch appearance.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModeThemeColors {
    pub accent: Option<String>,
    pub panel_bg: Option<String>,
    pub sidebar_bg: Option<String>,
    pub active_row_bg: Option<String>,
    pub selection_bg: Option<String>,
    pub surface0: Option<String>,
    pub surface1: Option<String>,
    pub surface_dim: Option<String>,
    pub overlay0: Option<String>,
    pub overlay1: Option<String>,
    pub text: Option<String>,
    pub subtext0: Option<String>,
    pub mauve: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub blue: Option<String>,
    pub teal: Option<String>,
    pub peach: Option<String>,
}

/// Named colors accepted by [`parse_color`]; aliases map to the same color.
const NAMED_COLORS: &[(&str, ratatui::style::Color)] = &[
    ("black", ratatui::style::Color::Black),
    ("red", ratatui::style::Color::Red),
    ("green", ratatui::style::Color::Green),
    ("yellow", ratatui::style::Color::Yellow),
    ("blue", ratatui::style::Color::Blue),
    ("magenta", ratatui::style::Color::Magenta),
    ("purple", ratatui::style::Color::Magenta),
    ("cyan", ratatui::style::Color::Cyan),
    ("white", ratatui::style::Color::White),
    ("gray", ratatui::style::Color::Gray),
    ("grey", ratatui::style::Color::Gray),
    ("darkgray", ratatui::style::Color::DarkGray),
    ("darkgrey", ratatui::style::Color::DarkGray),
    ("lightred", ratatui::style::Color::LightRed),
    ("lightgreen", ratatui::style::Color::LightGreen),
    ("lightyellow", ratatui::style::Color::LightYellow),
    ("lightblue", ratatui::style::Color::LightBlue),
    ("lightmagenta", ratatui::style::Color::LightMagenta),
    ("lightcyan", ratatui::style::Color::LightCyan),
];

/// Strict variant of [`parse_color`]: returns `None` for unrecognized colors
/// instead of falling back to cyan. Accepts the same grammar.
pub fn try_parse_color(s: &str) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    let s = s.trim().to_lowercase();

    match s.as_str() {
        "reset" | "default" | "none" | "transparent" => return Some(Color::Reset),
        _ => {}
    }

    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return Some(Color::Rgb(r, g, b));
            }
        } else if hex.len() == 3 {
            let chars: Vec<u8> = hex
                .chars()
                .filter_map(|c| u8::from_str_radix(&c.to_string(), 16).ok())
                .collect();
            if chars.len() == 3 {
                return Some(Color::Rgb(chars[0] * 17, chars[1] * 17, chars[2] * 17));
            }
        }
    }

    if let Some(inner) = s.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].trim().parse::<u8>(),
                parts[1].trim().parse::<u8>(),
                parts[2].trim().parse::<u8>(),
            ) {
                return Some(Color::Rgb(r, g, b));
            }
        }
    }

    NAMED_COLORS
        .iter()
        .find_map(|(name, color)| (s == *name).then_some(*color))
}

/// Parse a color string into a ratatui Color.
/// Supports: hex (#rrggbb, #rgb), named colors, rgb(r,g,b), and reset aliases.
pub fn parse_color(s: &str) -> ratatui::style::Color {
    match try_parse_color(s) {
        Some(color) => color,
        None => {
            warn!(color = s, "unknown color, defaulting to cyan");
            ratatui::style::Color::Cyan
        }
    }
}

/// Map an RGB color to the nearest xterm-256 palette index.
///
/// Compares the 6x6x6 color cube against the 24-step grayscale ramp and
/// picks the closer entry by squared RGB distance; ties go to the cube.
pub fn rgb_to_xterm256(r: u8, g: u8, b: u8) -> u8 {
    const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    fn cube_channel(value: u8) -> (u8, u8) {
        let mut best_index = 0usize;
        let mut best_distance = u8::MAX;
        for (index, level) in CUBE_LEVELS.iter().enumerate() {
            let distance = value.abs_diff(*level);
            if distance < best_distance {
                best_index = index;
                best_distance = distance;
            }
        }
        (best_index as u8, CUBE_LEVELS[best_index])
    }

    let (cube_r, level_r) = cube_channel(r);
    let (cube_g, level_g) = cube_channel(g);
    let (cube_b, level_b) = cube_channel(b);
    let cube_index = 16 + 36 * cube_r + 6 * cube_g + cube_b;

    // Grayscale ramp: indices 232..=255 map to 8 + 10 * step.
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_step = (average.saturating_sub(8).saturating_add(5) / 10).min(23) as u8;
    let gray_level = 8 + 10 * gray_step;
    let gray_index = 232 + gray_step;

    let distance = |er: u8, eg: u8, eb: u8| -> u32 {
        u32::from(r.abs_diff(er)).pow(2)
            + u32::from(g.abs_diff(eg)).pow(2)
            + u32::from(b.abs_diff(eb)).pow(2)
    };
    if distance(level_r, level_g, level_b) <= distance(gray_level, gray_level, gray_level) {
        cube_index
    } else {
        gray_index
    }
}

/// Downgrade a color for 256-color output. RGB colors map to the nearest
/// xterm-256 index; named, indexed, and reset colors pass through unchanged.
pub fn degrade_color_to_256(color: ratatui::style::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match color {
        Color::Rgb(r, g, b) => Color::Indexed(rgb_to_xterm256(r, g, b)),
        other => other,
    }
}

/// Resolve `ui.color_depth`: explicit values pass through; `auto` detects the
/// host terminal capability from the environment.
pub fn resolve_color_depth(config: ColorDepthConfig) -> ColorDepth {
    match config.depth() {
        ColorDepth::Auto => detect_host_color_depth(),
        explicit => explicit,
    }
}

/// Detect the host terminal's color capability from the environment.
///
/// Positive truecolor evidence (COLORTERM=truecolor/24bit, direct/truecolor
/// TERM) wins; a 256color TERM without that evidence downgrades (the common
/// SSH/tmux shape where COLORTERM is not forwarded); anything else keeps the
/// historical truecolor output.
pub fn detect_host_color_depth() -> ColorDepth {
    detect_host_color_depth_with(|name| std::env::var(name).ok())
}

pub(crate) fn detect_host_color_depth_with(get: impl Fn(&str) -> Option<String>) -> ColorDepth {
    let colorterm = get("COLORTERM").map(|value| value.to_lowercase());
    if matches!(colorterm.as_deref(), Some("truecolor") | Some("24bit")) {
        return ColorDepth::Truecolor;
    }

    let term = get("TERM").unwrap_or_default().to_lowercase();
    if term.contains("truecolor") || term.contains("24bit") || term.contains("-direct") {
        return ColorDepth::Truecolor;
    }
    if term.contains("256color") {
        return ColorDepth::Color256;
    }
    ColorDepth::Truecolor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn theme_name_parses() {
        let toml = r#"
[theme]
name = "dracula"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("dracula"));
    }

    #[test]
    fn unknown_theme_names_are_diagnosed() {
        let config: Config = toml::from_str(
            r#"
[theme]
name = "catppucin"
dark_name = "tokio-night"
light_name = "lattee"
"#,
        )
        .unwrap();

        let diagnostics = config.theme.diagnostics();
        assert_eq!(diagnostics.len(), 3);
        assert!(diagnostics[0].contains("theme.name = \"catppucin\""));
        assert!(diagnostics[0].contains("using \"catppuccin\""));
        assert!(diagnostics[1].contains("theme.dark_name = \"tokio-night\""));
        assert!(diagnostics[2].contains("theme.light_name = \"lattee\""));
        assert!(diagnostics[2].contains("using \"catppuccin-latte\""));
    }

    #[test]
    fn theme_name_aliases_are_valid() {
        for name in ["catppuccin-mocha", "tokyonight", "gruvbox-dark", "dawn"] {
            assert!(canonical_theme_name(name).is_some(), "alias: {name}");
        }
    }

    #[test]
    fn parse_color_accepts_reset_aliases() {
        use ratatui::style::Color;

        for value in ["reset", "default", "none", "transparent"] {
            assert_eq!(parse_color(value), Color::Reset, "value: {value}");
        }
    }

    #[test]
    fn try_parse_color_matches_parse_color_grammar_without_fallback() {
        use ratatui::style::Color;

        for (value, expected) in [
            ("#a1B2c3", Color::Rgb(0xa1, 0xb2, 0xc3)),
            ("#abc", Color::Rgb(0xaa, 0xbb, 0xcc)),
            ("rgb(1, 2, 3)", Color::Rgb(1, 2, 3)),
            ("purple", Color::Magenta),
            ("grey", Color::Gray),
            (" RESET ", Color::Reset),
        ] {
            assert_eq!(try_parse_color(value), Some(expected), "value: {value}");
        }
        for value in ["", "not-a-color", "#abcd", "rgb(1,2,300)", "#zzzzzz"] {
            assert_eq!(try_parse_color(value), None, "value: {value}");
        }
    }

    #[test]
    fn theme_auto_switch_fields_parse() {
        let toml = r#"
[theme]
name = "catppuccin"
auto_switch = true
dark_name = "tokyo-night"
light_name = "catppuccin-latte"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("catppuccin"));
        assert!(config.theme.auto_switch);
        assert_eq!(config.theme.dark_name.as_deref(), Some("tokyo-night"));
        assert_eq!(config.theme.light_name.as_deref(), Some("catppuccin-latte"));
    }

    #[test]
    fn theme_custom_overrides_parse() {
        let toml = r##"
[theme]
name = "nord"

[theme.custom]
panel_bg = "#1e1e2e"
sidebar_bg = "#181825"
active_row_bg = "#313244"
selection_bg = "#45475a"
accent = "#ff79c6"
red = "rgb(255, 85, 85)"
"##;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("nord"));
        let custom = config.theme.custom.as_ref().unwrap();
        assert_eq!(custom.panel_bg.as_deref(), Some("#1e1e2e"));
        assert_eq!(custom.sidebar_bg.as_deref(), Some("#181825"));
        assert_eq!(custom.active_row_bg.as_deref(), Some("#313244"));
        assert_eq!(custom.selection_bg.as_deref(), Some("#45475a"));
        assert_eq!(custom.accent.as_deref(), Some("#ff79c6"));
        assert_eq!(custom.red.as_deref(), Some("rgb(255, 85, 85)"));
        assert!(custom.green.is_none());
    }

    #[test]
    fn theme_custom_mode_overrides_parse() {
        let toml = r##"
[theme.custom]
accent = "#010203"

[theme.custom.light]
accent = "#040506"
text = "#070809"
selection_bg = "#101112"

[theme.custom.dark]
panel_bg = "#0a0b0c"
sidebar_bg = "#0d0e0f"
active_row_bg = "#131415"
"##;
        let config: Config = toml::from_str(toml).unwrap();
        let custom = config.theme.custom.as_ref().unwrap();
        assert_eq!(custom.accent.as_deref(), Some("#010203"));
        let light = custom.light.as_ref().unwrap();
        assert_eq!(light.accent.as_deref(), Some("#040506"));
        assert_eq!(light.text.as_deref(), Some("#070809"));
        assert_eq!(light.selection_bg.as_deref(), Some("#101112"));
        let dark = custom.dark.as_ref().unwrap();
        assert_eq!(dark.panel_bg.as_deref(), Some("#0a0b0c"));
        assert_eq!(dark.sidebar_bg.as_deref(), Some("#0d0e0f"));
        assert_eq!(dark.active_row_bg.as_deref(), Some("#131415"));
    }

    #[test]
    fn theme_defaults_when_missing() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.theme.name.is_none());
        assert!(!config.theme.auto_switch);
        assert!(config.theme.dark_name.is_none());
        assert!(config.theme.light_name.is_none());
        assert!(config.theme.custom.is_none());
        assert!(config.theme.components.is_none());
    }

    #[test]
    fn theme_components_parse_all_fields() {
        let toml = r##"
[theme.components]
pane_border_focused = "#89b4fa"
pane_border_unfocused = "#585b70"
scrollbar_thumb = "#7f849c"
scrollbar_track = "#313244"
mode_bar_accent = "#cba6f7"
toast_border_success = "green"
toast_border_info = "blue"
toast_border_error = "#f38ba8"
selection_mix_ratio = 0.4
"##;
        let config: Config = toml::from_str(toml).unwrap();
        let components = config.theme.components.as_ref().unwrap();
        assert_eq!(components.pane_border_focused.as_deref(), Some("#89b4fa"));
        assert_eq!(components.pane_border_unfocused.as_deref(), Some("#585b70"));
        assert_eq!(components.scrollbar_thumb.as_deref(), Some("#7f849c"));
        assert_eq!(components.scrollbar_track.as_deref(), Some("#313244"));
        assert_eq!(components.mode_bar_accent.as_deref(), Some("#cba6f7"));
        assert_eq!(components.toast_border_success.as_deref(), Some("green"));
        assert_eq!(components.toast_border_info.as_deref(), Some("blue"));
        assert_eq!(components.toast_border_error.as_deref(), Some("#f38ba8"));
        assert_eq!(components.selection_mix_ratio, Some(0.4));
        assert!(config.theme.diagnostics().is_empty());
    }

    #[test]
    fn unknown_theme_custom_colors_are_diagnosed_with_field_and_value() {
        let config: Config = toml::from_str(
            r##"
[theme.custom]
accent = "puce"
green = "#a6e3a1"

[theme.custom.light]
text = "bluish"

[theme.custom.dark]
red = "rgb(300, 0, 0)"
"##,
        )
        .unwrap();

        let diagnostics = config.theme.diagnostics();
        assert_eq!(diagnostics.len(), 3);
        assert!(diagnostics[0].contains("theme.custom.accent = \"puce\""));
        assert!(diagnostics[0].contains("cyan"));
        assert!(diagnostics[1].contains("theme.custom.light.text = \"bluish\""));
        assert!(diagnostics[2].contains("theme.custom.dark.red = \"rgb(300, 0, 0)\""));
    }

    #[test]
    fn unknown_component_colors_and_bad_ratio_are_diagnosed() {
        let config: Config = toml::from_str(
            r##"
[theme.components]
pane_border_focused = "octarine"
toast_border_error = "#f38ba8"
selection_mix_ratio = 1.5
"##,
        )
        .unwrap();

        let diagnostics = config.theme.diagnostics();
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics[0].contains("theme.components.pane_border_focused = \"octarine\""));
        assert!(diagnostics[1].contains("selection_mix_ratio = 1.5"));
        assert!(diagnostics[1].contains("0.0..=1.0"));
    }

    #[test]
    fn component_ratio_nan_is_rejected() {
        let config: Config = toml::from_str(
            r#"
[theme.components]
selection_mix_ratio = nan
"#,
        )
        .unwrap();
        assert_eq!(config.theme.diagnostics().len(), 1);
    }

    #[test]
    fn rgb_to_xterm256_maps_cube_and_grayscale() {
        use ratatui::style::Color;

        // Cube corners and mid-tones land on exact cube indices.
        assert_eq!(rgb_to_xterm256(0, 0, 0), 16);
        assert_eq!(rgb_to_xterm256(255, 255, 255), 231);
        assert_eq!(rgb_to_xterm256(255, 0, 0), 196);
        assert_eq!(rgb_to_xterm256(0, 95, 0), 22);
        // Catppuccin blue 89b4fa -> nearest cube entry (137,180,250-ish).
        let index = rgb_to_xterm256(0x89, 0xb4, 0xfa);
        assert!(index >= 16, "expected palette index, got {index}");
        // Pure grays land on the grayscale ramp.
        assert_eq!(rgb_to_xterm256(128, 128, 128), 244);
        assert_eq!(rgb_to_xterm256(18, 18, 18), 233);
        // Degrade is idempotent and leaves symbolic colors alone.
        assert_eq!(
            degrade_color_to_256(Color::Rgb(128, 128, 128)),
            Color::Indexed(244)
        );
        assert_eq!(
            degrade_color_to_256(Color::Indexed(244)),
            Color::Indexed(244)
        );
        assert_eq!(degrade_color_to_256(Color::Blue), Color::Blue);
        assert_eq!(degrade_color_to_256(Color::Reset), Color::Reset);
    }

    #[test]
    fn detect_host_color_depth_follows_colorterm_then_term() {
        let with = |colorterm: Option<&str>, term: Option<&str>| {
            detect_host_color_depth_with(move |name| match name {
                "COLORTERM" => colorterm.map(str::to_owned),
                "TERM" => term.map(str::to_owned),
                _ => None,
            })
        };

        assert_eq!(with(Some("truecolor"), None), ColorDepth::Truecolor);
        assert_eq!(
            with(Some("24bit"), Some("xterm-256color")),
            ColorDepth::Truecolor
        );
        assert_eq!(with(None, Some("xterm-direct")), ColorDepth::Truecolor);
        assert_eq!(with(None, Some("wezterm-truecolor")), ColorDepth::Truecolor);
        // The SSH/tmux shape: COLORTERM is not forwarded, TERM stays 256color.
        assert_eq!(with(None, Some("xterm-256color")), ColorDepth::Color256);
        assert_eq!(with(None, Some("screen-256color")), ColorDepth::Color256);
        // No contrary evidence keeps historical truecolor output.
        assert_eq!(with(None, Some("xterm")), ColorDepth::Truecolor);
        assert_eq!(with(None, Some("dumb")), ColorDepth::Truecolor);
        assert_eq!(with(None, None), ColorDepth::Truecolor);

        assert_eq!(
            resolve_color_depth(ColorDepthConfig::from(ColorDepth::Color256)),
            ColorDepth::Color256
        );
        assert_eq!(
            resolve_color_depth(ColorDepthConfig::from(ColorDepth::Truecolor)),
            ColorDepth::Truecolor
        );
    }
}
