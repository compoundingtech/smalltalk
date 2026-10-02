use ratatui::style::{Color, Modifier, Style};

/// Conversation and Markdown colors supplied by the embedding application.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Theme {
    pub user_bg: Color,
    pub tool_bg: Color,
    pub tool_ok_bg: Color,
    pub tool_err_bg: Color,
    pub overlay0: Color,
    pub overlay1: Color,
    pub text: Color,
    pub subtext0: Color,
    pub subtext1: Color,
    pub working: Color,
    pub green: Color,
    pub red: Color,
    pub sapphire: Color,
    pub surface1: Color,
    pub surface2: Color,
    pub teal: Color,
    pub blue: Color,
    pub peach: Color,
    pub lavender: Color,
}

/// stui's palette (Catppuccin Mocha), for an embedder that has none of its own, such as the
/// `st` CLI.
impl Default for Theme {
    fn default() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Self {
            user_bg: rgb(0x28, 0x29, 0x3d),
            tool_bg: rgb(0x23, 0x24, 0x36),
            tool_ok_bg: rgb(0x1f, 0x2b, 0x2a),
            tool_err_bg: rgb(0x33, 0x22, 0x2c),
            overlay0: rgb(0x6c, 0x70, 0x86),
            overlay1: rgb(0x7f, 0x84, 0x9c),
            text: rgb(0xcd, 0xd6, 0xf4),
            subtext0: rgb(0xa6, 0xad, 0xc8),
            subtext1: rgb(0xba, 0xc2, 0xde),
            working: rgb(0xfa, 0xb3, 0x87),
            green: rgb(0xa6, 0xe3, 0xa1),
            red: rgb(0xf3, 0x8b, 0xa8),
            sapphire: rgb(0x74, 0xc7, 0xec),
            surface1: rgb(0x45, 0x47, 0x5a),
            surface2: rgb(0x58, 0x5b, 0x70),
            teal: rgb(0x94, 0xe2, 0xd5),
            blue: rgb(0x89, 0xb4, 0xfa),
            peach: rgb(0xfa, 0xb3, 0x87),
            lavender: rgb(0xb4, 0xbe, 0xfe),
        }
    }
}

impl Theme {
    pub fn text(self) -> Style {
        Style::default().fg(self.text)
    }
    pub fn dim(self) -> Style {
        Style::default().fg(self.overlay0)
    }
    pub fn soft(self) -> Style {
        Style::default().fg(self.subtext0)
    }
    pub fn bold(self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }
}

pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}
pub fn strong(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}
