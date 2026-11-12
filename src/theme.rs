//! The colors of the herdr theme, for the popups of the plugin.
//!
//! herdr gives no color to its plugins: no variable, no command and no field of the API. Thus the
//! plugin reads `[theme]` and `[ui] accent` in the config of herdr, and it builds the palette of
//! herdr with the same rules: the theme `name` (catppuccin by default), `dark_name` and `light_name`
//! with `auto_switch`, then `[theme.custom]`, then `[ui] accent`, then `[theme.custom.dark]` or
//! `[theme.custom.light]`. Each process of a popup reads the config: a new theme shows at the next
//! opening. herdr-omnibar uses the same file.
//!
//! The palettes come from herdr 0.9.5 (src/app/state.rs, `Palette`): update them with herdr.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use toml::{Table, Value};

/// A color of herdr: the default color of the terminal, an ANSI color, or an RGB color. The names
/// are the names of ratatui, that herdr uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    /// A color of the 256 colors of the terminal.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// The number of an ANSI color, as ratatui gives it to the terminal: 0 to 15 for a named color,
    /// or the number of a 256 color.
    fn ansi(self) -> Option<u8> {
        Some(match self {
            Color::Black => 0,
            Color::Red => 1,
            Color::Green => 2,
            Color::Yellow => 3,
            Color::Blue => 4,
            Color::Magenta => 5,
            Color::Cyan => 6,
            Color::Gray => 7,
            Color::DarkGray => 8,
            Color::LightRed => 9,
            Color::LightGreen => 10,
            Color::LightYellow => 11,
            Color::LightBlue => 12,
            Color::LightMagenta => 13,
            Color::LightCyan => 14,
            Color::White => 15,
            Color::Indexed(number) => number,
            Color::Reset | Color::Rgb(..) => return None,
        })
    }

    /// The escape code that gives this color to the text.
    pub fn foreground(self) -> String {
        match (self, self.ansi()) {
            (Color::Rgb(r, g, b), _) => format!("\x1b[38;2;{r};{g};{b}m"),
            (_, Some(number)) => format!("\x1b[38;5;{number}m"),
            _ => "\x1b[39m".to_owned(),
        }
    }

    /// The escape code that gives this color to the background.
    pub fn background(self) -> String {
        match (self, self.ansi()) {
            (Color::Rgb(r, g, b), _) => format!("\x1b[48;2;{r};{g};{b}m"),
            (_, Some(number)) => format!("\x1b[48;5;{number}m"),
            _ => "\x1b[49m".to_owned(),
        }
    }

    /// The color for the option --color of fzf: #rrggbb, an ANSI number, or -1 for the default.
    pub fn fzf(self) -> String {
        match (self, self.ansi()) {
            (Color::Rgb(r, g, b), _) => format!("#{r:02x}{g:02x}{b:02x}"),
            (_, Some(number)) => number.to_string(),
            _ => "-1".to_owned(),
        }
    }
}

/// The colors of a herdr theme, with the names of herdr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub panel_bg: Color,
    pub sidebar_bg: Color,
    pub active_row_bg: Color,
    pub selection_bg: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub surface_dim: Color,
    pub overlay0: Color,
    pub overlay1: Color,
    pub text: Color,
    pub subtext0: Color,
    pub mauve: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub blue: Color,
    pub teal: Color,
    pub peach: Color,
}

impl Palette {
    /// Catppuccin Mocha — the default.
    fn catppuccin() -> Palette {
        Palette {
            accent: Color::Rgb(137, 180, 250), // blue
            panel_bg: Color::Rgb(24, 24, 37),
            sidebar_bg: Color::Reset,
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
    fn catppuccin_latte() -> Palette {
        Palette {
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
    fn terminal() -> Palette {
        Palette {
            accent: Color::Blue,
            panel_bg: Color::Reset,
            sidebar_bg: Color::Reset,
            active_row_bg: Color::DarkGray,
            selection_bg: Color::Reset,
            surface0: Color::Reset,
            surface1: Color::DarkGray,
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
    fn tokyo_night() -> Palette {
        Palette {
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
    fn tokyo_night_day() -> Palette {
        Palette {
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
    fn dracula() -> Palette {
        Palette {
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
    fn nord() -> Palette {
        Palette {
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
    fn gruvbox() -> Palette {
        Palette {
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
    fn gruvbox_light() -> Palette {
        Palette {
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
    fn one_dark() -> Palette {
        Palette {
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
    fn one_light() -> Palette {
        Palette {
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
    fn solarized() -> Palette {
        Palette {
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
    fn solarized_light() -> Palette {
        Palette {
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
    fn kanagawa() -> Palette {
        Palette {
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
    fn kanagawa_lotus() -> Palette {
        Palette {
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
    fn rose_pine() -> Palette {
        Palette {
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
    fn rose_pine_dawn() -> Palette {
        Palette {
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
    fn vesper() -> Palette {
        Palette {
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

    /// The palette of a theme name, or None for an unknown name.
    pub fn from_name(name: &str) -> Option<Palette> {
        Some(match canonical_name(name)? {
            "catppuccin" => Palette::catppuccin(),
            "catppuccin-latte" => Palette::catppuccin_latte(),
            "terminal" => Palette::terminal(),
            "tokyo-night" => Palette::tokyo_night(),
            "tokyo-night-day" => Palette::tokyo_night_day(),
            "dracula" => Palette::dracula(),
            "nord" => Palette::nord(),
            "gruvbox" => Palette::gruvbox(),
            "gruvbox-light" => Palette::gruvbox_light(),
            "one-dark" => Palette::one_dark(),
            "one-light" => Palette::one_light(),
            "solarized" => Palette::solarized(),
            "solarized-light" => Palette::solarized_light(),
            "kanagawa" => Palette::kanagawa(),
            "kanagawa-lotus" => Palette::kanagawa_lotus(),
            "rose-pine" => Palette::rose_pine(),
            "rose-pine-dawn" => Palette::rose_pine_dawn(),
            "vesper" => Palette::vesper(),
            _ => return None,
        })
    }

    /// Replace the colors that a table of the config gives: `[theme.custom]`, or its table `dark`
    /// or `light`. A key that is not a string, or not a color of the palette, stays without effect.
    fn apply(&mut self, table: &Table) {
        for (key, value) in table {
            let Some(text) = value.as_str() else {
                continue;
            };
            let color = parse_color(text);
            match key.as_str() {
                "accent" => self.accent = color,
                "panel_bg" => self.panel_bg = color,
                "sidebar_bg" => self.sidebar_bg = color,
                "active_row_bg" => self.active_row_bg = color,
                "selection_bg" => self.selection_bg = color,
                "surface0" => self.surface0 = color,
                "surface1" => self.surface1 = color,
                "surface_dim" => self.surface_dim = color,
                "overlay0" => self.overlay0 = color,
                "overlay1" => self.overlay1 = color,
                "text" => self.text = color,
                "subtext0" => self.subtext0 = color,
                "mauve" => self.mauve = color,
                "green" => self.green = color,
                "yellow" => self.yellow = color,
                "red" => self.red = color,
                "blue" => self.blue = color,
                "teal" => self.teal = color,
                "peach" => self.peach = color,
                _ => {}
            }
        }
    }
}

/// The canonical name of a theme, with the aliases of herdr.
fn canonical_name(name: &str) -> Option<&'static str> {
    Some(
        match name.to_lowercase().replace([' ', '_'], "-").as_str() {
            "catppuccin" | "catppuccin-mocha" => "catppuccin",
            "catppuccin-latte" | "latte" | "light" => "catppuccin-latte",
            "terminal" => "terminal",
            "tokyo-night" | "tokyonight" => "tokyo-night",
            "tokyo-night-day" | "tokyo-day" | "tokyonight-day" => "tokyo-night-day",
            "dracula" => "dracula",
            "nord" => "nord",
            "gruvbox" | "gruvbox-dark" => "gruvbox",
            "gruvbox-light" => "gruvbox-light",
            "one-dark" | "onedark" => "one-dark",
            "one-light" | "onelight" => "one-light",
            "solarized" | "solarized-dark" => "solarized",
            "solarized-light" => "solarized-light",
            "kanagawa" => "kanagawa",
            "kanagawa-lotus" | "lotus" => "kanagawa-lotus",
            "rose-pine" | "rosepine" => "rose-pine",
            "rose-pine-dawn" | "rosepine-dawn" | "dawn" => "rose-pine-dawn",
            "vesper" => "vesper",
            _ => return None,
        },
    )
}

/// The dark and the light theme of a theme name, for `auto_switch`.
fn siblings(name: &str) -> (&str, &str) {
    match canonical_name(name) {
        Some("catppuccin" | "catppuccin-latte") => ("catppuccin", "catppuccin-latte"),
        Some("tokyo-night" | "tokyo-night-day") => ("tokyo-night", "tokyo-night-day"),
        Some("gruvbox" | "gruvbox-light") => ("gruvbox", "gruvbox-light"),
        Some("one-dark" | "one-light") => ("one-dark", "one-light"),
        Some("solarized" | "solarized-light") => ("solarized", "solarized-light"),
        Some("kanagawa" | "kanagawa-lotus") => ("kanagawa", "kanagawa-lotus"),
        Some("rose-pine" | "rose-pine-dawn") => ("rose-pine", "rose-pine-dawn"),
        _ => (name, name),
    }
}

/// A color of the config of herdr: #rrggbb, #rgb, rgb(r, g, b), a name, or reset.
fn parse_color(text: &str) -> Color {
    let text = text.trim().to_lowercase();
    if matches!(text.as_str(), "reset" | "default" | "none" | "transparent") {
        return Color::Reset;
    }
    if let Some(hex) = text.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .filter_map(|digit| digit.to_digit(16).map(|value| value as u8))
            .collect();
        match digits.as_slice() {
            [r1, r2, g1, g2, b1, b2] if hex.len() == 6 => {
                return Color::Rgb(r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2);
            }
            [r, g, b] if hex.len() == 3 => return Color::Rgb(r * 17, g * 17, b * 17),
            _ => {}
        }
    }
    if let Some(inner) = text
        .strip_prefix("rgb(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts: Vec<u8> = inner
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        if let [r, g, b] = parts.as_slice() {
            return Color::Rgb(*r, *g, *b);
        }
    }
    match text.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        // herdr gives cyan to an unknown color.
        _ => Color::Cyan,
    }
}

/// A theme of herdr: its palette, and true for the theme `terminal`. The theme `terminal` uses the
/// ANSI colors of the terminal: the roles of the popups then keep fixed greys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub palette: Palette,
    pub terminal: bool,
}

/// The variable that gives the appearance of the host (dark or light) to the processes that a
/// popup starts: they do not ask macOS again.
pub const APPEARANCE_VARIABLE: &str = "THEME_HOST_APPEARANCE";

/// The appearance of the host, if a popup asked it: only `auto_switch` asks it.
static HOST_IS_DARK: OnceLock<bool> = OnceLock::new();

/// The appearance to give to the processes of a popup: "dark", "light", or None if the popup did
/// not ask it.
pub fn appearance() -> Option<&'static str> {
    HOST_IS_DARK
        .get()
        .map(|dark| if *dark { "dark" } else { "light" })
}

/// A string of a table of the config.
fn text_of<'a>(table: Option<&'a Table>, key: &str) -> Option<&'a str> {
    table?.get(key)?.as_str()
}

/// A table of a table of the config.
fn table_of<'a>(table: Option<&'a Table>, key: &str) -> Option<&'a Table> {
    table?.get(key)?.as_table()
}

/// The theme of a config of herdr. `host_is_dark` gives the appearance of the host: only
/// `auto_switch` asks it.
fn resolve(text: &str, host_is_dark: impl FnOnce() -> bool) -> Theme {
    // A config with a wrong syntax gives the defaults, like herdr. A key with a wrong type stays
    // without effect: the other keys still count.
    let config: Table = toml::from_str(text).unwrap_or_default();
    let theme = table_of(Some(&config), "theme");
    let custom = table_of(theme, "custom");
    let manual = text_of(theme, "name").unwrap_or("catppuccin");
    let auto_switch = theme
        .and_then(|theme| theme.get("auto_switch"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (name, fallback, mode) = if !auto_switch {
        (manual, "catppuccin", None)
    } else {
        let (default_dark, default_light) = siblings(manual);
        if host_is_dark() {
            let name = text_of(theme, "dark_name").unwrap_or(default_dark);
            (name, "catppuccin", Some("dark"))
        } else {
            let name = text_of(theme, "light_name").unwrap_or(default_light);
            (name, "catppuccin-latte", Some("light"))
        }
    };
    let mut palette = Palette::from_name(name)
        .or_else(|| Palette::from_name(fallback))
        .unwrap_or_else(Palette::catppuccin);
    if let Some(custom) = custom {
        palette.apply(custom);
    }
    // [ui] accent is the old key of the accent: herdr uses it if it is not cyan and if
    // [theme.custom] has no accent.
    if let Some(accent) = text_of(table_of(Some(&config), "ui"), "accent") {
        if accent != "cyan" && text_of(custom, "accent").is_none() {
            palette.accent = parse_color(accent);
        }
    }
    if let Some(mode) = mode.and_then(|mode| table_of(custom, mode)) {
        palette.apply(mode);
    }
    Theme {
        palette,
        terminal: canonical_name(name) == Some("terminal"),
    }
}

/// The config file of herdr: HERDR_CONFIG_PATH, else config.toml in the config folder of herdr
/// (XDG_CONFIG_HOME, else ~/.config).
fn config_path() -> PathBuf {
    let set = |name: &str| env::var_os(name).filter(|value| !value.is_empty());
    if let Some(path) = set("HERDR_CONFIG_PATH") {
        return PathBuf::from(path);
    }
    let folder = match set("XDG_CONFIG_HOME") {
        Some(folder) => PathBuf::from(folder),
        None => set("HOME")
            .map(|home| PathBuf::from(home).join(".config"))
            .unwrap_or_default(),
    };
    folder.join("herdr").join("config.toml")
}

/// True if the host is dark. herdr asks the terminal: the popup asks macOS, else it says dark,
/// like herdr without an answer. A process that a popup starts gets the answer in a variable.
fn host_is_dark() -> bool {
    *HOST_IS_DARK.get_or_init(|| match env::var(APPEARANCE_VARIABLE).as_deref() {
        Ok("dark") => true,
        Ok("light") => false,
        _ if !cfg!(target_os = "macos") => true,
        _ => Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "Dark")
            .unwrap_or(true),
    })
}

/// The theme of the running herdr, from its config file.
pub fn load() -> Theme {
    let text = fs::read_to_string(config_path()).unwrap_or_default();
    resolve(&text, host_is_dark)
}

/// The theme of the popups, read one time for each process. The tests use the palette dracula:
/// the config of the machine does not change them.
pub fn current() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        if cfg!(test) {
            Theme {
                palette: Palette::dracula(),
                terminal: false,
            }
        } else {
            load()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_of_the_config_gives_its_palette() {
        let palette = resolve("[theme]\nname = \"dracula\"\n", || true).palette;
        assert_eq!(palette.accent, Color::Rgb(189, 147, 249));
        assert_eq!(resolve("", || true).palette, Palette::catppuccin());
        assert_eq!(
            resolve("[theme]\nname = \"unknown\"\n", || true).palette,
            Palette::catppuccin()
        );
        assert_eq!(
            resolve("[theme]\nname = \"Tokyo Night\"\n", || true).palette,
            Palette::tokyo_night()
        );
    }

    #[test]
    fn the_custom_colors_and_the_accent_of_ui_change_the_palette() {
        let text = "[theme]\nname = \"nord\"\n[theme.custom]\nred = \"#ff0000\"\ngreen = \"rgb(1, 2, 3)\"\n[ui]\naccent = \"#0f0\"\n";
        let palette = resolve(text, || true).palette;
        assert_eq!(palette.red, Color::Rgb(255, 0, 0));
        assert_eq!(palette.green, Color::Rgb(1, 2, 3));
        assert_eq!(palette.accent, Color::Rgb(0, 255, 0));
        assert_eq!(palette.text, Palette::nord().text);
    }

    #[test]
    fn a_key_with_a_wrong_type_keeps_the_other_keys_and_terminal_is_known() {
        let text = "[theme]\nname = \"nord\"\nauto_switch = \"yes\"\n[theme.custom]\nred = 1\ngreen = \"#000\"\n";
        let theme = resolve(text, || panic!("auto_switch is not a boolean"));
        assert_eq!(theme.palette.red, Palette::nord().red);
        assert_eq!(theme.palette.green, Color::Rgb(0, 0, 0));
        assert!(!theme.terminal);
        assert!(resolve("[theme]\nname = \"terminal\"\n", || true).terminal);
        assert_eq!(resolve("[theme\n", || true).palette, Palette::catppuccin());
    }

    #[test]
    fn auto_switch_takes_the_light_sibling_on_a_light_host() {
        let text = "[theme]\nname = \"gruvbox\"\nauto_switch = true\n[theme.custom.light]\nred = \"blue\"\n";
        assert_eq!(resolve(text, || true).palette, Palette::gruvbox());
        let light = resolve(text, || false).palette;
        assert_eq!(light.text, Palette::gruvbox_light().text);
        assert_eq!(light.red, Color::Blue);
    }

    #[test]
    fn a_color_gives_its_escape_codes_and_its_fzf_color() {
        let purple = Color::Rgb(189, 147, 249);
        assert_eq!(purple.foreground(), "\x1b[38;2;189;147;249m");
        assert_eq!(purple.background(), "\x1b[48;2;189;147;249m");
        assert_eq!(purple.fzf(), "#bd93f9");
        assert_eq!(Color::LightBlue.foreground(), "\x1b[38;5;12m");
        assert_eq!(Color::DarkGray.fzf(), "8");
        assert_eq!(Color::Reset.fzf(), "-1");
        assert_eq!(Color::Reset.background(), "\x1b[49m");
    }

    #[test]
    fn each_theme_of_herdr_has_a_palette() {
        for name in [
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
        ] {
            assert!(Palette::from_name(name).is_some(), "{name}");
        }
    }
}
