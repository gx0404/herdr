use crate::config::{Keybinds, NewTerminalCwdConfig, SoundConfig, ToastConfig};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::detect::AgentState;
use crate::layout::{PaneId, PaneInfo};

pub(crate) type InstalledPluginRegistry =
    std::collections::HashMap<String, crate::api::schema::InstalledPluginInfo>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PluginPaneRecord {
    pub plugin_id: String,
    pub entrypoint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopupPaneState {
    pub pane_id: PaneId,
    pub terminal_id: crate::terminal::TerminalId,
    pub width: Option<crate::popup_size::PopupSize>,
    pub height: Option<crate::popup_size::PopupSize>,
}

use crate::terminal_theme::{HostAppearance, TerminalTheme};
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// Theme palette — all UI colors in one place, ready for theming
// ---------------------------------------------------------------------------

/// 行底色之间的最小 WCAG 对比度。低于这个比值在终端上与背景肉眼不可分：
/// rose-pine-dawn 的 `surface1` 对 `panel_bg` 只有 1.05:1（逐通道差 5/6/6），
/// 而同主题的 `selection_bg` 有 1.10:1 且清晰可见，所以门槛取 1.10。
const ROW_BG_MIN_CONTRAST: f64 = 1.10;

/// All colors used by the UI. Derived from a base accent color for now,
/// but structured so a full theme system can replace it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// Primary accent (highlight, active borders).
    pub accent: Color,
    /// Background for the tab bar, floating panels, overlays, and modals.
    pub panel_bg: Color,
    /// Optional desktop sidebar background. Reset preserves the terminal background.
    pub sidebar_bg: Color,
    /// Background for the active workspace and focused agent rows.
    pub active_row_bg: Color,
    /// Background for the Navigate-mode cursor row in the sidebar.
    pub selection_bg: Color,
    /// Subtle surface background for selected/focused items.
    pub surface0: Color,
    /// Slightly lighter surface for hover/active states.
    pub surface1: Color,
    /// Very dim surface for separators.
    pub surface_dim: Color,
    /// Muted text (secondary info, numbers).
    pub overlay0: Color,
    /// Slightly brighter overlay text.
    pub overlay1: Color,
    /// Main text color — soft white.
    pub text: Color,
    /// Subdued text (workspace numbers, dim labels).
    pub subtext0: Color,
    /// Branch name / special label color.
    pub mauve: Color,
    /// Done / idle states.
    pub green: Color,
    /// Working / running states.
    pub yellow: Color,
    /// Needs attention / blocked states.
    pub red: Color,
    /// Unseen / done notification accent.
    pub blue: Color,
    /// Notification accent / unseen markers.
    pub teal: Color,
    /// Interrupted / warning states.
    pub peach: Color,
}

impl Palette {
    /// Catppuccin Mocha — the default.
    pub fn catppuccin() -> Self {
        Self {
            accent: Color::Rgb(137, 180, 250), // blue
            panel_bg: Color::Rgb(24, 24, 37),
            // Solid sidebar background: hosts that paint a wallpaper behind
            // the terminal otherwise bleed it through every Reset cell and
            // the sidebar reads as artifacts instead of a panel.
            sidebar_bg: Color::Rgb(24, 24, 37),
            active_row_bg: Color::Rgb(30, 30, 46),
            selection_bg: Color::Rgb(49, 50, 68),
            surface0: Color::Rgb(49, 50, 68),
            surface1: Color::Rgb(69, 71, 90),
            surface_dim: Color::Rgb(30, 30, 46),
            overlay0: Color::Rgb(108, 112, 134),
            overlay1: Color::Rgb(127, 132, 156),
            text: Color::Rgb(205, 214, 244),
            subtext0: Color::Rgb(166, 173, 200),
            mauve: Color::Rgb(203, 166, 247),
            green: Color::Rgb(166, 227, 161),
            yellow: Color::Rgb(249, 226, 175),
            red: Color::Rgb(243, 139, 168),
            blue: Color::Rgb(137, 180, 250),
            teal: Color::Rgb(148, 226, 213),
            peach: Color::Rgb(250, 179, 135),
        }
    }

    /// Catppuccin Latte — the light Catppuccin flavor.
    pub fn catppuccin_latte() -> Self {
        Self {
            accent: Color::Rgb(30, 102, 245),
            panel_bg: Color::Rgb(239, 241, 245),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(230, 233, 239),
            selection_bg: Color::Rgb(189, 208, 245),
            surface0: Color::Rgb(204, 208, 218),
            surface1: Color::Rgb(188, 192, 204),
            surface_dim: Color::Rgb(230, 233, 239),
            overlay0: Color::Rgb(156, 160, 176),
            overlay1: Color::Rgb(140, 143, 161),
            text: Color::Rgb(76, 79, 105),
            subtext0: Color::Rgb(108, 111, 133),
            mauve: Color::Rgb(136, 57, 239),
            green: Color::Rgb(64, 160, 43),
            yellow: Color::Rgb(223, 142, 29),
            red: Color::Rgb(210, 15, 57),
            blue: Color::Rgb(30, 102, 245),
            teal: Color::Rgb(23, 146, 153),
            peach: Color::Rgb(254, 100, 11),
        }
    }

    /// Terminal 16-color theme.
    ///
    /// `surface0`/`surface1` 用 DarkGray/Gray 而不是 `Reset`（C-28 / ds-02）：
    /// 结构面留成终端背景会让输入框、键帽、侧栏与标签 hover 与常态面板同像素。
    /// `text`/`panel_bg` 保持 `Reset`——正文与面板底色跟随宿主是好设计的部分。
    /// `selection_bg` 也保持 `Reset`：填入 DarkGray 会与聚焦行用的
    /// `active_row_bg` 撞色（`selection_row_bg()` 的 accent 回退正是为此存在，
    /// 上游 #4300）；选中面的可见性交给 `selection_row_bg()` 守。
    pub fn terminal() -> Self {
        Self {
            accent: Color::Blue,
            panel_bg: Color::Reset,
            sidebar_bg: Color::Reset,
            active_row_bg: Color::DarkGray,
            selection_bg: Color::Reset,
            surface0: Color::DarkGray,
            surface1: Color::Gray,
            surface_dim: Color::DarkGray,
            overlay0: Color::Gray,
            overlay1: Color::White,
            text: Color::Reset,
            subtext0: Color::Gray,
            mauve: Color::Gray,
            green: Color::Green,
            yellow: Color::Yellow,
            red: Color::LightRed,
            blue: Color::Blue,
            teal: Color::Cyan,
            peach: Color::Yellow,
        }
    }

    /// Tokyo Night — blue-purple aesthetic.
    pub fn tokyo_night() -> Self {
        Self {
            accent: Color::Rgb(122, 162, 247), // blue
            panel_bg: Color::Rgb(26, 27, 38),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(35, 38, 54),
            selection_bg: Color::Rgb(45, 54, 80),
            surface0: Color::Rgb(36, 40, 59),
            surface1: Color::Rgb(65, 72, 104),
            surface_dim: Color::Rgb(26, 27, 38),
            overlay0: Color::Rgb(86, 95, 137),
            overlay1: Color::Rgb(105, 113, 150),
            text: Color::Rgb(192, 202, 245),
            subtext0: Color::Rgb(169, 177, 214),
            mauve: Color::Rgb(187, 154, 247),
            green: Color::Rgb(158, 206, 106),
            yellow: Color::Rgb(224, 175, 104),
            red: Color::Rgb(247, 118, 142),
            blue: Color::Rgb(122, 162, 247),
            teal: Color::Rgb(125, 207, 255),
            peach: Color::Rgb(255, 158, 100),
        }
    }

    /// Tokyo Night Day — the light Tokyo Night style.
    pub fn tokyo_night_day() -> Self {
        Self {
            accent: Color::Rgb(46, 125, 233),
            panel_bg: Color::Rgb(225, 226, 231),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(210, 211, 218),
            selection_bg: Color::Rgb(182, 202, 231),
            surface0: Color::Rgb(196, 200, 218),
            surface1: Color::Rgb(168, 174, 203),
            surface_dim: Color::Rgb(210, 211, 218),
            overlay0: Color::Rgb(137, 144, 179),
            overlay1: Color::Rgb(104, 112, 154),
            text: Color::Rgb(55, 96, 191),
            subtext0: Color::Rgb(97, 114, 176),
            mauve: Color::Rgb(120, 71, 189),
            green: Color::Rgb(88, 117, 57),
            yellow: Color::Rgb(140, 108, 62),
            red: Color::Rgb(245, 42, 101),
            blue: Color::Rgb(46, 125, 233),
            teal: Color::Rgb(17, 140, 116),
            peach: Color::Rgb(177, 92, 0),
        }
    }

    /// Dracula — purple/pink/green.
    pub fn dracula() -> Self {
        Self {
            accent: Color::Rgb(189, 147, 249), // purple
            panel_bg: Color::Rgb(40, 42, 54),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(55, 60, 82),
            selection_bg: Color::Rgb(70, 63, 93),
            surface0: Color::Rgb(68, 71, 90),
            surface1: Color::Rgb(98, 114, 164),
            surface_dim: Color::Rgb(40, 42, 54),
            overlay0: Color::Rgb(98, 114, 164),
            overlay1: Color::Rgb(130, 140, 180),
            text: Color::Rgb(248, 248, 242),
            subtext0: Color::Rgb(210, 210, 220),
            mauve: Color::Rgb(255, 121, 198), // pink
            green: Color::Rgb(80, 250, 123),
            yellow: Color::Rgb(241, 250, 140),
            red: Color::Rgb(255, 85, 85),
            blue: Color::Rgb(139, 233, 253), // cyan-ish
            teal: Color::Rgb(139, 233, 253),
            peach: Color::Rgb(255, 184, 108),
        }
    }

    /// Nord — frosty blue palette.
    pub fn nord() -> Self {
        Self {
            accent: Color::Rgb(136, 192, 208), // frost
            panel_bg: Color::Rgb(46, 52, 64),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(67, 76, 94),
            selection_bg: Color::Rgb(64, 80, 93),
            surface0: Color::Rgb(59, 66, 82),
            surface1: Color::Rgb(67, 76, 94),
            surface_dim: Color::Rgb(46, 52, 64),
            overlay0: Color::Rgb(76, 86, 106),
            overlay1: Color::Rgb(100, 110, 130),
            text: Color::Rgb(236, 239, 244),
            subtext0: Color::Rgb(216, 222, 233),
            mauve: Color::Rgb(180, 142, 173),
            green: Color::Rgb(163, 190, 140),
            yellow: Color::Rgb(235, 203, 139),
            red: Color::Rgb(191, 97, 106),
            blue: Color::Rgb(129, 161, 193),
            teal: Color::Rgb(143, 188, 187),
            peach: Color::Rgb(208, 135, 112),
        }
    }

    /// Gruvbox Dark — warm retro palette.
    pub fn gruvbox() -> Self {
        Self {
            accent: Color::Rgb(215, 153, 33), // yellow
            panel_bg: Color::Rgb(40, 40, 40),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(50, 49, 48),
            selection_bg: Color::Rgb(75, 63, 39),
            surface0: Color::Rgb(60, 56, 54),
            surface1: Color::Rgb(80, 73, 69),
            surface_dim: Color::Rgb(40, 40, 40),
            overlay0: Color::Rgb(146, 131, 116),
            overlay1: Color::Rgb(168, 153, 132),
            text: Color::Rgb(235, 219, 178),
            subtext0: Color::Rgb(213, 196, 161),
            mauve: Color::Rgb(211, 134, 155),
            green: Color::Rgb(184, 187, 38),
            yellow: Color::Rgb(250, 189, 47),
            red: Color::Rgb(251, 73, 52),
            blue: Color::Rgb(131, 165, 152),
            teal: Color::Rgb(142, 192, 124),
            peach: Color::Rgb(254, 128, 25),
        }
    }

    /// Gruvbox Light — the light retro palette.
    pub fn gruvbox_light() -> Self {
        Self {
            accent: Color::Rgb(7, 102, 120),
            panel_bg: Color::Rgb(251, 241, 199),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(242, 229, 188),
            selection_bg: Color::Rgb(235, 219, 178),
            surface0: Color::Rgb(235, 219, 178),
            surface1: Color::Rgb(213, 196, 161),
            surface_dim: Color::Rgb(242, 229, 188),
            overlay0: Color::Rgb(146, 131, 116),
            overlay1: Color::Rgb(124, 111, 100),
            text: Color::Rgb(60, 56, 54),
            subtext0: Color::Rgb(80, 73, 69),
            mauve: Color::Rgb(143, 63, 113),
            green: Color::Rgb(121, 116, 14),
            yellow: Color::Rgb(181, 118, 20),
            red: Color::Rgb(157, 0, 6),
            blue: Color::Rgb(7, 102, 120),
            teal: Color::Rgb(66, 123, 88),
            peach: Color::Rgb(175, 58, 3),
        }
    }

    /// One Dark — Atom's classic dark theme.
    pub fn one_dark() -> Self {
        Self {
            accent: Color::Rgb(97, 175, 239), // blue
            panel_bg: Color::Rgb(40, 44, 52),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(49, 54, 64),
            selection_bg: Color::Rgb(51, 70, 89),
            surface0: Color::Rgb(44, 49, 58),
            surface1: Color::Rgb(62, 68, 81),
            surface_dim: Color::Rgb(40, 44, 52),
            overlay0: Color::Rgb(92, 99, 112),
            overlay1: Color::Rgb(115, 122, 135),
            text: Color::Rgb(171, 178, 191),
            subtext0: Color::Rgb(150, 156, 168),
            mauve: Color::Rgb(198, 120, 221),
            green: Color::Rgb(152, 195, 121),
            yellow: Color::Rgb(229, 192, 123),
            red: Color::Rgb(224, 108, 117),
            blue: Color::Rgb(97, 175, 239),
            teal: Color::Rgb(86, 182, 194),
            peach: Color::Rgb(209, 154, 102),
        }
    }

    /// One Light — Atom's classic light theme.
    pub fn one_light() -> Self {
        Self {
            accent: Color::Rgb(64, 120, 242),
            panel_bg: Color::Rgb(250, 250, 250),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(216, 219, 226),
            selection_bg: Color::Rgb(205, 219, 248),
            surface0: Color::Rgb(240, 240, 241),
            surface1: Color::Rgb(229, 229, 230),
            surface_dim: Color::Rgb(245, 245, 246),
            overlay0: Color::Rgb(160, 161, 167),
            overlay1: Color::Rgb(104, 107, 119),
            text: Color::Rgb(56, 58, 66),
            subtext0: Color::Rgb(104, 107, 119),
            mauve: Color::Rgb(166, 38, 164),
            green: Color::Rgb(80, 161, 79),
            yellow: Color::Rgb(193, 132, 1),
            red: Color::Rgb(228, 86, 73),
            blue: Color::Rgb(64, 120, 242),
            teal: Color::Rgb(1, 132, 188),
            peach: Color::Rgb(152, 104, 1),
        }
    }

    /// Solarized Dark — Ethan Schoonover's classic.
    pub fn solarized() -> Self {
        Self {
            accent: Color::Rgb(38, 139, 210), // blue
            panel_bg: Color::Rgb(0, 43, 54),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(22, 75, 87),
            selection_bg: Color::Rgb(8, 62, 85),
            surface0: Color::Rgb(7, 54, 66),
            surface1: Color::Rgb(88, 110, 117),
            surface_dim: Color::Rgb(0, 43, 54),
            overlay0: Color::Rgb(88, 110, 117),
            overlay1: Color::Rgb(101, 123, 131),
            text: Color::Rgb(147, 161, 161),
            subtext0: Color::Rgb(131, 148, 150),
            mauve: Color::Rgb(211, 54, 130),
            green: Color::Rgb(133, 153, 0),
            yellow: Color::Rgb(181, 137, 0),
            red: Color::Rgb(220, 50, 47),
            blue: Color::Rgb(38, 139, 210),
            teal: Color::Rgb(42, 161, 152),
            peach: Color::Rgb(203, 75, 22),
        }
    }

    /// Solarized Light — Ethan Schoonover's light variant.
    pub fn solarized_light() -> Self {
        Self {
            accent: Color::Rgb(38, 139, 210),
            panel_bg: Color::Rgb(253, 246, 227),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(238, 232, 213),
            selection_bg: Color::Rgb(201, 220, 223),
            surface0: Color::Rgb(238, 232, 213),
            surface1: Color::Rgb(147, 161, 161),
            surface_dim: Color::Rgb(238, 232, 213),
            overlay0: Color::Rgb(147, 161, 161),
            overlay1: Color::Rgb(88, 110, 117),
            text: Color::Rgb(101, 123, 131),
            subtext0: Color::Rgb(131, 148, 150),
            mauve: Color::Rgb(211, 54, 130),
            green: Color::Rgb(133, 153, 0),
            yellow: Color::Rgb(181, 137, 0),
            red: Color::Rgb(220, 50, 47),
            blue: Color::Rgb(38, 139, 210),
            teal: Color::Rgb(42, 161, 152),
            peach: Color::Rgb(203, 75, 22),
        }
    }

    /// Kanagawa — inspired by Katsushika Hokusai.
    pub fn kanagawa() -> Self {
        Self {
            accent: Color::Rgb(126, 156, 216), // blue
            panel_bg: Color::Rgb(31, 31, 40),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(54, 54, 70),
            selection_bg: Color::Rgb(50, 56, 75),
            surface0: Color::Rgb(42, 42, 55),
            surface1: Color::Rgb(54, 54, 70),
            surface_dim: Color::Rgb(31, 31, 40),
            overlay0: Color::Rgb(114, 113, 105),
            overlay1: Color::Rgb(135, 134, 125),
            text: Color::Rgb(220, 215, 186),
            subtext0: Color::Rgb(200, 195, 170),
            mauve: Color::Rgb(149, 127, 184),
            green: Color::Rgb(118, 148, 106),
            yellow: Color::Rgb(192, 163, 110),
            red: Color::Rgb(195, 64, 67),
            blue: Color::Rgb(126, 156, 216),
            teal: Color::Rgb(127, 180, 202),
            peach: Color::Rgb(255, 160, 102),
        }
    }

    /// Kanagawa Lotus — the light Kanagawa variant.
    pub fn kanagawa_lotus() -> Self {
        Self {
            accent: Color::Rgb(77, 105, 155),
            panel_bg: Color::Rgb(242, 236, 188),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(213, 206, 163),
            selection_bg: Color::Rgb(220, 213, 172),
            surface0: Color::Rgb(220, 213, 172),
            surface1: Color::Rgb(201, 203, 209),
            surface_dim: Color::Rgb(213, 206, 163),
            overlay0: Color::Rgb(160, 156, 172),
            overlay1: Color::Rgb(138, 137, 128),
            text: Color::Rgb(84, 84, 100),
            subtext0: Color::Rgb(67, 67, 108),
            mauve: Color::Rgb(98, 76, 131),
            green: Color::Rgb(111, 137, 78),
            yellow: Color::Rgb(119, 113, 63),
            red: Color::Rgb(200, 64, 83),
            blue: Color::Rgb(77, 105, 155),
            teal: Color::Rgb(78, 140, 162),
            peach: Color::Rgb(204, 109, 0),
        }
    }

    /// Rosé Pine — muted, elegant.
    pub fn rose_pine() -> Self {
        Self {
            accent: Color::Rgb(196, 167, 231), // iris
            panel_bg: Color::Rgb(25, 23, 36),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(38, 35, 58),
            selection_bg: Color::Rgb(59, 52, 75),
            surface0: Color::Rgb(31, 29, 46),
            surface1: Color::Rgb(38, 35, 58),
            surface_dim: Color::Rgb(38, 35, 58),
            overlay0: Color::Rgb(110, 106, 134),
            overlay1: Color::Rgb(144, 140, 170),
            text: Color::Rgb(224, 222, 244),
            subtext0: Color::Rgb(200, 197, 220),
            mauve: Color::Rgb(196, 167, 231),  // iris
            green: Color::Rgb(49, 116, 143),   // pine
            yellow: Color::Rgb(246, 193, 119), // gold
            red: Color::Rgb(235, 111, 146),    // love
            blue: Color::Rgb(49, 116, 143),    // pine
            teal: Color::Rgb(156, 207, 216),   // foam
            peach: Color::Rgb(234, 154, 151),  // rose
        }
    }

    /// Rosé Pine Dawn — the light Rosé Pine variant.
    pub fn rose_pine_dawn() -> Self {
        Self {
            accent: Color::Rgb(144, 122, 169),
            panel_bg: Color::Rgb(250, 244, 237),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(227, 217, 207),
            selection_bg: Color::Rgb(242, 233, 225),
            surface0: Color::Rgb(242, 233, 225),
            surface1: Color::Rgb(255, 250, 243),
            surface_dim: Color::Rgb(242, 233, 225),
            overlay0: Color::Rgb(152, 147, 165),
            overlay1: Color::Rgb(121, 117, 147),
            text: Color::Rgb(70, 66, 97),
            subtext0: Color::Rgb(121, 117, 147),
            mauve: Color::Rgb(144, 122, 169),
            green: Color::Rgb(40, 105, 131),
            yellow: Color::Rgb(234, 157, 52),
            red: Color::Rgb(180, 99, 122),
            blue: Color::Rgb(40, 105, 131),
            teal: Color::Rgb(86, 148, 159),
            peach: Color::Rgb(215, 130, 126),
        }
    }

    /// Vesper — minimal high-contrast monochrome with peach and mint accents.
    pub fn vesper() -> Self {
        Self {
            accent: Color::Rgb(255, 199, 153),
            panel_bg: Color::Rgb(26, 26, 26),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(16, 16, 16),
            selection_bg: Color::Rgb(35, 35, 35),
            surface0: Color::Rgb(35, 35, 35),
            surface1: Color::Rgb(40, 40, 40),
            surface_dim: Color::Rgb(16, 16, 16),
            overlay0: Color::Rgb(92, 92, 92),
            overlay1: Color::Rgb(126, 126, 126),
            text: Color::Rgb(255, 255, 255),
            subtext0: Color::Rgb(160, 160, 160),
            mauve: Color::Rgb(255, 209, 168),
            green: Color::Rgb(153, 255, 228),
            yellow: Color::Rgb(255, 199, 153),
            red: Color::Rgb(255, 128, 128),
            blue: Color::Rgb(176, 176, 176),
            teal: Color::Rgb(102, 221, 204),
            peach: Color::Rgb(255, 199, 153),
        }
    }

    /// Resolve a theme by name. Returns None for unknown names.
    pub fn from_name(name: &str) -> Option<Self> {
        match crate::config::canonical_theme_name(name)? {
            "catppuccin" => Some(Self::catppuccin()),
            "catppuccin-latte" => Some(Self::catppuccin_latte()),
            "terminal" => Some(Self::terminal()),
            "tokyo-night" => Some(Self::tokyo_night()),
            "tokyo-night-day" => Some(Self::tokyo_night_day()),
            "dracula" => Some(Self::dracula()),
            "nord" => Some(Self::nord()),
            "gruvbox" => Some(Self::gruvbox()),
            "gruvbox-light" => Some(Self::gruvbox_light()),
            "one-dark" => Some(Self::one_dark()),
            "one-light" => Some(Self::one_light()),
            "solarized" => Some(Self::solarized()),
            "solarized-light" => Some(Self::solarized_light()),
            "kanagawa" => Some(Self::kanagawa()),
            "kanagawa-lotus" => Some(Self::kanagawa_lotus()),
            "rose-pine" => Some(Self::rose_pine()),
            "rose-pine-dawn" => Some(Self::rose_pine_dawn()),
            "vesper" => Some(Self::vesper()),
            _ => None,
        }
    }

    /// Apply custom color overrides on top of this palette.
    pub fn with_overrides(mut self, custom: &crate::config::CustomThemeColors) -> Self {
        use crate::config::parse_color;
        if let Some(c) = &custom.accent {
            self.accent = parse_color(c);
        }
        if let Some(c) = &custom.panel_bg {
            self.panel_bg = parse_color(c);
        }
        if let Some(c) = &custom.sidebar_bg {
            self.sidebar_bg = parse_color(c);
        }
        if let Some(c) = &custom.active_row_bg {
            self.active_row_bg = parse_color(c);
        }
        if let Some(c) = &custom.selection_bg {
            self.selection_bg = parse_color(c);
        }
        if let Some(c) = &custom.surface0 {
            self.surface0 = parse_color(c);
        }
        if let Some(c) = &custom.surface1 {
            self.surface1 = parse_color(c);
        }
        if let Some(c) = &custom.surface_dim {
            self.surface_dim = parse_color(c);
        }
        if let Some(c) = &custom.overlay0 {
            self.overlay0 = parse_color(c);
        }
        if let Some(c) = &custom.overlay1 {
            self.overlay1 = parse_color(c);
        }
        if let Some(c) = &custom.text {
            self.text = parse_color(c);
        }
        if let Some(c) = &custom.subtext0 {
            self.subtext0 = parse_color(c);
        }
        if let Some(c) = &custom.mauve {
            self.mauve = parse_color(c);
        }
        if let Some(c) = &custom.green {
            self.green = parse_color(c);
        }
        if let Some(c) = &custom.yellow {
            self.yellow = parse_color(c);
        }
        if let Some(c) = &custom.red {
            self.red = parse_color(c);
        }
        if let Some(c) = &custom.blue {
            self.blue = parse_color(c);
        }
        if let Some(c) = &custom.teal {
            self.teal = parse_color(c);
        }
        if let Some(c) = &custom.peach {
            self.peach = parse_color(c);
        }
        self
    }

    pub fn with_mode_overrides(mut self, custom: &crate::config::ModeThemeColors) -> Self {
        use crate::config::parse_color;
        if let Some(c) = &custom.accent {
            self.accent = parse_color(c);
        }
        if let Some(c) = &custom.panel_bg {
            self.panel_bg = parse_color(c);
        }
        if let Some(c) = &custom.sidebar_bg {
            self.sidebar_bg = parse_color(c);
        }
        if let Some(c) = &custom.active_row_bg {
            self.active_row_bg = parse_color(c);
        }
        if let Some(c) = &custom.selection_bg {
            self.selection_bg = parse_color(c);
        }
        if let Some(c) = &custom.surface0 {
            self.surface0 = parse_color(c);
        }
        if let Some(c) = &custom.surface1 {
            self.surface1 = parse_color(c);
        }
        if let Some(c) = &custom.surface_dim {
            self.surface_dim = parse_color(c);
        }
        if let Some(c) = &custom.overlay0 {
            self.overlay0 = parse_color(c);
        }
        if let Some(c) = &custom.overlay1 {
            self.overlay1 = parse_color(c);
        }
        if let Some(c) = &custom.text {
            self.text = parse_color(c);
        }
        if let Some(c) = &custom.subtext0 {
            self.subtext0 = parse_color(c);
        }
        if let Some(c) = &custom.mauve {
            self.mauve = parse_color(c);
        }
        if let Some(c) = &custom.green {
            self.green = parse_color(c);
        }
        if let Some(c) = &custom.yellow {
            self.yellow = parse_color(c);
        }
        if let Some(c) = &custom.red {
            self.red = parse_color(c);
        }
        if let Some(c) = &custom.blue {
            self.blue = parse_color(c);
        }
        if let Some(c) = &custom.teal {
            self.teal = parse_color(c);
        }
        if let Some(c) = &custom.peach {
            self.peach = parse_color(c);
        }
        self
    }

    /// Map RGB tokens to the xterm-256 palette for low-color output.
    /// Symbolic (named/indexed/reset) tokens pass through; the mapping is
    /// idempotent, so applying it to an already degraded palette is a no-op.
    pub fn with_color_depth(mut self, depth: crate::config::ColorDepth) -> Self {
        if !depth.is_low_color() {
            return self;
        }
        for token in [
            &mut self.accent,
            &mut self.panel_bg,
            &mut self.sidebar_bg,
            &mut self.active_row_bg,
            &mut self.selection_bg,
            &mut self.surface0,
            &mut self.surface1,
            &mut self.surface_dim,
            &mut self.overlay0,
            &mut self.overlay1,
            &mut self.text,
            &mut self.subtext0,
            &mut self.mauve,
            &mut self.green,
            &mut self.yellow,
            &mut self.red,
            &mut self.blue,
            &mut self.teal,
            &mut self.peach,
        ] {
            *token = crate::config::degrade_color_to_256(*token);
        }
        self
    }

    /// 列表选中行的底色。terminal 之类的 16 色主题把 `selection_bg` 留成
    /// `Color::Reset`（即终端默认背景），直接拿它当选中底色会让选中行与
    /// 普通行像素完全一致；回退到 `active_row_bg` 又会撞上「聚焦行」——侧栏
    /// 聚焦的 workspace 用的正是 `active_row_bg`，导航光标停在非聚焦行上时
    /// 依旧看不出来（上游 #4300）。所以优先回退到 `accent`（terminal 下是
    /// Blue，与 `active_row_bg` 的 DarkGray 可区分），accent 也未定义时才退到
    /// `active_row_bg` 保底。纯函数、无分配，供侧栏行循环按格调用。
    pub fn selection_row_bg(&self) -> Color {
        if self.selection_bg != Color::Reset {
            self.selection_bg
        } else if self.accent != Color::Reset {
            self.accent
        } else {
            self.active_row_bg
        }
    }

    /// 列表「选中」行的弱底色（窄屏工作区列表用）：只要比常态行亮一档，
    /// 不像 `selection_row_bg` 那样做 accent 反色。
    ///
    /// 但它必须与同一列表里的「聚焦行」（`surface_dim`）和常态行（`panel_bg`）
    /// 都不同：16 色主题可能把 `surface0` 留成终端默认背景（= 常态行），
    /// terminal 主题则让 `surface0` 与 `surface_dim` 同为 DarkGray（选中行看起
    /// 来就是聚焦行）。撞色时退到 `selection_row_bg()`——宁可给一个更显眼的
    /// 选中色，也不要「选中 = 常态」或「选中 = 聚焦」。
    pub fn surface_selection_bg(&self) -> Color {
        if self.surface0 == Color::Reset
            || self.surface0 == self.surface_dim
            || self.surface0 == self.panel_bg
        {
            self.selection_row_bg()
        } else {
            self.surface0
        }
    }

    /// 菜单 / 浮层列表行的「指针悬浮」底色。必须比选中行弱（选中行用 accent
    /// 反色），又必须与常态行可区分——否则鼠标路过看起来就是键盘选中，回车
    /// 会激活指针早已离开的那一项（MENU-01）。
    ///
    /// 真正要保证的是「悬浮 ≠ 常态」，而常态行画的是 `panel_bg`，所以候选按
    /// 「弱 → 强」依次试，取第一个与 `panel_bg` 对比度过门槛的：`surface1`
    /// 在多数主题里就是标准 hover 面，但 rose-pine-dawn 这类浅色主题的
    /// `surface1` 比 `panel_bg` 还亮且只差几个色阶（1.05:1），在终端上肉眼
    /// 完全不可分。只在主题解析时算一次（`ComponentStyles::resolve`），不在
    /// 渲染循环里。
    pub fn hover_row_bg(&self) -> Color {
        for candidate in [
            self.surface1,
            self.surface0,
            self.surface_dim,
            self.selection_bg,
            self.active_row_bg,
        ] {
            if Self::row_bg_is_distinct(self.panel_bg, candidate) {
                return candidate;
            }
        }
        // 自定义主题把所有候选填成了同一个色（或全是 Reset）：宁可退到选中
        // 行的底色，也不要一个画了等于没画的 hover。
        self.selection_row_bg()
    }

    /// 两个行底色在终端上是否肉眼可辨。真彩色按 WCAG 对比度判定；16 色 /
    /// 索引色的实际亮度由终端配置决定，无法计算，只能退化成「不是同一个
    /// 色号」——这与 `selection_row_bg` 对 terminal 主题的既有取舍一致。
    /// 输入框聚焦态的底色挑选（`ui::widgets::input_field_focused_bg`）复用
    /// 同一口径。
    pub(crate) fn row_bg_is_distinct(base: Color, candidate: Color) -> bool {
        if candidate == Color::Reset || base == candidate {
            return false;
        }
        match Self::rgb_contrast_ratio(base, candidate) {
            Some(ratio) => ratio >= ROW_BG_MIN_CONTRAST,
            None => true,
        }
    }

    /// WCAG 相对亮度；非真彩色返回 None。
    fn rgb_relative_luminance(color: Color) -> Option<f64> {
        let Color::Rgb(r, g, b) = color else {
            return None;
        };
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        Some(0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b))
    }

    /// WCAG 对比度；任一侧不是真彩色时返回 None。
    fn rgb_contrast_ratio(a: Color, b: Color) -> Option<f64> {
        let a = Self::rgb_relative_luminance(a)?;
        let b = Self::rgb_relative_luminance(b)?;
        Some((a.max(b) + 0.05) / (a.min(b) + 0.05))
    }
}

/// Geometry for the server-rendered active-tab pane surface.
pub struct ViewState {
    pub terminal_area: Rect,
    pub pane_infos: Vec<PaneInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Navigate,
    Terminal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentPanelSort {
    #[default]
    Spaces,
    Launch,
}

#[derive(Debug, Clone)]
pub struct ThemeRuntimeConfig {
    pub manual_name: String,
    pub dark_name: String,
    pub light_name: String,
    pub auto_switch: bool,
    pub custom: Option<crate::config::CustomThemeColors>,
    pub legacy_accent: Option<String>,
    /// Component-level token overrides from `[theme.components]`.
    pub components: Option<crate::config::ThemeComponentsConfig>,
    /// Raw `ui.color_depth` selection; `auto` resolves against the host
    /// environment once at the app/client boundary, not in renderers.
    pub color_depth: crate::config::ColorDepthConfig,
}

/// Resolved component-level styles. Computed once per theme resolution from
/// the (already depth-adjusted) palette plus `[theme.components]`; renderers
/// read these fields directly and never consult config in the render loop.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentStyles {
    /// Border color of the focused pane. Fallback: accent.
    pub pane_border_focused: Color,
    /// Border color of unfocused panes. Fallback: overlay0.
    pub pane_border_unfocused: Color,
    /// Scrollbar thumb on the focused pane. Fallback: overlay1.
    pub scrollbar_thumb_focused: Color,
    /// Scrollbar thumb on unfocused panes. Fallback: overlay0.
    pub scrollbar_thumb_unfocused: Color,
    /// Scrollbar track on the focused pane. Fallback: overlay0.
    pub scrollbar_track_focused: Color,
    /// Scrollbar track on unfocused panes. Fallback: surface_dim.
    pub scrollbar_track_unfocused: Color,
    /// Mode bar accent. Fallback: accent.
    pub mode_bar_accent: Color,
    /// Success toast border. Fallback: green.
    pub toast_border_success: Color,
    /// Info toast border. Fallback: blue.
    pub toast_border_info: Color,
    /// Error toast border. Fallback: red.
    pub toast_border_error: Color,
    /// 菜单 / 浮层列表行的指针悬浮底色。Fallback: `Palette::hover_row_bg()`。
    pub hover_bg: Color,
    /// Selection background mix ratio toward white/black, within 0.0..=1.0.
    pub selection_mix_ratio: f32,
    /// Effective output color depth after `auto` resolution.
    pub color_depth: crate::config::ColorDepth,
}

impl ComponentStyles {
    /// Resolve component tokens on top of `palette`. The palette is expected
    /// to be depth-adjusted already; explicit component overrides are mapped
    /// through the same depth so fallback and override colors stay consistent.
    pub fn resolve(
        palette: &Palette,
        components: Option<&crate::config::ThemeComponentsConfig>,
        color_depth: crate::config::ColorDepth,
    ) -> Self {
        let overridden = |value: Option<&String>, fallback: Color| match value {
            Some(raw) => {
                let parsed = crate::config::parse_color(raw);
                if color_depth.is_low_color() {
                    crate::config::degrade_color_to_256(parsed)
                } else {
                    parsed
                }
            }
            None => fallback,
        };
        let component = |pick: fn(&crate::config::ThemeComponentsConfig) -> &Option<String>| {
            components.and_then(|components| pick(components).as_ref())
        };

        let selection_mix_ratio = components
            .and_then(|components| components.selection_mix_ratio)
            .filter(|ratio| (0.0..=1.0).contains(ratio))
            .unwrap_or(crate::config::DEFAULT_SELECTION_MIX_RATIO);

        Self {
            pane_border_focused: overridden(component(|c| &c.pane_border_focused), palette.accent),
            pane_border_unfocused: overridden(
                component(|c| &c.pane_border_unfocused),
                palette.overlay0,
            ),
            scrollbar_thumb_focused: overridden(
                component(|c| &c.scrollbar_thumb),
                palette.overlay1,
            ),
            scrollbar_thumb_unfocused: overridden(
                component(|c| &c.scrollbar_thumb),
                palette.overlay0,
            ),
            scrollbar_track_focused: overridden(
                component(|c| &c.scrollbar_track),
                palette.overlay0,
            ),
            scrollbar_track_unfocused: overridden(
                component(|c| &c.scrollbar_track),
                palette.surface_dim,
            ),
            mode_bar_accent: overridden(component(|c| &c.mode_bar_accent), palette.accent),
            toast_border_success: overridden(component(|c| &c.toast_border_success), palette.green),
            toast_border_info: overridden(component(|c| &c.toast_border_info), palette.blue),
            toast_border_error: overridden(component(|c| &c.toast_border_error), palette.red),
            hover_bg: overridden(component(|c| &c.hover_bg), palette.hover_row_bg()),
            selection_mix_ratio,
            color_depth,
        }
    }

    /// Test-only convenience: defaults derived from the palette alone — no
    /// component overrides and truecolor output.
    #[cfg(test)]
    pub fn from_palette(palette: &Palette) -> Self {
        Self::resolve(palette, None, crate::config::ColorDepth::Truecolor)
    }
}

/// One theme resolution pass: semantic palette plus component tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTheme {
    pub palette: Palette,
    pub components: ComponentStyles,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    NeedsAttention,
    Finished,
    UpdateInstalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastTarget {
    pub workspace_id: String,
    pub pane_id: PaneId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastNotification {
    pub kind: ToastKind,
    pub title: String,
    pub context: String,
    pub position: Option<crate::config::ToastHerdrPosition>,
    pub target: Option<ToastTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentNotification {
    pub pane_id: PaneId,
    pub workspace_id: String,
    pub agent_label: String,
    pub known_agent: Option<crate::detect::Agent>,
    pub kind: ToastKind,
    pub state: AgentState,
    pub deadline: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNotificationDelivery {
    pub pane_id: PaneId,
    pub workspace_id: String,
    pub agent_label: String,
    pub known_agent: Option<crate::detect::Agent>,
    pub kind: ToastKind,
    pub toast: Option<ToastNotification>,
    pub client_notification: Option<ToastNotification>,
    pub sound: Option<crate::sound::Sound>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyFeedback {
    pub message: String,
}

#[derive(Debug)]
pub struct ReleaseNotesState {
    pub version: String,
    pub body: String,
    pub scroll: u16,
    pub preview: bool,
}

#[derive(Debug)]
pub struct ProductAnnouncementState {
    pub version: String,
    pub id: String,
    pub title: String,
    pub body: String,
    pub scroll: u16,
    pub preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneFocusTarget {
    pub workspace_id: String,
    pub pane_id: PaneId,
}

/// 每个 agent 保留的活动节点上限：超出的部分截断并置 `truncated`（运行中的节点
/// 及其祖先优先保留），全量经 `agent.activity.read` 按需取。
pub const MAX_AGENT_ACTIVITY_NODES: usize = 32;

/// 外部来源连续这么久没有一次成功刷新，其条目标为「暂不可读」（`readable =
/// false`），但不删除：来源恢复后整源替换回来。
pub const EXTERNAL_AGENT_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// 每个 pane 缓存的最近一份 `pane.report_agent_activity` hint 的字节上限（与 API
/// 单请求上限同量级）；更长的提示只当信号、不缓存文本。
pub const MAX_AGENT_ACTIVITY_HINT_BYTES: usize = 1024 * 1024;

/// 一个 agent（pane）的活动树快照：后台适配器一次发现的结果，截断后落库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentActivitySnapshot {
    /// 截断后的节点，保持来源顺序。
    pub nodes: Vec<crate::api::schema::AgentActivityNode>,
    /// 截断前的运行中节点数。
    pub running: u32,
    /// 截断前的节点总数。
    pub total: u32,
    pub truncated: bool,
    /// 最近一次刷新（含内容未变的刷新）的时刻。
    pub refreshed_at: std::time::Instant,
    /// 内容每变化一次递增；同一 pane 内单调。
    pub revision: u64,
}

/// 一次活动树变化后的计数，随 `pane.agent_activity_changed` 事件下发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentActivityCounts {
    pub running: u32,
    pub total: u32,
}

/// 一次活动树写入的结果（内容有变化时给出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentActivityApplied {
    /// 截断前的计数，进 `pane.agent_activity_changed`。
    pub counts: AgentActivityCounts,
    /// 客户端快照里可见的那部分（计数、截断标记、最新节点）是否变化。只有它为真
    /// 才需要重建每客户端投影：整棵树照常落库，深层节点的变化走
    /// `agent.activity.read` / `agent.get`，不值得让每个挂载客户端整份重建快照。
    pub summary_changed: bool,
}

/// 客户端快照摘要里的「最新节点」（`server::client_shell::SnapshotActivity::
/// Summary`）：优先运行中的节点、取开始时间最晚者；没有运行中的节点时取结束
/// （缺失则开始）时间最晚者。时间缺失视为最早，同分取来源顺序靠后者。
pub(crate) fn latest_activity_node(
    nodes: &[crate::api::schema::AgentActivityNode],
) -> Option<&crate::api::schema::AgentActivityNode> {
    nodes
        .iter()
        .enumerate()
        .max_by_key(|(index, node)| {
            let running = node.status == crate::api::schema::AgentActivityStatus::Running;
            let at = if running {
                node.started_at_ms
            } else {
                node.ended_at_ms.or(node.started_at_ms)
            };
            (running, at, *index)
        })
        .map(|(_, node)| node)
}

/// 摘要形态下发的 `truncated`：存储本身已截断，或整棵树在摘要里放不下（摘要只带
/// 最新一个节点）。
pub(crate) fn activity_summary_truncated(
    truncated: bool,
    nodes: &[crate::api::schema::AgentActivityNode],
) -> bool {
    truncated || nodes.len() > 1
}

/// 一个外部来源条目（不属于任何 pane）：`info.activity` 已截断，截断前的计数另存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAgentRecord {
    pub info: crate::api::schema::ExternalAgentInfo,
    pub running: u32,
    pub total: u32,
    pub truncated: bool,
}

/// 刷新一个 pane 的活动树所需的 agent 身份（交给后台适配器的入参，全部自有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentActivitySubject {
    /// 规范化的 agent 名（`"claude"`、`"codex"` 等），用于查找来源适配器。
    pub agent: String,
    pub session: Option<crate::agent_resume::AgentSessionRef>,
    pub cwd: Option<std::path::PathBuf>,
    /// 该 pane 最近一份 hint 文本（共享所有权：提交后台任务不复制大快照）。
    pub latest_hint: Option<std::sync::Arc<str>>,
}

/// agent 启动序号与活动树的存储：纯数据，随 `AppState` 走，无 PTY 可测。
///
/// 启动序号在 pane 首次获得 agent 身份时分配（进程内单调计数，不持久化、跨
/// server 不可比），agent 释放或 pane 关闭后作废，重新识别取新号；`0` 表示未知。
/// 活动树按 pane 存放，每个 agent 最多 [`MAX_AGENT_ACTIVITY_NODES`] 个节点；外部
/// 来源条目按 `(source, external_id)` 排序存放，整源替换。`hinted` 是钩子提示的
/// 收件箱：只记「哪个 pane 报过有变化」，刷新调度（限频、后台读取）在 server 侧
/// 取走后执行；`latest_hints` 另存每个 pane 最近一份 hint 文本（pi 的树整份装在
/// 里面），交给来源适配器读取。
#[derive(Debug, Default)]
pub struct AgentActivityStore {
    next_launch_seq: u64,
    launch_seqs: std::collections::HashMap<PaneId, u64>,
    activity: std::collections::HashMap<PaneId, AgentActivitySnapshot>,
    hinted: std::collections::HashSet<PaneId>,
    latest_hints: std::collections::HashMap<PaneId, std::sync::Arc<str>>,
    /// 各 pane 钩子随会话上报的转录路径（claude）及其上报序号，只给活动树定位会话文件
    /// 用：与会话 id 成对存放，用时核对（[`Self::transcript`]）。上报可能早于 agent 被
    /// 识别，所以 pane 还在就留着，不随 [`Self::retain_panes`] 清理。
    transcripts:
        std::collections::HashMap<PaneId, (Option<u64>, crate::agent_resume::ReportedTranscript)>,
    external: Vec<ExternalAgentRecord>,
    /// 各外部来源最近一次成功刷新的时刻。
    external_refreshed_at: std::collections::HashMap<String, std::time::Instant>,
}

impl AgentActivityStore {
    /// 该 pane 的启动序号；未分配为 `0`。
    pub fn launch_seq(&self, pane_id: PaneId) -> u64 {
        self.launch_seqs.get(&pane_id).copied().unwrap_or(0)
    }

    /// pane 首次获得 agent 身份时分配序号；已有序号保持不变。返回是否新分配。
    pub fn ensure_launch_seq(&mut self, pane_id: PaneId) -> bool {
        if self.launch_seqs.contains_key(&pane_id) {
            return false;
        }
        self.next_launch_seq = self.next_launch_seq.saturating_add(1);
        self.launch_seqs.insert(pane_id, self.next_launch_seq);
        true
    }

    /// agent 释放或 pane 关闭：作废该 pane 的序号、活动树与未处理的提示。返回
    /// 投影可见的内容（序号或活动树）是否有被移除。
    pub fn forget_pane(&mut self, pane_id: PaneId) -> bool {
        self.hinted.remove(&pane_id);
        self.latest_hints.remove(&pane_id);
        let seq = self.launch_seqs.remove(&pane_id).is_some();
        let activity = self.activity.remove(&pane_id).is_some();
        seq || activity
    }

    /// 只保留 `alive` 判定为真的 pane 的记录（过期清理）。返回投影可见的内容是否
    /// 有被移除。
    pub fn retain_panes(&mut self, mut alive: impl FnMut(PaneId) -> bool) -> bool {
        let before = self.launch_seqs.len() + self.activity.len();
        self.launch_seqs.retain(|pane_id, _| alive(*pane_id));
        self.activity.retain(|pane_id, _| alive(*pane_id));
        self.hinted.retain(|pane_id| alive(*pane_id));
        self.latest_hints.retain(|pane_id, _| alive(*pane_id));
        before != self.launch_seqs.len() + self.activity.len()
    }

    /// 该 pane 当前的活动树快照。
    pub fn activity(&self, pane_id: PaneId) -> Option<&AgentActivitySnapshot> {
        self.activity.get(&pane_id)
    }

    /// 是否有任何 pane 存着活动树（投影据此跳过逐 agent 查找）。
    pub fn has_activity(&self) -> bool {
        !self.activity.is_empty()
    }

    /// 写入一次发现结果（截断到上限）。内容变化返回新计数并说明客户端快照摘要是否
    /// 也变了；空结果写到没有记录的 pane 不算变化（轮询空树不刷事件）。无论是否
    /// 变化都记下刷新时刻。
    pub fn apply_activity(
        &mut self,
        pane_id: PaneId,
        nodes: Vec<crate::api::schema::AgentActivityNode>,
        now: std::time::Instant,
    ) -> Option<AgentActivityApplied> {
        let truncated = truncate_activity_nodes(nodes);
        let counts = AgentActivityCounts {
            running: truncated.running,
            total: truncated.total,
        };
        match self.activity.get_mut(&pane_id) {
            Some(existing) => {
                existing.refreshed_at = now;
                let unchanged = existing.nodes == truncated.nodes
                    && existing.running == truncated.running
                    && existing.total == truncated.total
                    && existing.truncated == truncated.truncated;
                if unchanged {
                    return None;
                }
                // 摘要只带计数、截断标记与最新一个节点：深层节点（结束时间、摘要
                // 文本等）变化不进投影，不必让每个客户端整份重建快照。
                let summary_changed = existing.running != truncated.running
                    || existing.total != truncated.total
                    || activity_summary_truncated(existing.truncated, &existing.nodes)
                        != activity_summary_truncated(truncated.truncated, &truncated.nodes)
                    || latest_activity_node(&existing.nodes)
                        != latest_activity_node(&truncated.nodes);
                existing.nodes = truncated.nodes;
                existing.running = truncated.running;
                existing.total = truncated.total;
                existing.truncated = truncated.truncated;
                existing.revision = existing.revision.saturating_add(1);
                Some(AgentActivityApplied {
                    counts,
                    summary_changed,
                })
            }
            None => {
                if truncated.nodes.is_empty() {
                    return None;
                }
                self.activity.insert(
                    pane_id,
                    AgentActivitySnapshot {
                        nodes: truncated.nodes,
                        running: truncated.running,
                        total: truncated.total,
                        truncated: truncated.truncated,
                        refreshed_at: now,
                        revision: 1,
                    },
                );
                Some(AgentActivityApplied {
                    counts,
                    summary_changed: true,
                })
            }
        }
    }

    /// 记一次钩子提示。返回是否是新提示（同一 pane 未取走前重复提示只算一次）。
    pub fn note_hint(&mut self, pane_id: PaneId) -> bool {
        self.hinted.insert(pane_id)
    }

    /// 缓存该 pane 最近一份 hint 文本（后来者覆盖）。超过
    /// [`MAX_AGENT_ACTIVITY_HINT_BYTES`] 的提示不缓存（上一份保留），返回 false。
    pub fn store_hint(&mut self, pane_id: PaneId, hint: &str) -> bool {
        if hint.len() > MAX_AGENT_ACTIVITY_HINT_BYTES {
            return false;
        }
        self.latest_hints.insert(pane_id, hint.into());
        true
    }

    /// 该 pane 最近一份 hint 文本；没报过或已作废为 `None`。
    pub fn latest_hint(&self, pane_id: PaneId) -> Option<std::sync::Arc<str>> {
        self.latest_hints.get(&pane_id).cloned()
    }

    /// 记下该 pane 钩子随会话上报的转录路径。按上报序号取舍，与会话状态机同口径
    /// （`TerminalState::accept_hook_report`）：带序号的只收比已记下的更大的，不带序号
    /// 的只在还没记下过带序号的上报时收——迟到的旧会话上报不能盖掉当前会话的路径。
    /// 返回是否收下。
    pub fn note_transcript(
        &mut self,
        pane_id: PaneId,
        seq: Option<u64>,
        transcript: crate::agent_resume::ReportedTranscript,
    ) -> bool {
        let last = self.transcripts.get(&pane_id).and_then(|(seq, _)| *seq);
        let newer = match (seq, last) {
            (Some(seq), Some(last)) => seq > last,
            (None, Some(_)) => false,
            (_, None) => true,
        };
        if newer {
            self.transcripts.insert(pane_id, (seq, transcript));
        }
        newer
    }

    /// 该 pane 上报过的、属于会话 `session_id` 的转录路径；会话已换（id 对不上）或
    /// agent 不是 claude（只有它的钩子上报转录路径、适配器按转录布局解读）时为 `None`。
    pub fn transcript(
        &self,
        pane_id: PaneId,
        agent: &str,
        session_id: &str,
    ) -> Option<&crate::agent_resume::AgentSessionRef> {
        if agent != "claude" {
            return None;
        }
        self.transcripts
            .get(&pane_id)
            .map(|(_, transcript)| transcript)
            .filter(|transcript| transcript.session_id == session_id)
            .map(|transcript| &transcript.path)
    }

    /// 只保留 `exists` 判定为真的 pane 的转录路径（pane 关闭后清理）。
    pub fn retain_transcripts(&mut self, mut exists: impl FnMut(PaneId) -> bool) {
        self.transcripts.retain(|pane_id, _| exists(*pane_id));
    }

    pub fn has_hints(&self) -> bool {
        !self.hinted.is_empty()
    }

    /// 把全部未处理的提示交给刷新调度并清空收件箱。
    pub fn drain_hints(&mut self, mut deliver: impl FnMut(PaneId)) {
        for pane_id in self.hinted.drain() {
            deliver(pane_id);
        }
    }

    /// 当前外部来源条目，按 `(source, external_id)` 排序。
    pub fn external(&self) -> &[ExternalAgentRecord] {
        &self.external
    }

    /// 整源替换 `source` 的外部条目（其他来源保留）；每条的活动节点同样截断。
    /// 返回条目集合是否变化。无论是否变化都记下该来源的成功刷新时刻。
    pub fn apply_external(
        &mut self,
        source: &str,
        agents: Vec<crate::api::schema::ExternalAgentInfo>,
        now: std::time::Instant,
    ) -> bool {
        match self.external_refreshed_at.get_mut(source) {
            Some(at) => *at = now,
            None => {
                self.external_refreshed_at.insert(source.to_owned(), now);
            }
        }
        let mut next = self
            .external
            .iter()
            .filter(|record| record.info.source != source)
            .cloned()
            .chain(agents.into_iter().map(|mut info| {
                info.source = source.to_owned();
                let truncated = truncate_activity_nodes(std::mem::take(&mut info.activity));
                info.activity = truncated.nodes;
                ExternalAgentRecord {
                    info,
                    running: truncated.running,
                    total: truncated.total,
                    truncated: truncated.truncated,
                }
            }))
            .collect::<Vec<_>>();
        next.sort_by(|left, right| {
            left.info
                .source
                .cmp(&right.info.source)
                .then_with(|| left.info.external_id.cmp(&right.info.external_id))
        });
        if next == self.external {
            return false;
        }
        self.external = next;
        true
    }

    /// 过期：超过 `stale_after` 没有成功刷新的来源，其条目标为暂不可读。返回是否
    /// 有条目的状态变化。
    pub fn expire_external(
        &mut self,
        now: std::time::Instant,
        stale_after: std::time::Duration,
    ) -> bool {
        let mut changed = false;
        for record in &mut self.external {
            if !record.info.readable {
                continue;
            }
            let stale = self
                .external_refreshed_at
                .get(&record.info.source)
                .is_none_or(|at| now.saturating_duration_since(*at) >= stale_after);
            if stale {
                record.info.readable = false;
                changed = true;
            }
        }
        changed
    }
}

/// [`truncate_activity_nodes`] 的结果。
struct TruncatedActivity {
    nodes: Vec<crate::api::schema::AgentActivityNode>,
    running: u32,
    total: u32,
    truncated: bool,
}

/// 把一次发现结果截断到 [`MAX_AGENT_ACTIVITY_NODES`]。未超限时原样返回；超限时
/// 先保留运行中的节点连同其祖先链（整条链放不下就跳过该节点，避免孤儿），再按
/// 来源顺序补入父节点已保留（或无父节点）的其余节点；输出保持来源顺序。
fn truncate_activity_nodes(nodes: Vec<crate::api::schema::AgentActivityNode>) -> TruncatedActivity {
    use crate::api::schema::AgentActivityStatus;
    let total = nodes.len();
    let running = nodes
        .iter()
        .filter(|node| node.status == AgentActivityStatus::Running)
        .count();
    let running = u32::try_from(running).unwrap_or(u32::MAX);
    let total_count = u32::try_from(total).unwrap_or(u32::MAX);
    if total <= MAX_AGENT_ACTIVITY_NODES {
        return TruncatedActivity {
            nodes,
            running,
            total: total_count,
            truncated: false,
        };
    }

    let index_by_id = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let parent_index = |index: usize| -> Option<usize> {
        nodes[index]
            .parent_id
            .as_deref()
            .and_then(|parent| index_by_id.get(parent).copied())
            .filter(|parent| *parent != index)
    };
    let mut keep = vec![false; total];
    let mut kept = 0usize;
    let mut chain = Vec::new();
    for index in 0..total {
        if kept >= MAX_AGENT_ACTIVITY_NODES {
            break;
        }
        if keep[index] || nodes[index].status != AgentActivityStatus::Running {
            continue;
        }
        chain.clear();
        let mut cursor = Some(index);
        while let Some(current) = cursor {
            if keep[current] || chain.contains(&current) {
                break;
            }
            chain.push(current);
            cursor = parent_index(current);
        }
        if kept + chain.len() <= MAX_AGENT_ACTIVITY_NODES {
            for &member in &chain {
                keep[member] = true;
            }
            kept += chain.len();
        }
    }
    for index in 0..total {
        if kept >= MAX_AGENT_ACTIVITY_NODES {
            break;
        }
        if keep[index] {
            continue;
        }
        let attached = nodes[index].parent_id.is_none()
            || parent_index(index).is_none_or(|parent| keep[parent]);
        if attached {
            keep[index] = true;
            kept += 1;
        }
    }
    drop(index_by_id);
    let nodes = nodes
        .into_iter()
        .zip(keep)
        .filter_map(|(node, keep)| keep.then_some(node))
        .collect();
    TruncatedActivity {
        nodes,
        running,
        total: total_count,
        truncated: true,
    }
}

/// pane 是否仍持有 agent（存在且其终端是 agent 终端）。
fn pane_hosts_agent(
    workspaces: &[Workspace],
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    pane_id: PaneId,
) -> bool {
    workspaces
        .iter()
        .find_map(|ws| ws.pane_state(pane_id))
        .and_then(|pane| terminals.get(&pane.attached_terminal_id))
        .is_some_and(crate::terminal::TerminalState::is_agent_terminal)
}

/// 活动来源适配器用的规范化 agent 名：已知 agent 取规范名，否则取原始标签。
fn activity_agent_key(terminal: &crate::terminal::TerminalState) -> Option<&str> {
    terminal
        .effective_known_agent()
        .map(crate::detect::agent_label)
        .or_else(|| terminal.effective_agent_label())
}

/// All application state — pure data, no channels or async runtime.
/// Testable without PTYs or a tokio runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabBarStatusSegment {
    Zoom,
    Text(Option<String>),
}

pub struct AppState {
    pub terminals:
        std::collections::HashMap<crate::terminal::TerminalId, crate::terminal::TerminalState>,
    /// Terminal ids whose size is currently owned by a direct attach client.
    pub direct_attach_resize_locks: std::collections::HashSet<crate::terminal::TerminalId>,
    pub(crate) pane_id_aliases: std::collections::HashMap<u32, PaneId>,
    pub(crate) public_pane_id_aliases: std::collections::HashMap<String, PaneId>,
    pub workspaces: Vec<Workspace>,
    pub active: Option<usize>,
    pub(crate) previous_pane_focus: Option<PaneFocusTarget>,
    pub selected: usize,
    pub mode: Mode,
    pub should_quit: bool,
    /// Set when the headless server should ask attached clients to reload
    /// their client-local sound config from disk.
    pub request_client_config_reload: bool,
    pub worktree_directory: std::path::PathBuf,
    /// Latest endpoint-owned release notes, cached outside render paths.
    pub latest_release_notes: Option<crate::release_notes::ReleaseNotes>,
    pub product_announcement: Option<ProductAnnouncementState>,
    // Geometry of the most recently computed server pane surface.
    pub view: ViewState,
    // Notifications
    pub update_available: Option<String>,
    pub update_install_command: String,
    pub latest_release_notes_available: bool,
    pub update_dismissed: bool,
    pub config_diagnostic: Option<String>,
    pub toast: Option<ToastNotification>,
    pub pending_agent_notifications: std::collections::HashMap<PaneId, PendingAgentNotification>,
    /// Last reported focus state for the outer terminal hosting herdr.
    /// None means unsupported or not yet reported, which preserves active-pane suppression.
    pub outer_terminal_focus: Option<bool>,
    // Config
    pub prefix_code: KeyCode,
    pub prefix_mods: KeyModifiers,
    /// Virtual terminal size (columns, rows) used when no client is attached.
    pub(crate) headless_size: (u16, u16),
    pub agent_panel_sort: AgentPanelSort,
    /// Transient session-wide projection override for the built-in Agents view.
    pub agent_view_override: Option<crate::api::schema::AgentViewSetParams>,
    pub sidebar_agents: crate::config::AgentsSidebarConfig,
    pub sidebar_spaces: crate::config::SpacesSidebarConfig,
    pub next_agent_state_change_seq: u64,
    /// agent 启动序号与活动树（`AgentInfo.launch_seq` / `.activity` 与投影的来源）。
    pub agent_activity: AgentActivityStore,
    pub confirm_close: bool,
    pub pane_borders: crate::config::PaneBordersConfig,
    pub pane_outer_borders: bool,
    pub pane_scrollbars: bool,
    pub pane_gaps: bool,
    /// Pane border glyph table resolved from `ui.border_style`.
    pub border_glyphs: crate::ui::BorderGlyphs,
    /// Effective output color depth for Herdr's own UI colors;
    /// `ui.color_depth = "auto"` is already resolved against the host here.
    pub host_color_depth: crate::config::ColorDepth,
    pub show_agent_labels_on_pane_borders: bool,
    pub tab_bar_right: Vec<TabBarStatusSegment>,
    pub tab_bar_right_separator: String,
    /// Expose the focused pane's cursor anchor to the outer terminal even when
    /// the pane requested `?25l`. See `[experimental] reveal_hidden_cursor_for_cjk_ime`.
    pub reveal_hidden_cursor_for_cjk_ime: bool,
    /// Restrict cursor reveal to focused panes whose detected agent matches
    /// one of these. When false, apply to any focused pane.
    pub cjk_ime_agent_filter_configured: bool,
    pub cjk_ime_agents: Vec<crate::detect::Agent>,
    /// DECSCUSR shape parameter (1–6) for the IME anchor cursor.
    pub cjk_ime_cursor_shape: u8,
    pub kitty_graphics_enabled: bool,
    pub default_shell: String,
    pub shell_mode: crate::config::ShellModeConfig,
    pub new_terminal_cwd: NewTerminalCwdConfig,
    pub pane_scrollback_limit_bytes: usize,
    pub sound: SoundConfig,
    pub toast_config: ToastConfig,
    pub keybinds: Keybinds,
    /// UI color palette — all sidebar/UI colors centralized for theming.
    pub palette: Palette,
    /// Component-level styles resolved from `[theme.components]` on top of
    /// `palette`; recomputed together with the palette, read by renderers.
    pub components: ComponentStyles,
    /// Currently applied theme name (for settings UI).
    pub theme_name: String,
    /// Runtime theme configuration used to resolve manual and auto-switch palettes.
    pub theme_runtime: ThemeRuntimeConfig,
    /// Last known foreground host terminal appearance.
    pub host_terminal_appearance: Option<HostAppearance>,
    /// True when the foreground host explicitly reported appearance via Mode 2031.
    pub host_terminal_appearance_explicit: bool,
    /// Cached integration recommendations and detection manifest summaries.
    pub integration_recommendations: Vec<crate::integration::IntegrationRecommendation>,
    pub agent_manifest_summaries: Vec<crate::detect::manifest::AgentManifestSummary>,
    /// Cached remote detection manifest update diagnostics for runtime/API status.
    pub agent_manifest_update_status: crate::detect::manifest_update::ManifestUpdateStatus,
    /// Installed or linked plugins known to this running Herdr instance.
    pub(crate) installed_plugins: InstalledPluginRegistry,
    /// Pane ids opened through the plugin pane API.
    pub(crate) plugin_panes: std::collections::HashMap<PaneId, PluginPaneRecord>,
    /// Session-modal terminal popup. This is intentionally outside workspace layouts.
    pub(crate) popup_pane: Option<PopupPaneState>,
    /// Recent plugin action/event command executions.
    pub(crate) plugin_command_logs: Vec<crate::api::schema::PluginCommandLogInfo>,
    pub(crate) next_plugin_command_log_id: u64,
    pub(crate) plugin_commands_in_flight: usize,
    /// Resolved host terminal default colors for theming embedded panes.
    pub host_terminal_theme: TerminalTheme,
    /// Last known foreground host terminal cell size in pixels.
    pub(crate) host_cell_size: crate::kitty_graphics::HostCellSize,
    /// Set when a persisted session snapshot would change.
    pub session_dirty: bool,
    /// 仅当「工作区集合为空」是用户/API 显式关闭最后一个工作区的结果时置位。
    /// 它是清空持久化会话的唯一许可：主机重启同样会因为所有 pane 退出让集合
    /// 归零，那份快照必须留住（HSR-04 / 上游 #4320）。
    pub(crate) explicit_session_teardown: bool,
    /// Terminal runtimes that should be shut down by the app/runtime layer
    /// after state has detached their terminal metadata.
    pub(crate) terminal_runtime_shutdowns: Vec<crate::terminal::TerminalId>,
    /// 投影纪元：任何会进入 ClientShell 投影的内容变更都必须在写入点递增它
    /// （HSR-05/APP-002）。render 循环用它跳过无变化的候选重建；递增遗漏会
    /// 导致客户端投影陈旧，写入点清单由 projection_epoch 测试守门。
    pub(crate) projection_epoch: u64,
}

impl AppState {
    pub(crate) fn bump_projection_epoch(&mut self) {
        self.projection_epoch = self.projection_epoch.wrapping_add(1);
    }

    /// 后台发现结果落库。pane 已不持有 agent（迟到的结果）则丢弃并清记录；内容
    /// 变化返回新计数。
    ///
    /// 只有随快照下发的活动摘要变化时才递增投影纪元（HSR-05 写入点）。纪元是每
    /// 客户端投影复用的键，而生产默认下发形态是摘要
    /// （`server::client_shell::SNAPSHOT_ACTIVITY`，护栏测试
    /// `snapshot_activity_defaults_to_summary_per_agent` 钉住）：按「任意节点变化即
    /// 递增」会让深层节点的每一次变动都触发每个挂载客户端整份重建快照再深比较，
    /// 产出往往逐字节相同。整棵树照常落库，供 `agent.activity.read` / `agent.get`
    /// 读取；`pane.agent_activity_changed` 仍按内容变化发。
    pub(crate) fn apply_agent_activity(
        &mut self,
        pane_id: PaneId,
        nodes: Vec<crate::api::schema::AgentActivityNode>,
        now: std::time::Instant,
    ) -> Option<AgentActivityApplied> {
        if !pane_hosts_agent(&self.workspaces, &self.terminals, pane_id) {
            if self.agent_activity.forget_pane(pane_id) {
                self.bump_projection_epoch();
            }
            return None;
        }
        let applied = self.agent_activity.apply_activity(pane_id, nodes, now)?;
        if applied.summary_changed {
            self.bump_projection_epoch();
        }
        Some(applied)
    }

    /// 外部来源条目落库；集合变化时递增投影纪元。返回是否变化。
    pub(crate) fn apply_external_agents(
        &mut self,
        source: &str,
        agents: Vec<crate::api::schema::ExternalAgentInfo>,
        now: std::time::Instant,
    ) -> bool {
        let changed = self.agent_activity.apply_external(source, agents, now);
        if changed {
            self.bump_projection_epoch();
        }
        changed
    }

    /// 过期清理：清掉已不持有 agent（或已关闭）的 pane 的启动序号与活动树，并把
    /// 超过 [`EXTERNAL_AGENT_STALE_AFTER`] 没有成功刷新的外部条目标为暂不可读。
    /// 返回是否有变化（已递增投影纪元）。
    pub(crate) fn expire_agent_activity(&mut self, now: std::time::Instant) -> bool {
        let AppState {
            agent_activity,
            workspaces,
            terminals,
            ..
        } = self;
        let panes_changed =
            agent_activity.retain_panes(|pane_id| pane_hosts_agent(workspaces, terminals, pane_id));
        // 转录路径不进投影：只清掉已关闭 pane 的，不递增投影纪元。
        agent_activity.retain_transcripts(|pane_id| {
            workspaces
                .iter()
                .any(|workspace| workspace.pane_state(pane_id).is_some())
        });
        let external_changed = agent_activity.expire_external(now, EXTERNAL_AGENT_STALE_AFTER);
        let changed = panes_changed || external_changed;
        if changed {
            self.bump_projection_epoch();
        }
        changed
    }

    /// 逐个访问持有 agent 的 pane：`(pane_id, 规范化 agent 名, 是否 Working)`。
    /// 刷新调度每秒最多走一遍；回调内不分配。
    pub(crate) fn for_each_agent_pane(&self, mut visit: impl FnMut(PaneId, &str, bool)) {
        for workspace in &self.workspaces {
            for tab in &workspace.tabs {
                for (pane_id, pane) in &tab.panes {
                    let Some(terminal) = self.terminals.get(&pane.attached_terminal_id) else {
                        continue;
                    };
                    if !terminal.is_agent_terminal() {
                        continue;
                    }
                    let Some(agent) = activity_agent_key(terminal) else {
                        continue;
                    };
                    visit(*pane_id, agent, terminal.state == AgentState::Working);
                }
            }
        }
    }

    /// 刷新该 pane 活动树所需的 agent 身份；pane 不存在或不持有 agent 时为 `None`。
    pub(crate) fn agent_activity_subject(&self, pane_id: PaneId) -> Option<AgentActivitySubject> {
        let pane = self
            .workspaces
            .iter()
            .find_map(|workspace| workspace.pane_state(pane_id))?;
        let terminal = self.terminals.get(&pane.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let agent = activity_agent_key(terminal)?.to_owned();
        let session = terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.clone())
            .or_else(|| {
                terminal
                    .persisted_agent_session
                    .as_ref()
                    .map(|session| session.session_ref.clone())
            })
            .map(|session| {
                // 钩子随这个会话上报过转录路径（claude）：按路径定位会话文件，pane 里单独
                // 设置的配置目录也能跟上（路径是 CLI 自己给的）；会话已换时不用旧路径。
                match session.kind {
                    crate::agent_resume::AgentSessionRefKind::Id => self
                        .agent_activity
                        .transcript(pane_id, &agent, &session.value)
                        .cloned()
                        .unwrap_or(session),
                    crate::agent_resume::AgentSessionRefKind::Path => session,
                }
            });
        let cwd = (!terminal.cwd.as_os_str().is_empty()).then(|| terminal.cwd.clone());
        Some(AgentActivitySubject {
            agent,
            session,
            cwd,
            latest_hint: self.agent_activity.latest_hint(pane_id),
        })
    }

    pub(crate) fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
        self.bump_projection_epoch();
    }

    /// 记录一次显式（用户/API）工作区关闭的结果。
    ///
    /// 只有它会置位 `explicit_session_teardown`，而且仅在关完后集合真的为空时；
    /// 集合还有工作区就顺带复位，避免陈旧标记留到下一次归零。隐式归零（pane
    /// 批量退出、主机重启）走 `handle_pane_died`，那里必须复位。
    pub(crate) fn note_explicit_workspace_teardown(&mut self) {
        self.explicit_session_teardown = self.workspaces.is_empty();
    }

    pub(crate) fn remove_alias_shadowed_by_new_pane(&mut self, pane_id: PaneId) {
        self.pane_id_aliases.remove(&pane_id.raw());
    }

    pub(crate) fn refresh_agent_manifest_summaries(&mut self) {
        self.agent_manifest_summaries = crate::detect::manifest::manifest_summaries();
    }

    pub(crate) fn integration_updates_available(&self) -> bool {
        self.integration_recommendations
            .iter()
            .any(|recommendation| {
                recommendation.state == crate::integration::IntegrationStatusKind::Outdated
                    && recommendation.needs_install()
            })
    }

    pub fn estimate_pane_size(&self) -> (u16, u16) {
        if let Some(info) = self.view.pane_infos.first() {
            (info.rect.height, info.rect.width)
        } else {
            (self.headless_size.1, self.headless_size.0)
        }
    }

    /// Returns true when the given (workspace, tab, pane) refers to the
    /// currently focused pane in the active workspace's active tab.
    pub(crate) fn runtime_for_pane_in_workspace<'a>(
        &'a self,
        terminal_runtimes: &'a crate::terminal::TerminalRuntimeRegistry,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<&'a crate::terminal::TerminalRuntime> {
        #[cfg(test)]
        if let Some(runtime) = self.workspaces.get(ws_idx)?.test_runtimes.get(&pane_id) {
            return Some(runtime);
        }
        #[cfg(test)]
        if let Some(runtime) = self
            .workspaces
            .get(ws_idx)?
            .tabs
            .iter()
            .find_map(|tab| tab.runtimes.get(&pane_id))
        {
            return Some(runtime);
        }
        let terminal_id = self.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        terminal_runtimes.get(terminal_id)
    }

    pub(crate) fn pane_visible_on_active_surface(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> bool {
        if self.active != Some(ws_idx) {
            return false;
        }
        let Some(tab) = self
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.active_tab())
        else {
            return false;
        };
        if tab.zoomed {
            tab.layout.focused() == pane_id
        } else {
            tab.layout.pane_ids().contains(&pane_id)
        }
    }

    pub fn is_active_pane(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> bool {
        let Some(active_ws_idx) = self.active else {
            return false;
        };
        if ws_idx != active_ws_idx {
            return false;
        }
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        if tab_idx != ws.active_tab_index() {
            return false;
        }
        ws.active_tab().map(|tab| tab.layout.focused()) == Some(pane_id)
    }
}

#[cfg(test)]
pub fn key_matches(
    key: &crossterm::event::KeyEvent,
    expected_code: KeyCode,
    expected_mods: KeyModifiers,
) -> bool {
    crate::config::terminal_key_matches_combo(
        &crate::input::TerminalKey::from(*key),
        (expected_code, expected_mods),
    )
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
impl AppState {
    /// Create an AppState for testing — no channels, no PTYs.
    pub fn test_new() -> Self {
        Self {
            terminals: std::collections::HashMap::new(),
            direct_attach_resize_locks: std::collections::HashSet::new(),
            pane_id_aliases: std::collections::HashMap::new(),
            public_pane_id_aliases: std::collections::HashMap::new(),
            workspaces: Vec::new(),
            active: None,
            previous_pane_focus: None,
            selected: 0,
            mode: Mode::Navigate,
            should_quit: false,
            request_client_config_reload: false,
            worktree_directory: std::path::PathBuf::from("/tmp/herdr-worktrees"),
            latest_release_notes: None,
            product_announcement: None,
            view: ViewState {
                terminal_area: Rect::default(),
                pane_infos: Vec::new(),
            },
            update_available: None,
            update_install_command: "herdr update".into(),
            latest_release_notes_available: false,
            update_dismissed: false,
            config_diagnostic: None,
            toast: None,
            pending_agent_notifications: std::collections::HashMap::new(),
            outer_terminal_focus: None,
            prefix_code: KeyCode::Char('b'),
            prefix_mods: KeyModifiers::CONTROL,
            headless_size: (
                crate::config::DEFAULT_HEADLESS_COLS,
                crate::config::DEFAULT_HEADLESS_ROWS,
            ),
            agent_panel_sort: AgentPanelSort::Spaces,
            agent_view_override: None,
            sidebar_agents: crate::config::AgentsSidebarConfig::default(),
            sidebar_spaces: crate::config::SpacesSidebarConfig::default(),
            next_agent_state_change_seq: 0,
            agent_activity: AgentActivityStore::default(),
            confirm_close: true,
            pane_borders: crate::config::PaneBordersConfig::Auto,
            pane_outer_borders: true,
            pane_scrollbars: true,
            pane_gaps: false,
            border_glyphs: crate::ui::BorderGlyphs::SINGLE,
            host_color_depth: crate::config::ColorDepth::Truecolor,
            show_agent_labels_on_pane_borders: false,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: " ".into(),
            reveal_hidden_cursor_for_cjk_ime: true,
            cjk_ime_agent_filter_configured: false,
            cjk_ime_agents: Vec::new(),
            cjk_ime_cursor_shape: 5, // bar
            kitty_graphics_enabled: false,
            default_shell: String::new(),
            shell_mode: crate::config::ShellModeConfig::Auto,
            new_terminal_cwd: NewTerminalCwdConfig::Follow,
            pane_scrollback_limit_bytes: crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES,
            sound: SoundConfig {
                enabled: false,
                ..SoundConfig::default()
            },
            toast_config: ToastConfig::default(),
            keybinds: Keybinds::default(),
            palette: Palette::catppuccin(),
            components: ComponentStyles::from_palette(&Palette::catppuccin()),
            theme_name: "catppuccin".to_string(),
            theme_runtime: ThemeRuntimeConfig {
                manual_name: "catppuccin".to_string(),
                dark_name: "catppuccin".to_string(),
                light_name: "catppuccin-latte".to_string(),
                auto_switch: false,
                custom: None,
                legacy_accent: None,
                components: None,
                color_depth: crate::config::ColorDepthConfig::default(),
            },
            host_terminal_appearance: None,
            host_terminal_appearance_explicit: false,
            integration_recommendations: Vec::new(),
            agent_manifest_summaries: Vec::new(),
            agent_manifest_update_status:
                crate::detect::manifest_update::ManifestUpdateStatus::default(),
            installed_plugins: std::collections::HashMap::new(),
            plugin_panes: std::collections::HashMap::new(),
            popup_pane: None,
            plugin_command_logs: Vec::new(),
            next_plugin_command_log_id: 1,
            plugin_commands_in_flight: 0,
            host_terminal_theme: TerminalTheme::default(),
            host_cell_size: crate::kitty_graphics::HostCellSize::default(),
            session_dirty: false,
            explicit_session_teardown: false,
            terminal_runtime_shutdowns: Vec::new(),
            projection_epoch: 0,
        }
    }

    /// Populate missing `TerminalState` entries for every pane so tests that
    /// read or write terminal metadata don't need to manually create them.
    pub fn ensure_test_terminals(&mut self) {
        use crate::terminal::TerminalState;
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                for pane in tab.panes.values() {
                    if !self.terminals.contains_key(&pane.attached_terminal_id) {
                        let cwd = ws.identity_cwd.clone();
                        self.terminals.insert(
                            pane.attached_terminal_id.clone(),
                            TerminalState::new(pane.attached_terminal_id.clone(), cwd),
                        );
                    }
                }
            }
        }
    }

    pub fn test_with_adversarial_identity_state() -> Self {
        let mut state = Self::test_new();
        state.workspaces = vec![crate::workspace::Workspace::test_adversarial_identity_state()];
        state.active = Some(0);
        state.selected = 0;
        state.ensure_test_terminals();
        state
    }

    pub fn assert_invariants_for_test(&self) {
        if self.workspaces.is_empty() {
            assert!(
                self.active.is_none(),
                "empty app state must not have active workspace {:?}",
                self.active
            );
            assert_eq!(
                self.selected, 0,
                "empty app state should keep selected workspace at 0"
            );
            assert!(
                self.pane_id_aliases.is_empty(),
                "empty app state must not keep raw pane aliases"
            );
            assert!(
                self.public_pane_id_aliases.is_empty(),
                "empty app state must not keep public pane aliases"
            );
            assert!(
                self.previous_pane_focus.is_none(),
                "empty app state must not keep previous pane focus"
            );
            assert!(
                self.plugin_panes.is_empty(),
                "empty app state must not keep plugin pane records"
            );
            assert!(
                self.pending_agent_notifications.is_empty(),
                "empty app state must not keep pending agent notifications"
            );
            if let Some(toast) = &self.toast {
                assert!(
                    toast.target.is_none(),
                    "empty app state must not keep pane-targeted toast"
                );
            }
            return;
        }

        assert!(
            self.selected < self.workspaces.len(),
            "selected workspace {} out of bounds for {} workspaces",
            self.selected,
            self.workspaces.len()
        );
        let active = self
            .active
            .expect("non-empty app state must have active workspace");
        assert!(
            active < self.workspaces.len(),
            "active workspace {} out of bounds for {} workspaces",
            active,
            self.workspaces.len()
        );

        let mut workspace_ids = std::collections::HashSet::new();
        let mut workspace_id_to_idx = std::collections::HashMap::new();
        let mut pane_ids = std::collections::HashSet::new();
        let mut attached_terminal_ids = std::collections::HashSet::new();
        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            assert!(
                workspace_ids.insert(ws.id.clone()),
                "duplicate workspace id {} at workspace index {}",
                ws.id,
                ws_idx
            );
            workspace_id_to_idx.insert(ws.id.clone(), ws_idx);
            ws.assert_invariants_for_test();

            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
                    assert!(
                        pane_ids.insert(*pane_id),
                        "pane {:?} appears in more than one workspace",
                        pane_id
                    );
                    assert!(
                        attached_terminal_ids.insert(pane.attached_terminal_id.clone()),
                        "terminal {} is attached to more than one app pane",
                        pane.attached_terminal_id
                    );
                    assert!(
                        self.terminals.contains_key(&pane.attached_terminal_id),
                        "pane {:?} is attached to missing terminal {}",
                        pane_id,
                        pane.attached_terminal_id
                    );
                }
            }
        }

        let assert_live_pane = |pane_id: PaneId, context: &str| {
            assert!(
                pane_ids.contains(&pane_id),
                "{context} references missing pane {:?}",
                pane_id
            );
        };
        let assert_workspace_pane = |workspace_id: &str, pane_id: PaneId, context: &str| {
            let ws_idx = workspace_id_to_idx
                .get(workspace_id)
                .copied()
                .unwrap_or_else(|| panic!("{context} references missing workspace {workspace_id}"));
            assert!(
                self.workspaces[ws_idx].pane_state(pane_id).is_some(),
                "{context} references pane {:?} outside workspace {}",
                pane_id,
                workspace_id
            );
        };
        for (&raw, &pane_id) in &self.pane_id_aliases {
            assert_live_pane(pane_id, &format!("raw pane alias {raw}"));
        }
        for (public_id, &pane_id) in &self.public_pane_id_aliases {
            assert_live_pane(pane_id, &format!("public pane alias {public_id}"));
        }
        if let Some(focus) = &self.previous_pane_focus {
            assert_workspace_pane(&focus.workspace_id, focus.pane_id, "previous pane focus");
        }
        if let Some(toast) = &self.toast {
            if let Some(target) = &toast.target {
                assert_workspace_pane(&target.workspace_id, target.pane_id, "toast target");
            }
        }
        for (&pane_id, notification) in &self.pending_agent_notifications {
            assert_eq!(
                pane_id, notification.pane_id,
                "pending agent notification map key must match payload pane id"
            );
            assert_workspace_pane(
                &notification.workspace_id,
                notification.pane_id,
                "pending agent notification",
            );
        }
        if let Some(popup) = &self.popup_pane {
            assert!(
                self.terminals.contains_key(&popup.terminal_id),
                "popup {:?} references missing terminal {}",
                popup.pane_id,
                popup.terminal_id
            );
            assert!(
                !attached_terminal_ids.contains(&popup.terminal_id),
                "popup terminal {} must not be attached to a tiled pane",
                popup.terminal_id
            );
        }
        for &pane_id in self.plugin_panes.keys() {
            assert_live_pane(pane_id, "plugin pane record");
        }
    }

    pub fn insert_test_runtime(
        &mut self,
        pane_id: crate::layout::PaneId,
        runtime: crate::terminal::TerminalRuntime,
    ) {
        if let Some(ws) = self
            .workspaces
            .iter_mut()
            .find(|ws| ws.terminal_id(pane_id).is_some())
        {
            ws.insert_test_runtime(pane_id, runtime);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn pane_size_estimate_uses_headless_size_before_first_view() {
        let mut state = AppState::test_new();
        state.headless_size = (132, 41);

        assert_eq!(state.estimate_pane_size(), (41, 132));
    }

    #[test]
    fn adversarial_identity_state_satisfies_app_invariants_after_mutation() {
        let mut state = AppState::test_with_adversarial_identity_state();
        state.assert_invariants_for_test();

        let ws = &mut state.workspaces[0];
        let active_public = ws.tabs[ws.active_tab].number;
        assert_ne!(ws.active_tab + 1, active_public);
        let new_pane = ws.test_split(ratatui::layout::Direction::Horizontal);
        assert!(ws.public_pane_number(new_pane).is_some());
        state.ensure_test_terminals();

        state.assert_invariants_for_test();
    }

    fn rgb_luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("expected RGB color, got {color:?}");
        };
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast_ratio(a: Color, b: Color) -> f64 {
        let (lighter, darker) = {
            let a = rgb_luminance(a);
            let b = rgb_luminance(b);
            (a.max(b), a.min(b))
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn built_in_theme_names_resolve() {
        for name in crate::config::THEME_NAMES {
            assert!(
                Palette::from_name(name).is_some(),
                "theme should resolve: {name}"
            );
        }
    }

    #[test]
    fn built_in_active_rows_remain_visible_with_matching_terminal_backgrounds() {
        for name in crate::config::THEME_NAMES
            .iter()
            .copied()
            .filter(|name| *name != "terminal")
        {
            let palette = Palette::from_name(name).unwrap();
            let background_contrast = contrast_ratio(palette.panel_bg, palette.active_row_bg);
            assert!(
                background_contrast >= 1.05,
                "active row blends into the matching terminal background for {name}: {background_contrast:.2}:1"
            );

            let text_contrast = contrast_ratio(palette.text, palette.active_row_bg);
            assert!(
                text_contrast >= 3.0,
                "active row text loses contrast for {name}: {text_contrast:.2}:1"
            );
        }
    }

    #[test]
    fn built_in_selection_rows_stay_distinct_from_background_and_active_rows() {
        for name in crate::config::THEME_NAMES
            .iter()
            .copied()
            .filter(|name| *name != "terminal")
        {
            let palette = Palette::from_name(name).unwrap();
            let background_contrast = contrast_ratio(palette.panel_bg, palette.selection_bg);
            assert!(
                background_contrast >= 1.05,
                "selection row blends into the matching terminal background for {name}: {background_contrast:.2}:1"
            );

            let text_contrast = contrast_ratio(palette.text, palette.selection_bg);
            assert!(
                text_contrast >= 3.0,
                "selection row text loses contrast for {name}: {text_contrast:.2}:1"
            );
            assert_ne!(
                palette.selection_bg, palette.active_row_bg,
                "selection row shares the active row color for {name}"
            );
        }
    }

    #[test]
    fn built_in_themes_leave_sidebar_background_unset_except_the_default() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).unwrap();
            if *name == "catppuccin" {
                // The default theme paints a solid sidebar so hosts with a
                // wallpaper behind the terminal do not bleed it through.
                assert_eq!(palette.sidebar_bg, palette.panel_bg);
            } else {
                assert_eq!(
                    palette.sidebar_bg,
                    Color::Reset,
                    "built-in theme changed the sidebar background: {name}"
                );
            }
        }
    }

    #[test]
    fn custom_sidebar_colors_override_the_defaults() {
        let custom = crate::config::CustomThemeColors {
            sidebar_bg: Some("#181825".to_string()),
            active_row_bg: Some("#313244".to_string()),
            selection_bg: Some("#45475a".to_string()),
            ..Default::default()
        };
        let palette = Palette::catppuccin().with_overrides(&custom);

        assert_eq!(palette.sidebar_bg, Color::Rgb(24, 24, 37));
        assert_eq!(palette.active_row_bg, Color::Rgb(49, 50, 68));
        assert_eq!(palette.selection_bg, Color::Rgb(69, 71, 90));
    }

    #[test]
    fn light_theme_aliases_resolve() {
        for name in ["light", "latte", "tokyo-day", "onelight", "lotus", "dawn"] {
            assert!(
                Palette::from_name(name).is_some(),
                "theme should resolve: {name}"
            );
        }
    }

    #[test]
    fn key_matches_requires_exact_modifiers() {
        assert!(key_matches(
            &KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        ));

        assert!(!key_matches(
            &KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        ));
    }

    #[test]
    fn key_matches_letters_case_insensitively() {
        assert!(key_matches(
            &KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
            KeyCode::Char('b'),
            KeyModifiers::SHIFT,
        ));
    }

    #[test]
    fn component_styles_fall_back_to_semantic_palette_tokens() {
        let palette = Palette::catppuccin();
        let components =
            ComponentStyles::resolve(&palette, None, crate::config::ColorDepth::Truecolor);

        assert_eq!(components.pane_border_focused, palette.accent);
        assert_eq!(components.pane_border_unfocused, palette.overlay0);
        assert_eq!(components.scrollbar_thumb_focused, palette.overlay1);
        assert_eq!(components.scrollbar_thumb_unfocused, palette.overlay0);
        assert_eq!(components.scrollbar_track_focused, palette.overlay0);
        assert_eq!(components.scrollbar_track_unfocused, palette.surface_dim);
        assert_eq!(components.mode_bar_accent, palette.accent);
        assert_eq!(components.toast_border_success, palette.green);
        assert_eq!(components.toast_border_info, palette.blue);
        assert_eq!(components.toast_border_error, palette.red);
        assert_eq!(
            components.selection_mix_ratio,
            crate::config::DEFAULT_SELECTION_MIX_RATIO
        );
        assert_eq!(components.color_depth, crate::config::ColorDepth::Truecolor);
        assert_eq!(components, ComponentStyles::from_palette(&palette));
    }

    #[test]
    fn component_styles_apply_explicit_overrides() {
        let palette = Palette::catppuccin();
        let components = ComponentStyles::resolve(
            &palette,
            Some(&crate::config::ThemeComponentsConfig {
                pane_border_focused: Some("#010203".to_string()),
                scrollbar_thumb: Some("red".to_string()),
                scrollbar_track: Some("#040506".to_string()),
                toast_border_error: Some("#070809".to_string()),
                selection_mix_ratio: Some(0.5),
                ..Default::default()
            }),
            crate::config::ColorDepth::Truecolor,
        );

        assert_eq!(components.pane_border_focused, Color::Rgb(1, 2, 3));
        assert_eq!(components.pane_border_unfocused, palette.overlay0);
        // One scrollbar override covers both focus states.
        assert_eq!(components.scrollbar_thumb_focused, Color::Red);
        assert_eq!(components.scrollbar_thumb_unfocused, Color::Red);
        assert_eq!(components.scrollbar_track_focused, Color::Rgb(4, 5, 6));
        assert_eq!(components.scrollbar_track_unfocused, Color::Rgb(4, 5, 6));
        assert_eq!(components.toast_border_error, Color::Rgb(7, 8, 9));
        assert_eq!(components.toast_border_info, palette.blue);
        assert_eq!(components.selection_mix_ratio, 0.5);
    }

    #[test]
    fn component_styles_out_of_range_ratio_falls_back_to_default() {
        let palette = Palette::catppuccin();
        for ratio in [-0.5, 1.5, f32::NAN] {
            let components = ComponentStyles::resolve(
                &palette,
                Some(&crate::config::ThemeComponentsConfig {
                    selection_mix_ratio: Some(ratio),
                    ..Default::default()
                }),
                crate::config::ColorDepth::Truecolor,
            );
            assert_eq!(
                components.selection_mix_ratio,
                crate::config::DEFAULT_SELECTION_MIX_RATIO,
                "ratio: {ratio}"
            );
        }
    }

    #[test]
    fn palette_and_components_degrade_to_256_together() {
        let palette = Palette::catppuccin();
        let degraded = palette
            .clone()
            .with_color_depth(crate::config::ColorDepth::Color256);

        assert_eq!(
            degraded.accent,
            Color::Indexed(crate::config::rgb_to_xterm256(137, 180, 250))
        );
        // The default sidebar is solid and degrades like every RGB token.
        assert_eq!(
            degraded.sidebar_bg,
            Color::Indexed(crate::config::rgb_to_xterm256(24, 24, 37))
        );
        // Symbolic tokens still pass through unchanged on themes that keep
        // a transparent sidebar.
        assert_eq!(
            Palette::tokyo_night()
                .with_color_depth(crate::config::ColorDepth::Color256)
                .sidebar_bg,
            Color::Reset
        );
        // Degradation is idempotent.
        assert_eq!(
            degraded
                .clone()
                .with_color_depth(crate::config::ColorDepth::Color256),
            degraded
        );
        // Truecolor and auto-pass-through leave the palette untouched.
        assert_eq!(
            palette
                .clone()
                .with_color_depth(crate::config::ColorDepth::Truecolor),
            palette
        );

        let components = ComponentStyles::resolve(
            &degraded,
            Some(&crate::config::ThemeComponentsConfig {
                toast_border_success: Some("#a6e3a1".to_string()),
                ..Default::default()
            }),
            crate::config::ColorDepth::Color256,
        );
        // Fallbacks inherit the degraded palette tokens.
        assert_eq!(components.pane_border_focused, degraded.accent);
        // Explicit overrides degrade through the same mapping.
        assert_eq!(
            components.toast_border_success,
            Color::Indexed(crate::config::rgb_to_xterm256(0xa6, 0xe3, 0xa1))
        );
    }

    #[test]
    fn selection_row_bg_falls_back_when_the_theme_leaves_selection_unset() {
        // terminal 16 色主题的 selection_bg 是 Color::Reset：直接当选中底色用，
        // 选中行与普通行像素完全一致（上游 #4300）。
        let terminal = Palette::terminal();
        assert_eq!(terminal.selection_bg, Color::Reset);
        assert_ne!(terminal.selection_row_bg(), Color::Reset);
        // 回退也不能落到 active_row_bg：侧栏聚焦行用的就是它，否则「选中」
        // 与「聚焦」同色，导航光标依旧不可辨认。
        assert_ne!(terminal.selection_row_bg(), terminal.active_row_bg);
        assert_eq!(terminal.selection_row_bg(), terminal.accent);

        // 显式给了 selection_bg 的主题原样返回。
        let catppuccin = Palette::catppuccin();
        assert_eq!(catppuccin.selection_row_bg(), catppuccin.selection_bg);

        // selection_bg 与 accent 都未定义时退到 active_row_bg 保底，仍不是 Reset。
        let mut bare = Palette::terminal();
        bare.accent = Color::Reset;
        assert_eq!(bare.selection_row_bg(), bare.active_row_bg);
        assert_ne!(bare.selection_row_bg(), Color::Reset);
    }

    /// C-28 (ds-02)：terminal 调色板曾把 `surface0`/`surface1` 留成
    /// `Color::Reset`，于是输入框、键帽、侧栏与标签的 hover 全部与常态面板
    /// 同像素，塌缩成普通文本。16 色不是必然代价——DarkGray/Gray 可用。
    #[test]
    fn terminal_theme_surfaces_stay_visible_against_the_terminal_background() {
        let terminal = Palette::terminal();
        assert_ne!(terminal.surface0, Color::Reset, "surface0 塌缩成终端背景");
        assert_ne!(terminal.surface1, Color::Reset, "surface1 塌缩成终端背景");
        assert_ne!(
            terminal.surface0, terminal.surface1,
            "surface0/surface1 必须保持「弱 / 更亮」的层级"
        );
        // 结构面不能撞上同一主题的行语义：选中行用 selection_row_bg（accent
        // 回退），聚焦行用 active_row_bg。
        assert_ne!(terminal.surface0, terminal.selection_row_bg());
        assert_ne!(terminal.surface1, terminal.selection_row_bg());
    }

    #[test]
    fn surface_selection_bg_stays_distinct_from_the_plain_row_for_every_theme() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).expect("built-in theme");
            let selected = palette.surface_selection_bg();
            assert_ne!(selected, Color::Reset, "{name} 选中行落回终端背景");
            assert_ne!(selected, palette.panel_bg, "{name} 选中行与常态行同色");
        }
    }

    #[test]
    fn surface_selection_bg_backs_off_when_the_surface_collides_with_the_focused_row() {
        // terminal：surface0 与 surface_dim 同为 DarkGray，选中行会被读成聚焦行，
        // 所以退到 selection_row_bg() 的 accent（Blue）。
        let terminal = Palette::terminal();
        assert_eq!(terminal.surface0, terminal.surface_dim);
        assert_eq!(terminal.surface_selection_bg(), terminal.selection_row_bg());
        assert_ne!(terminal.surface_selection_bg(), terminal.surface_dim);

        // 常规主题用 surface0 原值，不改变观感。
        let catppuccin = Palette::catppuccin();
        assert_eq!(catppuccin.surface_selection_bg(), catppuccin.surface0);

        // rose-pine-dawn 的 surface0/surface_dim/selection_bg 三个 token 同值，
        // 回退也救不回「选中 ≠ 聚焦」——这是该主题自身的 token 重复（归 ds-11），
        // 本批不动主题数据；这里只钉住「至少不与常态行同色」。
        let dawn = Palette::rose_pine_dawn();
        assert_eq!(dawn.surface0, dawn.surface_dim);
        assert_ne!(dawn.surface_selection_bg(), dawn.panel_bg);
    }

    #[test]
    fn hover_row_bg_stays_visible_and_distinct_from_the_selected_row() {
        // 常态行画的是 panel_bg、选中行画的是 accent（`shell::list_row_bg`
        // 的三态），所以真正要守的是 hover ≠ panel_bg 且 hover ≠ accent——
        // 逐个内置主题核对，不是抽两个样本（MENU-01）。
        for name in crate::config::THEME_NAMES
            .iter()
            .copied()
            .filter(|name| *name != "terminal")
        {
            let palette = Palette::from_name(name).expect("built-in theme");
            let hover = palette.hover_row_bg();
            assert_ne!(hover, Color::Reset, "hover 底色落回 Reset：{name}");
            assert_ne!(hover, palette.panel_bg, "悬浮行与常态行同色：{name}");
            assert_ne!(hover, palette.accent, "悬浮行与选中行同色：{name}");
            let background = contrast_ratio(palette.panel_bg, hover);
            assert!(
                background >= 1.10,
                "悬浮行与常态行肉眼不可分：{name} {background:.3}:1"
            );
            let selected = contrast_ratio(palette.accent, hover);
            assert!(
                selected >= 1.10,
                "悬浮行与选中行肉眼不可分：{name} {selected:.3}:1"
            );
        }

        // 16 色主题算不出亮度，只能守「不是同一个色号」。
        let terminal = Palette::terminal();
        // surface1 = Gray 是 terminal 的 hover 面（surface0 = DarkGray 是常态
        // 结构面，两者与 Reset 的 panel_bg 都可区分）。
        assert_eq!(terminal.hover_row_bg(), Color::Gray);
        assert_ne!(terminal.hover_row_bg(), terminal.panel_bg);
        assert_ne!(terminal.hover_row_bg(), terminal.accent);
        assert_ne!(terminal.hover_row_bg(), terminal.surface0);

        // 自定义主题把候选逐个留成 Reset 时依次回退，绝不落回 Reset。
        let mut bare = Palette::terminal();
        bare.surface1 = Color::Reset;
        assert_eq!(bare.hover_row_bg(), bare.surface0);
        bare.surface0 = Color::Reset;
        assert_eq!(bare.hover_row_bg(), bare.surface_dim);
        bare.surface_dim = Color::Reset;
        assert_eq!(bare.hover_row_bg(), bare.active_row_bg);
        bare.active_row_bg = Color::Reset;
        assert_eq!(bare.hover_row_bg(), bare.selection_row_bg());
        assert_ne!(bare.hover_row_bg(), Color::Reset);
    }

    /// rose-pine-dawn 是门槛的来源：它的 surface1 比 panel_bg 还亮、逐通道只
    /// 差 5/6/6，旧实现直接取 surface1 等于没有悬浮提示。
    #[test]
    fn rose_pine_dawn_hover_row_skips_its_near_invisible_surface1() {
        let palette = Palette::rose_pine_dawn();
        assert!(contrast_ratio(palette.panel_bg, palette.surface1) < 1.10);
        assert_ne!(palette.hover_row_bg(), palette.surface1);
        assert_eq!(palette.hover_row_bg(), palette.active_row_bg);
    }

    #[test]
    fn hover_bg_component_token_falls_back_and_accepts_overrides() {
        let palette = Palette::catppuccin();
        let components = ComponentStyles::from_palette(&palette);
        assert_eq!(components.hover_bg, palette.hover_row_bg());

        let overrides = crate::config::ThemeComponentsConfig {
            hover_bg: Some("#010203".into()),
            ..Default::default()
        };
        let components = ComponentStyles::resolve(
            &palette,
            Some(&overrides),
            crate::config::ColorDepth::Truecolor,
        );
        assert_eq!(components.hover_bg, Color::Rgb(1, 2, 3));
    }
}

#[cfg(test)]
mod agent_activity_store_tests {
    use super::*;
    use crate::api::schema::{
        AgentActivityKind, AgentActivityNode, AgentActivityStatus, AgentStatus, ExternalAgentInfo,
    };
    use std::time::{Duration, Instant};

    fn node(id: &str, parent: Option<&str>, status: AgentActivityStatus) -> AgentActivityNode {
        AgentActivityNode {
            id: id.into(),
            kind: AgentActivityKind::Task,
            label: id.into(),
            status,
            parent_id: parent.map(str::to_owned),
            ..AgentActivityNode::default()
        }
    }

    fn ids(nodes: &[AgentActivityNode]) -> Vec<&str> {
        nodes.iter().map(|node| node.id.as_str()).collect()
    }

    fn external(id: &str) -> ExternalAgentInfo {
        ExternalAgentInfo {
            external_id: id.into(),
            source: "ignored".into(),
            agent_status: AgentStatus::Working,
            label: id.into(),
            readable: true,
            agent: None,
            cwd: None,
            updated_at_ms: None,
            activity: Vec::new(),
        }
    }

    #[test]
    fn activity_writes_report_counts_only_when_the_tree_changes() {
        let mut store = AgentActivityStore::default();
        let pane = PaneId::from_raw(1);
        let t0 = Instant::now();
        assert_eq!(
            store.apply_activity(pane, Vec::new(), t0),
            None,
            "空树不建记录"
        );
        assert!(store.activity(pane).is_none());

        let tree = vec![
            node("a", None, AgentActivityStatus::Running),
            node("b", Some("a"), AgentActivityStatus::Done),
        ];
        assert_eq!(
            store.apply_activity(pane, tree.clone(), t0),
            Some(AgentActivityApplied {
                counts: AgentActivityCounts {
                    running: 1,
                    total: 2
                },
                summary_changed: true
            })
        );
        let first = store.activity(pane).expect("已落库").clone();
        assert_eq!((first.revision, first.truncated), (1, false));

        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            store.apply_activity(pane, tree, t1),
            None,
            "内容未变不算变化"
        );
        let unchanged = store.activity(pane).expect("仍在");
        assert_eq!(unchanged.revision, 1);
        assert_eq!(unchanged.refreshed_at, t1, "刷新时刻照记");

        assert_eq!(
            store.apply_activity(pane, Vec::new(), t1),
            Some(AgentActivityApplied {
                counts: AgentActivityCounts {
                    running: 0,
                    total: 0
                },
                summary_changed: true
            }),
            "已有记录的树清空是变化"
        );
        assert_eq!(store.activity(pane).expect("仍在").revision, 2);
    }

    #[test]
    fn truncation_keeps_running_nodes_with_their_ancestors_in_source_order() {
        let mut store = AgentActivityStore::default();
        let pane = PaneId::from_raw(1);
        let mut tree = vec![node("root", None, AgentActivityStatus::Done)];
        tree.extend((0..40).map(|index| {
            node(
                &format!("done-{index:02}"),
                Some("root"),
                AgentActivityStatus::Done,
            )
        }));
        // 运行中的深层节点排在最后：父链 mid → root 必须一起保留，且不产生孤儿。
        tree.push(node("mid", Some("root"), AgentActivityStatus::Blocked));
        tree.push(node("leaf", Some("mid"), AgentActivityStatus::Running));
        tree.push(node(
            "orphan-parent-missing",
            Some("gone"),
            AgentActivityStatus::Running,
        ));
        let applied = store
            .apply_activity(pane, tree, Instant::now())
            .expect("落库");
        assert_eq!(
            applied.counts,
            AgentActivityCounts {
                running: 2,
                total: 44
            },
            "计数是截断前的"
        );
        let stored = store.activity(pane).expect("已落库");
        assert!(stored.truncated);
        assert_eq!(stored.nodes.len(), MAX_AGENT_ACTIVITY_NODES);
        let kept = ids(&stored.nodes);
        assert_eq!(kept[0], "root", "来源顺序不变");
        for id in ["mid", "leaf", "orphan-parent-missing"] {
            assert!(kept.contains(&id), "{id} 应优先保留");
        }
        let expected_tail = ["mid", "leaf", "orphan-parent-missing"];
        assert_eq!(&kept[kept.len() - 3..], expected_tail);
        for kept_node in &stored.nodes {
            if let Some(parent) = kept_node.parent_id.as_deref().filter(|p| *p != "gone") {
                assert!(
                    kept.contains(&parent),
                    "{} 的父节点 {parent} 被截掉",
                    kept_node.id
                );
            }
        }
    }

    #[test]
    fn truncation_skips_a_running_chain_that_cannot_fit_whole() {
        let mut store = AgentActivityStore::default();
        let pane = PaneId::from_raw(1);
        // 一条 40 层的链，只有最深处在运行：整条链放不下 → 不保留它（不产生孤儿），
        // 其余名额按来源顺序补入父节点已保留的节点。
        let mut tree = vec![node("n00", None, AgentActivityStatus::Done)];
        for depth in 1..40 {
            let status = if depth == 39 {
                AgentActivityStatus::Running
            } else {
                AgentActivityStatus::Done
            };
            tree.push(node(
                &format!("n{depth:02}"),
                Some(&format!("n{:02}", depth - 1)),
                status,
            ));
        }
        store.apply_activity(pane, tree, Instant::now());
        let stored = store.activity(pane).expect("已落库");
        assert_eq!(stored.nodes.len(), MAX_AGENT_ACTIVITY_NODES);
        assert_eq!(ids(&stored.nodes)[..3], ["n00", "n01", "n02"]);
        assert!(!ids(&stored.nodes).contains(&"n39"));
    }

    #[test]
    fn forgetting_or_retaining_panes_drops_seq_tree_and_pending_hints() {
        let mut store = AgentActivityStore::default();
        let kept = PaneId::from_raw(1);
        let gone = PaneId::from_raw(2);
        let now = Instant::now();
        for pane in [kept, gone] {
            store.ensure_launch_seq(pane);
            store.apply_activity(
                pane,
                vec![node("a", None, AgentActivityStatus::Running)],
                now,
            );
            store.note_hint(pane);
        }
        assert!(store.retain_panes(|pane| pane == kept));
        assert_eq!(store.launch_seq(gone), 0);
        assert!(store.activity(gone).is_none());
        let mut hinted = Vec::new();
        store.drain_hints(|pane| hinted.push(pane));
        assert_eq!(hinted, [kept], "被清掉的 pane 的提示一并丢弃");
        assert!(!store.retain_panes(|pane| pane == kept), "无变化");

        store.note_hint(kept);
        assert!(!store.note_hint(kept), "未取走前重复提示只算一次");
        assert!(store.forget_pane(kept));
        assert!(!store.has_hints());
        assert!(!store.forget_pane(kept));
    }

    #[test]
    fn latest_hint_text_is_kept_per_pane_capped_and_dropped_with_the_pane() {
        let mut store = AgentActivityStore::default();
        let kept = PaneId::from_raw(1);
        let gone = PaneId::from_raw(2);
        assert!(store.latest_hint(kept).is_none());
        assert!(store.store_hint(kept, "first"));
        assert!(store.store_hint(kept, "second"), "后来者覆盖");
        assert!(store.store_hint(gone, "elsewhere"));
        assert_eq!(store.latest_hint(kept).as_deref(), Some("second"));
        assert_eq!(store.latest_hint(gone).as_deref(), Some("elsewhere"));

        let oversized = "x".repeat(MAX_AGENT_ACTIVITY_HINT_BYTES + 1);
        assert!(!store.store_hint(kept, &oversized), "超限不缓存");
        assert_eq!(
            store.latest_hint(kept).as_deref(),
            Some("second"),
            "超限时上一份保留"
        );
        let at_limit = "y".repeat(MAX_AGENT_ACTIVITY_HINT_BYTES);
        assert!(store.store_hint(kept, &at_limit), "恰好到上限可缓存");

        assert!(
            !store.retain_panes(|pane| pane == kept),
            "只有 hint 文本的 pane 被清掉不算投影变化"
        );
        assert!(store.latest_hint(gone).is_none());
        assert!(store.latest_hint(kept).is_some());
        assert!(!store.forget_pane(kept), "没有序号和树：不算投影变化");
        assert!(store.latest_hint(kept).is_none(), "释放后提示文本作废");
    }

    #[test]
    fn external_sources_are_replaced_whole_and_expire_to_unreadable() {
        let mut store = AgentActivityStore::default();
        let t0 = Instant::now();
        assert!(store.apply_external("zcode", vec![external("zcode:b"), external("zcode:a")], t0));
        assert!(store.apply_external("other", vec![external("other:x")], t0));
        let listed = store
            .external()
            .iter()
            .map(|record| {
                (
                    record.info.source.as_str(),
                    record.info.external_id.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            listed,
            [
                ("other", "other:x"),
                ("zcode", "zcode:a"),
                ("zcode", "zcode:b")
            ],
            "按 (source, external_id) 排序，source 以登记名为准"
        );
        assert!(
            !store.apply_external("zcode", vec![external("zcode:a"), external("zcode:b")], t0),
            "同内容不算变化"
        );
        assert!(store.apply_external("zcode", vec![external("zcode:a")], t0));
        assert_eq!(store.external().len(), 2, "整源替换，不影响其他来源");

        // zcode 一直刷新成功，other 60 s 没有成功刷新 → 只有 other 变为暂不可读。
        let later = t0 + EXTERNAL_AGENT_STALE_AFTER;
        store.apply_external("zcode", vec![external("zcode:a")], later);
        assert!(store.expire_external(later, EXTERNAL_AGENT_STALE_AFTER));
        let readable = store
            .external()
            .iter()
            .map(|record| (record.info.external_id.as_str(), record.info.readable))
            .collect::<Vec<_>>();
        assert_eq!(readable, [("other:x", false), ("zcode:a", true)]);
        assert!(
            !store.expire_external(later, EXTERNAL_AGENT_STALE_AFTER),
            "已标记不重复变化"
        );
        // 来源恢复：整源替换回可读。
        assert!(store.apply_external("other", vec![external("other:x")], later));
        assert!(store.external().iter().all(|record| record.info.readable));
    }

    #[test]
    fn external_activity_is_truncated_with_counts_kept_on_the_record() {
        let mut store = AgentActivityStore::default();
        let mut agent = external("zcode:a");
        agent.activity = (0..40)
            .map(|index| node(&format!("t{index}"), None, AgentActivityStatus::Running))
            .collect();
        store.apply_external("zcode", vec![agent], Instant::now());
        let record = &store.external()[0];
        assert_eq!(record.info.activity.len(), MAX_AGENT_ACTIVITY_NODES);
        assert_eq!(
            (record.running, record.total, record.truncated),
            (40, 40, true)
        );
    }

    fn state_with_agent_pane() -> (AppState, PaneId, PaneId) {
        let mut state = AppState::test_new();
        state.workspaces.push(Workspace::test_new("agent"));
        state.workspaces.push(Workspace::test_new("shell"));
        state.ensure_test_terminals();
        let agent_pane = state.workspaces[0].tabs[0].root_pane;
        let shell_pane = state.workspaces[1].tabs[0].root_pane;
        let terminal_id = state.workspaces[0]
            .pane_state(agent_pane)
            .expect("pane")
            .attached_terminal_id
            .clone();
        let terminal = state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.detected_agent = Some(crate::detect::Agent::Claude);
        terminal.state = AgentState::Working;
        (state, agent_pane, shell_pane)
    }

    #[test]
    fn app_state_accepts_activity_only_for_panes_that_host_an_agent() {
        let (mut state, agent_pane, shell_pane) = state_with_agent_pane();
        let tree = vec![node("a", None, AgentActivityStatus::Running)];
        let epoch = state.projection_epoch;
        assert!(state
            .apply_agent_activity(shell_pane, tree.clone(), Instant::now())
            .is_none());
        assert_eq!(state.projection_epoch, epoch, "迟到结果不改投影");
        assert!(state
            .apply_agent_activity(agent_pane, tree.clone(), Instant::now())
            .is_some());
        assert_ne!(
            state.projection_epoch, epoch,
            "活动摘要进投影，写入点递增纪元"
        );
        let epoch = state.projection_epoch;
        assert!(state
            .apply_agent_activity(agent_pane, tree, Instant::now())
            .is_none());
        assert_eq!(state.projection_epoch, epoch, "无变化不递增纪元");

        let mut visited = Vec::new();
        state.for_each_agent_pane(|pane_id, agent, working| {
            visited.push((pane_id, agent.to_owned(), working));
        });
        assert_eq!(visited, [(agent_pane, "claude".to_owned(), true)]);
        let subject = state
            .agent_activity_subject(agent_pane)
            .expect("持有 agent");
        assert_eq!(subject.agent, "claude");
        assert!(state.agent_activity_subject(shell_pane).is_none());
    }

    /// 客户端帧扇出是乘法路径：投影纪元是每客户端投影复用的键，而快照只带计数、
    /// 截断标记与最新节点。深层节点（这里是 `ended_at_ms`）变化必须落库供
    /// `agent.activity.read` / `agent.get` 读到，但不得递增纪元——否则每个挂载
    /// 客户端都要整份重建快照再深比较，产出逐字节相同。
    #[test]
    fn deep_node_changes_keep_the_projection_epoch_but_still_land_in_the_store() {
        let (mut state, agent_pane, _) = state_with_agent_pane();
        let running = AgentActivityNode {
            started_at_ms: Some(10),
            ..node("a", None, AgentActivityStatus::Running)
        };
        let mut done = AgentActivityNode {
            started_at_ms: Some(1),
            ended_at_ms: Some(2),
            ..node("b", Some("a"), AgentActivityStatus::Done)
        };
        assert!(state
            .apply_agent_activity(
                agent_pane,
                vec![running.clone(), done.clone()],
                Instant::now()
            )
            .is_some_and(|applied| applied.summary_changed));

        let epoch = state.projection_epoch;
        done.ended_at_ms = Some(500);
        let applied = state
            .apply_agent_activity(agent_pane, vec![running, done], Instant::now())
            .expect("内容变了");
        assert!(!applied.summary_changed, "深层节点变化不进摘要");
        assert_eq!(
            applied.counts,
            AgentActivityCounts {
                running: 1,
                total: 2
            },
            "事件计数照常给出"
        );
        assert_eq!(
            state.projection_epoch, epoch,
            "摘要没变，不让客户端重建投影"
        );
        let stored = state.agent_activity.activity(agent_pane).expect("已落库");
        assert_eq!(stored.revision, 2, "整棵树照常落库");
        assert_eq!(stored.nodes[1].ended_at_ms, Some(500));

        // 最新节点本身变了（运行中的节点结束）：摘要变化，纪元递增。
        let applied = state
            .apply_agent_activity(
                agent_pane,
                vec![
                    AgentActivityNode {
                        started_at_ms: Some(10),
                        ended_at_ms: Some(600),
                        ..node("a", None, AgentActivityStatus::Done)
                    },
                    AgentActivityNode {
                        started_at_ms: Some(1),
                        ended_at_ms: Some(500),
                        ..node("b", Some("a"), AgentActivityStatus::Done)
                    },
                ],
                Instant::now(),
            )
            .expect("内容变了");
        assert!(applied.summary_changed);
        assert_ne!(state.projection_epoch, epoch);
    }

    #[test]
    fn app_state_expiry_drops_panes_that_stopped_hosting_an_agent() {
        let (mut state, agent_pane, _) = state_with_agent_pane();
        state.apply_agent_activity(
            agent_pane,
            vec![node("a", None, AgentActivityStatus::Running)],
            Instant::now(),
        );
        assert!(!state.expire_agent_activity(Instant::now()), "仍持有 agent");
        let terminal_id = state.workspaces[0]
            .pane_state(agent_pane)
            .expect("pane")
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .detected_agent = None;
        let epoch = state.projection_epoch;
        assert!(state.expire_agent_activity(Instant::now()));
        assert!(state.agent_activity.activity(agent_pane).is_none());
        assert_ne!(state.projection_epoch, epoch);
    }
}
