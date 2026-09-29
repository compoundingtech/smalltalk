//! One palette, named by meaning. Screens ask for `theme::WORKING`, never for a raw colour.
//!
//! The values are Catppuccin Mocha. Mauve is reserved for "a person is needed": a gate, a
//! review, a decision. Nothing else may use it, so it always means the same thing.

use ratatui::style::{Color, Modifier, Style};

pub const BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
pub const MANTLE: Color = Color::Rgb(0x18, 0x18, 0x25);
pub const CRUST: Color = Color::Rgb(0x11, 0x11, 0x1b);
pub const SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
pub const SURFACE1: Color = Color::Rgb(0x45, 0x47, 0x5a);
pub const SURFACE2: Color = Color::Rgb(0x58, 0x5b, 0x70);
pub const OVERLAY0: Color = Color::Rgb(0x6c, 0x70, 0x86);
pub const OVERLAY1: Color = Color::Rgb(0x7f, 0x84, 0x9c);
pub const SUBTEXT0: Color = Color::Rgb(0xa6, 0xad, 0xc8);
pub const SUBTEXT1: Color = Color::Rgb(0xba, 0xc2, 0xde);
pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
pub const LAVENDER: Color = Color::Rgb(0xb4, 0xbe, 0xfe);
pub const BLUE: Color = Color::Rgb(0x89, 0xb4, 0xfa);
pub const SAPPHIRE: Color = Color::Rgb(0x74, 0xc7, 0xec);
pub const TEAL: Color = Color::Rgb(0x94, 0xe2, 0xd5);
pub const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
pub const PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
pub const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
pub const MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);

/// The accent: focus, the selected tab, links, keys in hints.
pub const ACCENT: Color = BLUE;
/// A person is needed. Reserved.
pub const PERSON: Color = MAUVE;
pub const WORKING: Color = PEACH;
pub const IDLE: Color = GREEN;
pub const DONE: Color = TEAL;
pub const FAULT: Color = RED;
pub const WAITING: Color = YELLOW;
pub const QUIET: Color = OVERLAY0;

/// Row backgrounds.
pub const ROW_SELECTED: Color = SURFACE0;
/// The user's own messages in a conversation.
pub const USER_BG: Color = Color::Rgb(0x28, 0x29, 0x3d);
pub const TOOL_BG: Color = Color::Rgb(0x23, 0x24, 0x36);
pub const TOOL_OK_BG: Color = Color::Rgb(0x1f, 0x2b, 0x2a);
pub const TOOL_ERR_BG: Color = Color::Rgb(0x33, 0x22, 0x2c);
pub const SELECTION_BG: Color = Color::Rgb(0x45, 0x47, 0x6a);

pub fn text() -> Style {
    Style::default().fg(TEXT)
}
pub fn dim() -> Style {
    Style::default().fg(OVERLAY0)
}
pub fn soft() -> Style {
    Style::default().fg(SUBTEXT0)
}
pub fn bold() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}
pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}
pub fn strong(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}
/// A section label: small, bold, dim, lowercase.
pub fn label() -> Style {
    Style::default().fg(OVERLAY1).add_modifier(Modifier::BOLD)
}
