//! Rendered lines as terminal text, for an embedder that prints rather than draws (the `st`
//! CLI): ANSI colour escapes when asked, plain text otherwise.

use ratatui::style::{Color, Modifier};
use ratatui::text::Line;

/// One rendered line, with its colours and weight as ANSI escapes when `color`.
pub fn line(line: &Line<'_>, color: bool) -> String {
    let mut text = String::new();
    for span in &line.spans {
        if !color {
            text.push_str(&span.content);
            continue;
        }
        let style = line.style.patch(span.style);
        let mut codes = Vec::new();
        for (modifier, code) in [
            (Modifier::BOLD, "1"),
            (Modifier::DIM, "2"),
            (Modifier::ITALIC, "3"),
            (Modifier::UNDERLINED, "4"),
        ] {
            if style.add_modifier.contains(modifier) {
                codes.push(code.to_owned());
            }
        }
        if let Some(Color::Rgb(r, g, b)) = style.fg {
            codes.push(format!("38;2;{r};{g};{b}"));
        }
        if let Some(Color::Rgb(r, g, b)) = style.bg {
            codes.push(format!("48;2;{r};{g};{b}"));
        }
        if codes.is_empty() {
            text.push_str(&span.content);
        } else {
            text.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.content));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use ratatui::text::Span;

    #[test]
    fn colour_is_escaped_only_when_asked() {
        let rendered = Line::from(vec![
            Span::styled(
                "tool",
                Style::default()
                    .fg(Color::Rgb(1, 2, 3))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ran"),
        ]);
        assert_eq!(line(&rendered, false), "tool ran");
        assert_eq!(line(&rendered, true), "\x1b[1;38;2;1;2;3mtool\x1b[0m ran");
    }
}
