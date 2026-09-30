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
