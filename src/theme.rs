use crate::config::CustomThemeConfig;
use ratatui::style::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gradient {
    pub start: (u8, u8, u8),
    pub mid: (u8, u8, u8),
    pub end: (u8, u8, u8),
}

pub struct Theme {
    pub name: String,

    // base
    pub main_bg: Color,
    pub main_fg: Color,
    pub title: Color,
    pub hi_fg: Color,
    pub selected_bg: Color,
    pub selected_fg: Color,
    pub inactive_fg: Color,
    pub graph_text: Color,
    pub meter_bg: Color,
    pub proc_misc: Color,
    pub div_line: Color,
    pub session_id: Color,

    // semantic colors
    pub status_fg: Color,
    pub warning_fg: Color,

    // box borders
    pub cpu_box: Color,
    pub mem_box: Color,
    pub net_box: Color,
    pub proc_box: Color,

    // gradients
    pub cpu_grad: Gradient,
    pub proc_grad: Gradient,
    pub used_grad: Gradient,
    pub free_grad: Gradient,
    pub cached_grad: Gradient,
}

impl Default for Theme {
    fn default() -> Self {
        Self::btop()
    }
}

pub const THEME_NAMES: &[&str] = &[
    "btop",
    "dracula",
    "catppuccin",
    "tokyo-night",
    "gruvbox",
    "nord",
    "light",
    "white",
    "high-contrast",
    "protanopia",
    "deuteranopia",
    "tritanopia",
];

impl Theme {
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "btop" => Some(Self::btop()),
            "dracula" => Some(Self::dracula()),
            "catppuccin" => Some(Self::catppuccin()),
            "tokyo-night" => Some(Self::tokyo_night()),
            "gruvbox" => Some(Self::gruvbox()),
            "nord" => Some(Self::nord()),
            "light" => Some(Self::light()),
            "white" => Some(Self::white()),
            "high-contrast" => Some(Self::high_contrast()),
            "protanopia" => Some(Self::protanopia()),
            "deuteranopia" => Some(Self::deuteranopia()),
            "tritanopia" => Some(Self::tritanopia()),
            _ => None,
        }
    }

    pub fn from_config_name(name: &str, custom_themes: &[CustomThemeConfig]) -> Option<Self> {
        Self::by_name(name).or_else(|| custom_theme_by_name(name, custom_themes))
    }

    pub fn available_names(custom_themes: &[CustomThemeConfig]) -> Vec<String> {
        let mut names: Vec<String> = THEME_NAMES.iter().map(|name| (*name).to_string()).collect();
        for custom in custom_themes {
            if !names.iter().any(|name| name == &custom.name) {
                names.push(custom.name.clone());
            }
        }
        names
    }

    /// btop default — exact RGB values from btop_theme.cpp Default_theme
    pub fn btop() -> Self {
        Self {
            name: "btop".to_string(),
            main_bg: Color::Rgb(25, 25, 25),
            main_fg: Color::Rgb(204, 204, 204),
            title: Color::Rgb(238, 238, 238),
            hi_fg: Color::Rgb(181, 64, 64),
            selected_bg: Color::Rgb(106, 47, 47),
            selected_fg: Color::Rgb(238, 238, 238),
            inactive_fg: Color::Rgb(64, 64, 64),
            graph_text: Color::Rgb(96, 96, 96),
            meter_bg: Color::Rgb(64, 64, 64),
            proc_misc: Color::Rgb(13, 231, 86),
            div_line: Color::Rgb(48, 48, 48),
            session_id: Color::Rgb(176, 160, 112),
            status_fg: Color::Rgb(220, 76, 76),
            warning_fg: Color::Rgb(220, 160, 50),
            cpu_box: Color::Rgb(85, 109, 89),
            mem_box: Color::Rgb(108, 108, 75),
            net_box: Color::Rgb(92, 88, 141),
            proc_box: Color::Rgb(128, 82, 82),
            cpu_grad: Gradient {
                start: (119, 202, 155),
                mid: (203, 192, 108),
                end: (220, 76, 76),
            },
            proc_grad: Gradient {
                start: (128, 208, 163),
                mid: (220, 209, 121),
                end: (212, 84, 84),
            },
            used_grad: Gradient {
                start: (89, 43, 38),
                mid: (217, 98, 109),
                end: (255, 71, 105),
            },
            free_grad: Gradient {
                start: (56, 79, 33),
                mid: (181, 230, 133),
                end: (220, 255, 133),
            },
            cached_grad: Gradient {
                start: (22, 51, 80),
                mid: (116, 230, 252),
                end: (38, 197, 255),
            },
        }
    }

    pub fn dracula() -> Self {
        Self {
            name: "dracula".to_string(),
            main_bg: Color::Rgb(40, 42, 54),
            main_fg: Color::Rgb(248, 248, 242),
            title: Color::Rgb(248, 248, 242),
            hi_fg: Color::Rgb(255, 121, 198),
            selected_bg: Color::Rgb(68, 71, 90),
            selected_fg: Color::Rgb(248, 248, 242),
            inactive_fg: Color::Rgb(98, 114, 164),
            graph_text: Color::Rgb(98, 114, 164),
            meter_bg: Color::Rgb(68, 71, 90),
            proc_misc: Color::Rgb(80, 250, 123),
            div_line: Color::Rgb(68, 71, 90),
            session_id: Color::Rgb(241, 250, 140),
            status_fg: Color::Rgb(255, 85, 85),
            warning_fg: Color::Rgb(241, 250, 140),
            cpu_box: Color::Rgb(139, 233, 253),
            mem_box: Color::Rgb(189, 147, 249),
            net_box: Color::Rgb(255, 121, 198),
            proc_box: Color::Rgb(255, 85, 85),
            cpu_grad: Gradient {
                start: (80, 250, 123),
                mid: (241, 250, 140),
                end: (255, 85, 85),
            },
            proc_grad: Gradient {
                start: (80, 250, 123),
                mid: (241, 250, 140),
                end: (255, 85, 85),
            },
            used_grad: Gradient {
                start: (68, 71, 90),
                mid: (255, 121, 198),
                end: (255, 85, 85),
            },
            free_grad: Gradient {
                start: (40, 42, 54),
                mid: (80, 250, 123),
                end: (139, 233, 253),
            },
            cached_grad: Gradient {
                start: (40, 42, 54),
                mid: (139, 233, 253),
                end: (189, 147, 249),
            },
        }
    }

    pub fn catppuccin() -> Self {
        // Catppuccin Mocha palette
        Self {
            name: "catppuccin".to_string(),
            main_bg: Color::Rgb(30, 30, 46),
            main_fg: Color::Rgb(205, 214, 244),
            title: Color::Rgb(205, 214, 244),
            hi_fg: Color::Rgb(243, 139, 168),
            selected_bg: Color::Rgb(49, 50, 68),
            selected_fg: Color::Rgb(205, 214, 244),
            inactive_fg: Color::Rgb(108, 112, 134),
            graph_text: Color::Rgb(147, 153, 178),
            meter_bg: Color::Rgb(49, 50, 68),
            proc_misc: Color::Rgb(166, 227, 161),
            div_line: Color::Rgb(69, 71, 90),
            session_id: Color::Rgb(249, 226, 175),
            status_fg: Color::Rgb(243, 139, 168),
            warning_fg: Color::Rgb(249, 226, 175),
            cpu_box: Color::Rgb(137, 180, 250),
            mem_box: Color::Rgb(203, 166, 247),
            net_box: Color::Rgb(245, 194, 231),
            proc_box: Color::Rgb(242, 205, 205),
            cpu_grad: Gradient {
                start: (166, 227, 161),
                mid: (249, 226, 175),
                end: (243, 139, 168),
            },
            proc_grad: Gradient {
                start: (148, 226, 213),
                mid: (249, 226, 175),
                end: (243, 139, 168),
            },
            used_grad: Gradient {
                start: (49, 50, 68),
                mid: (245, 194, 231),
                end: (243, 139, 168),
            },
            free_grad: Gradient {
                start: (30, 30, 46),
                mid: (166, 227, 161),
                end: (148, 226, 213),
            },
            cached_grad: Gradient {
                start: (30, 30, 46),
                mid: (137, 180, 250),
                end: (203, 166, 247),
            },
        }
    }

    pub fn tokyo_night() -> Self {
        // Tokyo Night — night variant
        Self {
            name: "tokyo-night".to_string(),
            main_bg: Color::Rgb(26, 27, 38),        // bg #1a1b26
            main_fg: Color::Rgb(169, 177, 214),     // fg_dark #a9b1d6
            title: Color::Rgb(192, 202, 245),       // fg #c0caf5
            hi_fg: Color::Rgb(247, 118, 142),       // red #f7768e
            selected_bg: Color::Rgb(41, 46, 66),    // bg_highlight #292e42
            selected_fg: Color::Rgb(192, 202, 245), // fg
            inactive_fg: Color::Rgb(65, 72, 104),   // terminal_black #414868
            graph_text: Color::Rgb(86, 95, 137),    // comment #565f89
            meter_bg: Color::Rgb(59, 66, 97),       // bg_visual #3b4261
            proc_misc: Color::Rgb(158, 206, 106),   // green #9ece6a
            div_line: Color::Rgb(26, 27, 38),       // bg #1a1b26
            session_id: Color::Rgb(224, 175, 104),  // yellow #e0af68
            status_fg: Color::Rgb(247, 118, 142),   // red #f7768e
            warning_fg: Color::Rgb(224, 175, 104),  // yellow #e0af68
            cpu_box: Color::Rgb(125, 207, 255),     // cyan #7dcfff
            mem_box: Color::Rgb(187, 154, 247),     // magenta #bb9af7
            net_box: Color::Rgb(247, 118, 142),     // red #f7768e
            proc_box: Color::Rgb(255, 158, 100),    // orange #ff9e64
            cpu_grad: Gradient {
                start: (158, 206, 106),
                mid: (224, 175, 104),
                end: (247, 118, 142),
            },
            proc_grad: Gradient {
                start: (115, 218, 202),
                mid: (224, 175, 104),
                end: (247, 118, 142),
            },
            used_grad: Gradient {
                start: (41, 46, 66),
                mid: (255, 158, 100),
                end: (247, 118, 142),
            },
            free_grad: Gradient {
                start: (26, 27, 38),
                mid: (158, 206, 106),
                end: (115, 218, 202),
            },
            cached_grad: Gradient {
                start: (26, 27, 38),
                mid: (125, 207, 255),
                end: (187, 154, 247),
            },
        }
    }

    pub fn gruvbox() -> Self {
        // gruvbox dark — bright accent variants for TUI visibility
        Self {
            name: "gruvbox".to_string(),
            main_bg: Color::Rgb(40, 40, 40),        // bg0 #282828
            main_fg: Color::Rgb(235, 219, 178),     // fg1 #ebdbb2
            title: Color::Rgb(251, 241, 199),       // fg0 #fbf1c7
            hi_fg: Color::Rgb(251, 73, 52),         // bright red #fb4934
            selected_bg: Color::Rgb(80, 73, 69),    // bg2 #504945
            selected_fg: Color::Rgb(251, 241, 199), // fg0
            inactive_fg: Color::Rgb(124, 111, 100), // bg4 #7c6f64
            graph_text: Color::Rgb(168, 153, 132),  // fg4 #a89984
            meter_bg: Color::Rgb(60, 56, 54),       // bg1 #3c3836
            proc_misc: Color::Rgb(184, 187, 38),    // bright green #b8bb26
            div_line: Color::Rgb(50, 48, 47),       // bg0_soft #32302f
            session_id: Color::Rgb(250, 189, 47),   // bright yellow #fabd2f
            status_fg: Color::Rgb(251, 73, 52),     // bright red #fb4934
            warning_fg: Color::Rgb(250, 189, 47),   // bright yellow #fabd2f
            cpu_box: Color::Rgb(131, 165, 152),     // bright blue #83a598
            mem_box: Color::Rgb(211, 134, 155),     // bright purple #d3869b
            net_box: Color::Rgb(254, 128, 25),      // bright orange #fe8019
            proc_box: Color::Rgb(251, 73, 52),      // bright red #fb4934
            cpu_grad: Gradient {
                start: (184, 187, 38), // bright green
                mid: (250, 189, 47),   // bright yellow
                end: (251, 73, 52),    // bright red
            },
            proc_grad: Gradient {
                start: (142, 192, 124), // bright aqua #8ec07c
                mid: (250, 189, 47),    // bright yellow
                end: (251, 73, 52),     // bright red
            },
            used_grad: Gradient {
                start: (60, 56, 54), // bg1
                mid: (254, 128, 25), // bright orange
                end: (251, 73, 52),  // bright red
            },
            free_grad: Gradient {
                start: (40, 40, 40),  // bg0
                mid: (184, 187, 38),  // bright green
                end: (142, 192, 124), // bright aqua
            },
            cached_grad: Gradient {
                start: (40, 40, 40),  // bg0
                mid: (131, 165, 152), // bright blue
                end: (211, 134, 155), // bright purple
            },
        }
    }

    pub fn nord() -> Self {
        // Nord — arctic color palette
        Self {
            name: "nord".to_string(),
            main_bg: Color::Rgb(46, 52, 64),        // nord0 #2e3440
            main_fg: Color::Rgb(216, 222, 233),     // nord4 #d8dee9
            title: Color::Rgb(236, 239, 244),       // nord6 #eceff4
            hi_fg: Color::Rgb(191, 97, 106),        // nord11 red #bf616a
            selected_bg: Color::Rgb(67, 76, 94),    // nord2 #434c5e
            selected_fg: Color::Rgb(236, 239, 244), // nord6
            inactive_fg: Color::Rgb(76, 86, 106),   // nord3 #4c566a
            graph_text: Color::Rgb(76, 86, 106),    // nord3 — neutral muted
            meter_bg: Color::Rgb(59, 66, 82),       // nord1 #3b4252
            proc_misc: Color::Rgb(163, 190, 140),   // nord14 green #a3be8c
            div_line: Color::Rgb(46, 52, 64),       // nord0 #2e3440
            session_id: Color::Rgb(235, 203, 139),  // nord13 yellow #ebcb8b
            status_fg: Color::Rgb(191, 97, 106),    // nord11 red #bf616a
            warning_fg: Color::Rgb(235, 203, 139),  // nord13 yellow #ebcb8b
            cpu_box: Color::Rgb(136, 192, 208),     // nord8 #88c0d0
            mem_box: Color::Rgb(180, 142, 173),     // nord15 purple #b48ead
            net_box: Color::Rgb(208, 135, 112),     // nord12 orange #d08770
            proc_box: Color::Rgb(191, 97, 106),     // nord11 red #bf616a
            cpu_grad: Gradient {
                start: (163, 190, 140),
                mid: (235, 203, 139),
                end: (191, 97, 106),
            },
            proc_grad: Gradient {
                start: (143, 188, 187),
                mid: (235, 203, 139),
                end: (191, 97, 106),
            },
            used_grad: Gradient {
                start: (59, 66, 82),
                mid: (208, 135, 112),
                end: (191, 97, 106),
            },
            free_grad: Gradient {
                start: (46, 52, 64),
                mid: (163, 190, 140),
                end: (143, 188, 187),
            },
            cached_grad: Gradient {
                start: (46, 52, 64),
                mid: (136, 192, 208),
                end: (180, 142, 173),
            },
        }
    }

    /// Light theme — Solarized Light palette. Cream background with
    /// muted accents for users on bright terminals.
    pub fn light() -> Self {
        Self {
            name: "light".to_string(),
            main_bg: Color::Rgb(253, 246, 227), // base3 #fdf6e3
            main_fg: Color::Rgb(88, 110, 117),  // base01 #586e75
            title: Color::Rgb(7, 54, 66),       // base02 #073642
            hi_fg: Color::Rgb(203, 75, 22),     // orange #cb4b16
            selected_bg: Color::Rgb(238, 232, 213), // base2 #eee8d5
            selected_fg: Color::Rgb(7, 54, 66), // base02
            inactive_fg: Color::Rgb(147, 161, 161), // base1 #93a1a1
            graph_text: Color::Rgb(131, 148, 150), // base0 #839496
            meter_bg: Color::Rgb(238, 232, 213), // base2
            proc_misc: Color::Rgb(133, 153, 0), // green #859900
            div_line: Color::Rgb(238, 232, 213), // base2
            session_id: Color::Rgb(181, 137, 0), // yellow #b58900
            status_fg: Color::Rgb(220, 50, 47), // red #dc322f
            warning_fg: Color::Rgb(203, 75, 22), // orange
            cpu_box: Color::Rgb(38, 139, 210),  // blue #268bd2
            mem_box: Color::Rgb(108, 113, 196), // violet #6c71c4
            net_box: Color::Rgb(42, 161, 152),  // cyan #2aa198
            proc_box: Color::Rgb(220, 50, 47),  // red
            cpu_grad: Gradient {
                start: (133, 153, 0), // green
                mid: (181, 137, 0),   // yellow
                end: (220, 50, 47),   // red
            },
            proc_grad: Gradient {
                start: (42, 161, 152), // cyan
                mid: (181, 137, 0),    // yellow
                end: (220, 50, 47),    // red
            },
            used_grad: Gradient {
                start: (238, 232, 213), // base2
                mid: (203, 75, 22),     // orange
                end: (220, 50, 47),     // red
            },
            free_grad: Gradient {
                start: (253, 246, 227), // base3
                mid: (133, 153, 0),     // green
                end: (42, 161, 152),    // cyan
            },
            cached_grad: Gradient {
                start: (253, 246, 227), // base3
                mid: (38, 139, 210),    // blue
                end: (108, 113, 196),   // violet
            },
        }
    }

    /// White theme — GitHub Light palette. Pure white background with
    /// crisp accent colors for users on bright terminals.
    pub fn white() -> Self {
        Self {
            name: "white".to_string(),
            main_bg: Color::Rgb(255, 255, 255),     // white
            main_fg: Color::Rgb(31, 35, 40),        // gh fg.default #1f2328
            title: Color::Rgb(0, 0, 0),             // black
            hi_fg: Color::Rgb(207, 34, 46),         // gh red #cf222e
            selected_bg: Color::Rgb(221, 244, 255), // gh accent.subtle #ddf4ff
            selected_fg: Color::Rgb(9, 105, 218),   // gh blue #0969da
            inactive_fg: Color::Rgb(140, 149, 159), // gh fg.muted #8c959f
            graph_text: Color::Rgb(101, 109, 118),  // gh fg.subtle #656d76
            meter_bg: Color::Rgb(234, 238, 242),    // gh canvas.subtle #eaeef2
            proc_misc: Color::Rgb(26, 127, 55),     // gh green #1a7f37
            div_line: Color::Rgb(208, 215, 222),    // gh border.default #d0d7de
            session_id: Color::Rgb(154, 103, 0),    // gh yellow.fg #9a6700
            status_fg: Color::Rgb(207, 34, 46),     // gh red
            warning_fg: Color::Rgb(191, 135, 0),    // gh attention.fg #bf8700
            cpu_box: Color::Rgb(9, 105, 218),       // gh blue
            mem_box: Color::Rgb(130, 80, 223),      // gh purple #8250df
            net_box: Color::Rgb(26, 127, 55),       // gh green
            proc_box: Color::Rgb(207, 34, 46),      // gh red
            cpu_grad: Gradient {
                start: (26, 127, 55), // green
                mid: (191, 135, 0),   // amber
                end: (207, 34, 46),   // red
            },
            proc_grad: Gradient {
                start: (9, 105, 218), // blue
                mid: (191, 135, 0),   // amber
                end: (207, 34, 46),   // red
            },
            used_grad: Gradient {
                start: (234, 238, 242), // canvas.subtle
                mid: (191, 135, 0),     // amber
                end: (207, 34, 46),     // red
            },
            free_grad: Gradient {
                start: (255, 255, 255), // white
                mid: (26, 127, 55),     // green
                end: (9, 105, 218),     // blue
            },
            cached_grad: Gradient {
                start: (255, 255, 255), // white
                mid: (9, 105, 218),     // blue
                end: (130, 80, 223),    // purple
            },
        }
    }

    /// High contrast — maximum luminance separation for low-vision users.
    /// Pure white fg on black, yellow/cyan accents (distinguishable under
    /// any color vision deficiency).
    pub fn high_contrast() -> Self {
        Self {
            name: "high-contrast".to_string(),
            main_bg: Color::Rgb(0, 0, 0),
            main_fg: Color::Rgb(255, 255, 255),
            title: Color::Rgb(255, 255, 255),
            hi_fg: Color::Rgb(255, 255, 0),
            selected_bg: Color::Rgb(255, 255, 0),
            selected_fg: Color::Rgb(0, 0, 0),
            inactive_fg: Color::Rgb(128, 128, 128),
            graph_text: Color::Rgb(192, 192, 192),
            meter_bg: Color::Rgb(64, 64, 64),
            proc_misc: Color::Rgb(0, 255, 255),
            div_line: Color::Rgb(96, 96, 96),
            session_id: Color::Rgb(255, 255, 0),
            status_fg: Color::Rgb(255, 255, 0),
            warning_fg: Color::Rgb(255, 255, 0),
            cpu_box: Color::Rgb(255, 255, 255),
            mem_box: Color::Rgb(255, 255, 255),
            net_box: Color::Rgb(255, 255, 255),
            proc_box: Color::Rgb(255, 255, 255),
            cpu_grad: Gradient {
                start: (0, 255, 255),
                mid: (255, 255, 255),
                end: (255, 255, 0),
            },
            proc_grad: Gradient {
                start: (0, 255, 255),
                mid: (255, 255, 255),
                end: (255, 255, 0),
            },
            used_grad: Gradient {
                start: (32, 32, 32),
                mid: (192, 192, 192),
                end: (255, 255, 0),
            },
            free_grad: Gradient {
                start: (32, 32, 32),
                mid: (192, 192, 192),
                end: (0, 255, 255),
            },
            cached_grad: Gradient {
                start: (32, 32, 32),
                mid: (128, 128, 255),
                end: (255, 255, 255),
            },
        }
    }

    /// Protanopia (red-blind) — IBM colorblind-safe palette.
    /// Avoids red/green confusion. Blue #648FFF, purple #785EF0,
    /// magenta #DC267F, orange #FE6100, yellow #FFB000.
    pub fn protanopia() -> Self {
        Self {
            name: "protanopia".to_string(),
            main_bg: Color::Rgb(20, 20, 32),
            main_fg: Color::Rgb(220, 220, 220),
            title: Color::Rgb(255, 255, 255),
            hi_fg: Color::Rgb(254, 97, 0), // orange
            selected_bg: Color::Rgb(40, 40, 60),
            selected_fg: Color::Rgb(255, 255, 255),
            inactive_fg: Color::Rgb(96, 96, 112),
            graph_text: Color::Rgb(140, 140, 160),
            meter_bg: Color::Rgb(48, 48, 64),
            proc_misc: Color::Rgb(100, 143, 255), // blue
            div_line: Color::Rgb(48, 48, 64),
            session_id: Color::Rgb(255, 176, 0), // yellow
            status_fg: Color::Rgb(254, 97, 0),   // orange (no red)
            warning_fg: Color::Rgb(255, 176, 0), // yellow
            cpu_box: Color::Rgb(100, 143, 255),
            mem_box: Color::Rgb(120, 94, 240),
            net_box: Color::Rgb(220, 38, 127),
            proc_box: Color::Rgb(254, 97, 0),
            cpu_grad: Gradient {
                start: (100, 143, 255),
                mid: (255, 176, 0),
                end: (254, 97, 0),
            },
            proc_grad: Gradient {
                start: (100, 143, 255),
                mid: (255, 176, 0),
                end: (254, 97, 0),
            },
            used_grad: Gradient {
                start: (40, 40, 60),
                mid: (220, 38, 127),
                end: (254, 97, 0),
            },
            free_grad: Gradient {
                start: (20, 20, 40),
                mid: (100, 143, 255),
                end: (255, 176, 0),
            },
            cached_grad: Gradient {
                start: (20, 20, 40),
                mid: (120, 94, 240),
                end: (100, 143, 255),
            },
        }
    }

    /// Deuteranopia (green-blind) — IBM colorblind-safe palette,
    /// biased toward blue/yellow separation which deuteranopes
    /// distinguish most reliably.
    pub fn deuteranopia() -> Self {
        Self {
            name: "deuteranopia".to_string(),
            main_bg: Color::Rgb(18, 24, 40),
            main_fg: Color::Rgb(222, 222, 230),
            title: Color::Rgb(255, 255, 255),
            hi_fg: Color::Rgb(255, 194, 10), // amber
            selected_bg: Color::Rgb(30, 40, 70),
            selected_fg: Color::Rgb(255, 255, 255),
            inactive_fg: Color::Rgb(100, 108, 130),
            graph_text: Color::Rgb(148, 156, 178),
            meter_bg: Color::Rgb(42, 52, 82),
            proc_misc: Color::Rgb(26, 133, 255), // blue
            div_line: Color::Rgb(42, 52, 82),
            session_id: Color::Rgb(255, 194, 10), // amber
            status_fg: Color::Rgb(255, 102, 0),   // orange (not red)
            warning_fg: Color::Rgb(255, 194, 10),
            cpu_box: Color::Rgb(26, 133, 255),
            mem_box: Color::Rgb(156, 106, 222),
            net_box: Color::Rgb(255, 194, 10),
            proc_box: Color::Rgb(255, 102, 0),
            cpu_grad: Gradient {
                start: (26, 133, 255),
                mid: (255, 194, 10),
                end: (255, 102, 0),
            },
            proc_grad: Gradient {
                start: (26, 133, 255),
                mid: (255, 194, 10),
                end: (255, 102, 0),
            },
            used_grad: Gradient {
                start: (30, 40, 70),
                mid: (255, 194, 10),
                end: (255, 102, 0),
            },
            free_grad: Gradient {
                start: (18, 24, 48),
                mid: (26, 133, 255),
                end: (180, 210, 255),
            },
            cached_grad: Gradient {
                start: (18, 24, 48),
                mid: (156, 106, 222),
                end: (26, 133, 255),
            },
        }
    }

    /// Tritanopia (blue-blind) — red/cyan palette avoiding blue/yellow
    /// confusion. Inspired by GitHub's tritanopia-friendly colors.
    pub fn tritanopia() -> Self {
        Self {
            name: "tritanopia".to_string(),
            main_bg: Color::Rgb(24, 20, 22),
            main_fg: Color::Rgb(224, 224, 224),
            title: Color::Rgb(255, 255, 255),
            hi_fg: Color::Rgb(220, 50, 47), // red
            selected_bg: Color::Rgb(64, 32, 40),
            selected_fg: Color::Rgb(255, 255, 255),
            inactive_fg: Color::Rgb(120, 104, 108),
            graph_text: Color::Rgb(168, 152, 156),
            meter_bg: Color::Rgb(60, 40, 48),
            proc_misc: Color::Rgb(64, 196, 208), // cyan
            div_line: Color::Rgb(48, 32, 38),
            session_id: Color::Rgb(255, 140, 144), // pink
            status_fg: Color::Rgb(220, 50, 47),
            warning_fg: Color::Rgb(255, 140, 144),
            cpu_box: Color::Rgb(64, 196, 208),
            mem_box: Color::Rgb(198, 120, 221),
            net_box: Color::Rgb(220, 50, 47),
            proc_box: Color::Rgb(255, 140, 144),
            cpu_grad: Gradient {
                start: (64, 196, 208),
                mid: (255, 140, 144),
                end: (220, 50, 47),
            },
            proc_grad: Gradient {
                start: (64, 196, 208),
                mid: (255, 140, 144),
                end: (220, 50, 47),
            },
            used_grad: Gradient {
                start: (40, 24, 28),
                mid: (255, 140, 144),
                end: (220, 50, 47),
            },
            free_grad: Gradient {
                start: (20, 28, 32),
                mid: (64, 196, 208),
                end: (180, 232, 240),
            },
            cached_grad: Gradient {
                start: (20, 28, 32),
                mid: (198, 120, 221),
                end: (64, 196, 208),
            },
        }
    }
}

pub fn available_theme_names(custom_themes: &[CustomThemeConfig]) -> Vec<String> {
    Theme::available_names(custom_themes)
}

fn custom_theme_by_name(name: &str, custom_themes: &[CustomThemeConfig]) -> Option<Theme> {
    let custom = custom_themes.iter().find(|theme| theme.name == name)?;
    let base_name = custom
        .values
        .get("base")
        .map(|raw| unquote(raw))
        .unwrap_or("btop");
    let mut theme = Theme::by_name(base_name).unwrap_or_default();
    theme.name = custom.name.clone();

    for (key, raw) in &custom.values {
        match key.as_str() {
            "base" => {}
            "main_bg" => set_color(&mut theme.main_bg, raw),
            "main_fg" => set_color(&mut theme.main_fg, raw),
            "title" => set_color(&mut theme.title, raw),
            "hi_fg" => set_color(&mut theme.hi_fg, raw),
            "selected_bg" => set_color(&mut theme.selected_bg, raw),
            "selected_fg" => set_color(&mut theme.selected_fg, raw),
            "inactive_fg" => set_color(&mut theme.inactive_fg, raw),
            "graph_text" => set_color(&mut theme.graph_text, raw),
            "meter_bg" => set_color(&mut theme.meter_bg, raw),
            "proc_misc" => set_color(&mut theme.proc_misc, raw),
            "div_line" => set_color(&mut theme.div_line, raw),
            "session_id" => set_color(&mut theme.session_id, raw),
            "status_fg" => set_color(&mut theme.status_fg, raw),
            "warning_fg" => set_color(&mut theme.warning_fg, raw),
            "cpu_box" => set_color(&mut theme.cpu_box, raw),
            "mem_box" => set_color(&mut theme.mem_box, raw),
            "net_box" => set_color(&mut theme.net_box, raw),
            "proc_box" => set_color(&mut theme.proc_box, raw),
            "cpu_grad" => set_gradient(&mut theme.cpu_grad, raw),
            "proc_grad" => set_gradient(&mut theme.proc_grad, raw),
            "used_grad" => set_gradient(&mut theme.used_grad, raw),
            "free_grad" => set_gradient(&mut theme.free_grad, raw),
            "cached_grad" => set_gradient(&mut theme.cached_grad, raw),
            _ => {}
        }
    }

    Some(theme)
}

fn set_color(target: &mut Color, raw: &str) {
    if let Some((r, g, b)) = parse_rgb(raw) {
        *target = Color::Rgb(r, g, b);
    }
}

fn set_gradient(target: &mut Gradient, raw: &str) {
    if let Some(gradient) = parse_gradient(raw) {
        *target = gradient;
    }
}

fn parse_gradient(raw: &str) -> Option<Gradient> {
    let inner = raw.trim().strip_prefix('[')?.strip_suffix(']')?;
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let start = parse_rgb(parts[0])?;
    let mid = parse_rgb(parts[1])?;
    let end = parse_rgb(parts[2])?;
    Some(Gradient { start, mid, end })
}

fn parse_rgb(raw: &str) -> Option<(u8, u8, u8)> {
    let value = unquote(raw.trim());
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn unquote(raw: &str) -> &str {
    raw.trim().trim_matches('"').trim_matches('\'')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_load() {
        for name in THEME_NAMES {
            assert!(Theme::by_name(name).is_some(), "theme '{}' not found", name);
        }
    }

    #[test]
    fn unknown_returns_none() {
        assert!(Theme::by_name("nonexistent").is_none());
    }

    #[test]
    fn default_is_btop() {
        let t = Theme::default();
        assert_eq!(t.name, "btop");
    }

    #[test]
    fn custom_theme_inherits_base_and_overrides_fields() {
        let custom = CustomThemeConfig {
            name: "midnight".to_string(),
            values: [
                ("base", r#""dracula""#),
                ("main_bg", r##""#010203""##),
                ("cpu_grad", r##"["#000000", "#111111", "#ffffff"]"##),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        };

        let theme = Theme::from_config_name("midnight", &[custom]).unwrap();

        assert_eq!(theme.name, "midnight");
        assert_eq!(theme.main_bg, Color::Rgb(1, 2, 3));
        assert_eq!(theme.main_fg, Theme::dracula().main_fg);
        assert_eq!(theme.cpu_grad.start, (0, 0, 0));
        assert_eq!(theme.cpu_grad.mid, (17, 17, 17));
        assert_eq!(theme.cpu_grad.end, (255, 255, 255));
    }

    #[test]
    fn malformed_custom_theme_values_fall_back_to_base() {
        let base = Theme::dracula();
        let custom = CustomThemeConfig {
            name: "broken".to_string(),
            values: [
                ("base", r#""dracula""#),
                ("main_bg", r#""010203""#),
                ("main_fg", r##""#nothex""##),
                ("cpu_grad", r##"["#000000", "#111111"]"##),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        };

        let theme = Theme::from_config_name("broken", &[custom]).unwrap();

        assert_eq!(theme.name, "broken");
        assert_eq!(theme.main_bg, base.main_bg);
        assert_eq!(theme.main_fg, base.main_fg);
        assert_eq!(theme.cpu_grad, base.cpu_grad);
    }

    #[test]
    fn custom_theme_with_unknown_base_falls_back_to_btop() {
        let custom = CustomThemeConfig {
            name: "unknown-base".to_string(),
            values: [("base", r#""not-a-theme""#)]
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        };

        let theme = Theme::from_config_name("unknown-base", &[custom]).unwrap();

        assert_eq!(theme.name, "unknown-base");
        assert_eq!(theme.main_bg, Theme::btop().main_bg);
    }

    #[test]
    fn built_in_theme_names_are_not_overridden_by_custom_themes() {
        let custom = CustomThemeConfig {
            name: "btop".to_string(),
            values: [("main_bg", r##""#000000""##)]
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        };

        let theme = Theme::from_config_name("btop", &[custom]).unwrap();

        assert_eq!(theme.name, "btop");
        assert_eq!(theme.main_bg, Theme::btop().main_bg);
    }

    #[test]
    fn custom_names_are_available_after_presets() {
        let custom = CustomThemeConfig {
            name: "midnight".to_string(),
            values: Default::default(),
        };

        let names = available_theme_names(&[custom]);

        assert_eq!(names.first().map(String::as_str), Some("btop"));
        assert!(names.iter().any(|name| name == "midnight"));
    }
}
