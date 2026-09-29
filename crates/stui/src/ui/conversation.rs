//! A conversation drawn the way pi draws one.
//!
//! Each entry renders to its own lines once and is cached by (id, content, width, options),
//! so a refresh that changes nothing changes no pixel, and a new entry only adds lines at the
//! bottom. The person's own messages are tinted blocks; replies are plain markdown; tool
//! calls are small boxes tinted by outcome and collapsed to their last lines; Small Talk
//! messages sit in the same stream with a bar, not in a separate section.

use super::doc::{Doc, Hit, Target};
use super::text::{self, run};
use super::theme;
use super::view::{Body, Entry, ToolState};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

const COLLAPSED_TOOL_LINES: usize = 5;

#[derive(Default)]
pub struct Cache {
    entries: RefCell<HashMap<u64, Rc<Doc>>>,
}

impl Cache {
    /// Every line of the conversation at `width`, with the click targets for tool boxes.
    pub fn render(
        &self,
        entries: &[Entry],
        width: usize,
        expanded: &HashSet<String>,
        spinner: &str,
    ) -> Doc {
        let mut doc = Doc::new();
        let mut cache = self.entries.borrow_mut();
        if cache.len() > 4096 {
            cache.clear();
        }
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                doc.blank();
            }
            let open = expanded.contains(&entry.id);
            let running = matches!(
                entry.body,
                Body::Tool {
                    state: ToolState::Running,
                    ..
                }
            );
            let key = {
                let mut hasher = DefaultHasher::new();
                entry.id.hash(&mut hasher);
                format!("{:?}", entry.body).hash(&mut hasher);
                width.hash(&mut hasher);
                open.hash(&mut hasher);
                if running {
                    spinner.hash(&mut hasher);
                }
                hasher.finish()
            };
            let rendered = cache
                .entry(key)
                .or_insert_with(|| Rc::new(render_entry(entry, width, open, spinner)))
                .clone();
            doc.append((*rendered).clone(), 0);
        }
        doc
    }
}

fn render_entry(entry: &Entry, width: usize, open: bool, spinner: &str) -> Doc {
    let mut doc = Doc::new();
    let inner = width.saturating_sub(2).max(10);
    match &entry.body {
        Body::User(body) => {
            let bg = theme::USER_BG;
            let time = format!("{} ", entry.at);
            doc.line(Line::from(vec![
                Span::styled(
                    " ".repeat(width.saturating_sub(text::width(&time))),
                    Style::default().bg(bg),
                ),
                Span::styled(time, Style::default().fg(theme::OVERLAY0).bg(bg)),
            ]));
            doc.lines(
                text::wrap(
                    &text::inline(&text::sanitize(body), theme::text()),
                    width.saturating_sub(1),
                    &[run(" ", Style::default())],
                    &[run(" ", Style::default())],
                    Some(bg),
                )
                .into_iter()
                .map(|line| pad(line, width, bg)),
            );
            doc.line(Line::from(Span::styled(
                " ".repeat(width),
                Style::default().bg(bg),
            )));
        }
        Body::Assistant(body) => {
            for line in text::markdown(body, inner, theme::text()) {
                doc.line(indent(line));
            }
        }
        Body::Thinking(body) => {
            let style = Style::default()
                .fg(theme::OVERLAY1)
                .add_modifier(Modifier::ITALIC);
            doc.lines(text::wrap(
                &[run(text::sanitize(body), style)],
                inner,
                &[run(" ∴ ", theme::dim())],
                &[run("   ", theme::dim())],
                None,
            ));
        }
        Body::Tool {
            title,
            state,
            output,
        } => {
            let (bg, glyph, color) = match state {
                ToolState::Running => (theme::TOOL_BG, spinner, theme::WORKING),
                ToolState::Ok => (theme::TOOL_OK_BG, "✓", theme::GREEN),
                ToolState::Failed => (theme::TOOL_ERR_BG, "✕", theme::RED),
            };
            let title = text::truncate(&text::sanitize(title), width.saturating_sub(6));
            let used = 3 + text::width(&title) + 2;
            doc.targets.push(Target {
                line: 0,
                column: 0,
                width: width as u16,
                hit: Hit::ToggleTool(entry.id.clone()),
            });
            doc.line(Line::from(vec![
                Span::styled(
                    format!(" {glyph} "),
                    Style::default()
                        .fg(color)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    title,
                    Style::default()
                        .fg(theme::TEXT)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " ".repeat(width.saturating_sub(used) + 2),
                    Style::default().bg(bg),
                ),
            ]));
            let shell =
                matches!(state, ToolState::Failed) || output.len() <= COLLAPSED_TOOL_LINES || open;
            let shown: Vec<&String> = if shell {
                output.iter().collect()
            } else {
                output
                    .iter()
                    .skip(output.len() - COLLAPSED_TOOL_LINES)
                    .collect()
            };
            let hidden = output.len() - shown.len();
            if hidden > 0 {
                doc.targets.push(Target {
                    line: doc.lines.len(),
                    column: 0,
                    width: width as u16,
                    hit: Hit::ToggleTool(entry.id.clone()),
                });
                doc.line(pad(
                    Line::from(Span::styled(
                        format!("   … {hidden} earlier lines · o or click to expand"),
                        Style::default().fg(theme::OVERLAY0).bg(bg),
                    )),
                    width,
                    bg,
                ));
            }
            for line in shown {
                let line = text::sanitize(line);
                let style = if line.starts_with('+') {
                    Style::default().fg(theme::GREEN)
                } else if line.starts_with('-') || line.contains("error") {
                    Style::default().fg(theme::RED)
                } else {
                    Style::default().fg(theme::SUBTEXT0)
                };
                doc.lines(text::wrap(
                    &[run(line, style)],
                    width,
                    &[run("   ", style)],
                    &[run("   ", style)],
                    Some(bg),
                ));
            }
            if open && output.len() > COLLAPSED_TOOL_LINES {
                doc.targets.push(Target {
                    line: doc.lines.len(),
                    column: 0,
                    width: width as u16,
                    hit: Hit::ToggleTool(entry.id.clone()),
                });
                doc.line(pad(
                    Line::from(Span::styled(
                        "   collapse",
                        Style::default().fg(theme::OVERLAY0).bg(bg),
                    )),
                    width,
                    bg,
                ));
            }
        }
        Body::Mail {
            from,
            to,
            subject,
            body,
        } => {
            let bar = run("▎ ", theme::fg(theme::SAPPHIRE));
            doc.lines(text::wrap(
                &[
                    run(
                        if to.is_empty() {
                            from.clone()
                        } else {
                            format!("{from} → {to}")
                        },
                        theme::strong(theme::SAPPHIRE),
                    ),
                    run(format!("  {}", text::sanitize(subject)), theme::bold()),
                    run(format!("  {}", entry.at), theme::dim()),
                ],
                inner,
                std::slice::from_ref(&bar),
                std::slice::from_ref(&bar),
                None,
            ));
            for line in text::markdown(body, inner.saturating_sub(2), theme::soft()) {
                let mut spans = vec![Span::styled(bar.text.clone(), bar.style)];
                spans.extend(line.spans);
                doc.line(Line::from(spans));
            }
        }
        Body::Pending { text: body, failed } => {
            // Dim until st has it; red if it never got there.
            let (bar, label, style) = match failed {
                None => (theme::SURFACE2, "you · sending…".to_owned(), theme::dim()),
                Some(error) => (
                    theme::RED,
                    format!("you · not sent: {error}"),
                    theme::fg(theme::RED),
                ),
            };
            let bar = run("▎ ", theme::fg(bar));
            doc.lines(text::wrap(
                &[
                    run(label, style),
                    run(format!("  {}", entry.at), theme::dim()),
                ],
                inner,
                std::slice::from_ref(&bar),
                std::slice::from_ref(&bar),
                None,
            ));
            for line in text::markdown(body, inner.saturating_sub(2), theme::dim()) {
                let mut spans = vec![Span::styled(bar.text.clone(), bar.style)];
                spans.extend(line.spans);
                doc.line(Line::from(spans));
            }
        }
        Body::Event(event) => {
            let label = text::truncate(
                &format!(" {} · {} ", text::sanitize(event), entry.at),
                width.saturating_sub(1),
            );
            let side = width.saturating_sub(text::width(&label)) / 2;
            doc.line(Line::from(vec![
                Span::styled("─".repeat(side.min(6)), theme::fg(theme::SURFACE1)),
                Span::styled(label, theme::dim()),
            ]));
        }
    }
    doc
}

fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(line.spans);
    Line::from(spans)
}

fn pad(line: Line<'static>, width: usize, bg: ratatui::style::Color) -> Line<'static> {
    let used = line.width();
    let mut line = line;
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, body: Body) -> Entry {
        Entry {
            id: id.into(),
            at: "09:00".into(),
            body,
        }
    }

    #[test]
    fn an_unchanged_conversation_renders_identically_and_new_entries_only_append() {
        let cache = Cache::default();
        let mut entries = vec![
            entry("1", Body::User("hello".into())),
            entry("2", Body::Assistant("**hi** there".into())),
        ];
        let first = cache.render(&entries, 40, &HashSet::new(), "⠋");
        let again = cache.render(&entries, 40, &HashSet::new(), "⠋");
        assert_eq!(first.lines, again.lines);
        entries.push(entry("3", Body::Event("step ready".into())));
        let grown = cache.render(&entries, 40, &HashSet::new(), "⠋");
        assert_eq!(&grown.lines[..first.lines.len()], &first.lines[..]);
    }

    #[test]
    fn long_tool_output_collapses_to_its_last_lines() {
        let cache = Cache::default();
        let output = (0..20).map(|n| format!("line {n}")).collect();
        let entries = vec![entry(
            "t",
            Body::Tool {
                title: "$ test".into(),
                state: ToolState::Ok,
                output,
            },
        )];
        let doc = cache.render(&entries, 40, &HashSet::new(), "⠋");
        let text = doc.lines.iter().map(text::plain).collect::<Vec<_>>();
        assert!(text[1].contains("15 earlier lines"));
        assert!(text.last().unwrap().contains("line 19"));
        let mut open = HashSet::new();
        open.insert("t".to_owned());
        assert!(cache.render(&entries, 40, &open, "⠋").lines.len() > doc.lines.len());
    }
}
