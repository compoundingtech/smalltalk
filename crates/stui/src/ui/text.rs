//! Shared safe text layout with stui's caller-owned palette.

use ratatui::style::Style;
use ratatui::text::Line;
#[cfg(test)]
pub use st3_conversation_ui::text::plain;
pub use st3_conversation_ui::text::{Run, run, sanitize, truncate, width, wrap};

pub fn inline(text: &str, base: Style) -> Vec<Run> {
    st3_conversation_ui::text::inline(text, base, &super::theme::conversation())
}

pub fn markdown(input: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    st3_conversation_ui::text::markdown(input, width, base, &super::theme::conversation())
}
