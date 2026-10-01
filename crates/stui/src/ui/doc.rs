//! A detail pane is a document: lines plus the click targets inside them.
//!
//! Building every pane as lines (cards included) means one scroll model, one selection model
//! and one hit map for all of them. A click target is stored against its line, so it still
//! works after the pane scrolls.

use super::text::{self, run};
use super::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Tab(usize),
    Row(usize),
    Key(char),
    Enter,
    Escape,
    ToggleTool(String),
    Pane(st3_conversation_ui::PaneIntent),
    JumpLatest,
    Composer,
    Help,
    Open(String),
    /// Show a popover card for a graph subject.
    Peek(String),
    /// Focus a field of a form.
    Field(usize),
    /// Revoke a paired device (after a confirmation).
    Revoke(String),
    /// Leave the terminal view.
    Detach,
    /// Glasses: open the palette at the glasses section.
    GlassMenu,
    /// Glasses: open the palette at one section.
    PaletteSection(usize),
    /// Glasses: show a group's tab (group 0's tab 0 is Home).
    GlassTab(usize, usize),
    /// Glasses: open the palette for a new tab in a group.
    GlassAdd(usize),
    /// Glasses: open a palette row.
    PaletteChoice(usize),
}

pub type Target = st3_conversation_ui::doc::Target<Hit>;
pub type Doc = st3_conversation_ui::doc::Doc<Hit>;

/// Shell-only cards, graph fields and buttons layered on the shared document.
pub trait DocExt {
    fn field(&mut self, label: &str, value: &str, width: usize, value_style: Style);
    fn section(&mut self, title: &str, count: Option<usize>, width: usize);
    fn buttons(&mut self, buttons: &[(&str, &str, Hit, Color)]);
    fn card(&mut self, title: &str, color: Color, heavy: bool, inner: Doc, width: usize);
}

impl DocExt for Doc {
    /// A "label   value" row with the value wrapped under itself.
    fn field(&mut self, label: &str, value: &str, width: usize, value_style: Style) {
        let label = format!("{label:<10}");
        let indent = " ".repeat(text::width(&label));
        self.lines.extend(text::wrap(
            &text::inline(&text::sanitize(value), value_style),
            width,
            &[run(label, theme::dim())],
            &[run(indent, theme::dim())],
            None,
        ));
    }
    /// A section heading: a small bold label and a rule.
    fn section(&mut self, title: &str, count: Option<usize>, width: usize) {
        let mut spans = vec![Span::styled(title.to_owned(), theme::label())];
        if let Some(count) = count {
            spans.push(Span::styled(format!(" {count}"), theme::dim()));
        }
        let used = spans.iter().map(|span| span.width()).sum::<usize>() + 1;
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "─".repeat(width.saturating_sub(used)),
            theme::fg(theme::SURFACE0),
        ));
        self.lines.push(Line::from(spans));
    }
    /// A row of buttons. Each is `[key label]`, and clicking it presses the key.
    fn buttons(&mut self, buttons: &[(&str, &str, Hit, Color)]) {
        let line = self.lines.len();
        let mut spans = Vec::new();
        let mut column = 0u16;
        for (index, (key, label, hit, color)) in buttons.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
                column += 2;
            }
            let key_text = format!(" {key} ");
            let label_text = format!("{label} ");
            let width = (text::width(&key_text) + text::width(&label_text)) as u16;
            spans.push(Span::styled(
                key_text,
                Style::default()
                    .fg(theme::CRUST)
                    .bg(*color)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                label_text,
                Style::default().fg(*color).bg(theme::SURFACE0),
            ));
            self.targets.push(Target {
                line,
                column,
                width,
                hit: hit.clone(),
            });
            column += width;
        }
        self.lines.push(Line::from(spans));
    }
    /// A card in deck's style: a rounded frame with its title in the top edge. A heavy
    /// card (thick frame, coloured) is reserved for the thing that needs a person.
    fn card(&mut self, title: &str, color: Color, heavy: bool, inner: Doc, width: usize) {
        let width = width.max(8);
        let (tl, tr, bl, br, h, v) = if heavy {
            ("┏", "┓", "┗", "┛", "━", "┃")
        } else {
            ("╭", "╮", "╰", "╯", "─", "│")
        };
        let border = theme::fg(if heavy { color } else { theme::SURFACE1 });
        let title = text::truncate(
            &format!(" {} ", title.to_uppercase()),
            width.saturating_sub(4),
        );
        let fill = width.saturating_sub(2 + 1 + text::width(&title));
        self.lines.push(Line::from(vec![
            Span::styled(format!("{tl}{h}"), border),
            Span::styled(title, theme::strong(color)),
            Span::styled(format!("{}{tr}", h.repeat(fill)), border),
        ]));
        let base = self.lines.len();
        let inner_width = width.saturating_sub(4);
        for target in inner.targets {
            self.targets.push(Target {
                line: base + target.line,
                column: target.column + 2,
                ..target
            });
        }
        for line in inner.lines {
            let used = line.width();
            let mut spans = vec![Span::styled(format!("{v} "), border)];
            spans.extend(line.spans);
            spans.push(Span::raw(" ".repeat(inner_width.saturating_sub(used))));
            spans.push(Span::styled(format!(" {v}"), border));
            self.lines.push(Line::from(spans));
        }
        self.lines.push(Line::from(Span::styled(
            format!("{bl}{}{br}", h.repeat(width.saturating_sub(2))),
            border,
        )));
    }
}

/// A meter of eighth blocks.
pub fn meter(done: usize, total: usize, cells: usize, color: Color) -> Vec<Span<'static>> {
    const EIGHTHS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
    let filled = if total == 0 {
        0.0
    } else {
        done as f32 / total as f32 * cells as f32
    };
    (0..cells)
        .map(|cell| {
            let part = (filled - cell as f32).clamp(0.0, 1.0);
            Span::styled(
                EIGHTHS[(part * 8.0).round() as usize],
                Style::default().fg(color).bg(theme::SURFACE0),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_lines_have_one_width_and_targets_move_inside_the_frame() {
        let mut inner = Doc::new();
        inner.line(Line::from("hello"));
        inner.buttons(&[("a", "Approve", Hit::Key('a'), theme::GREEN)]);
        let mut doc = Doc::new();
        doc.card("review", theme::MAUVE, true, inner, 30);
        assert!(doc.lines.iter().all(|line| line.width() == 30));
        assert_eq!(doc.targets[0].line, 2);
        assert_eq!(doc.targets[0].column, 2);
    }
}
