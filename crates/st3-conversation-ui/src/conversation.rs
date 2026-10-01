//! A conversation drawn the way pi draws one.
//!
//! Each entry renders to its own lines once and is cached by (id, content, width, options),
//! so a refresh that changes nothing changes no pixel, and a new entry only adds lines at the
//! bottom. The person's own messages are tinted blocks; replies are plain markdown; tool
//! calls are small boxes tinted by outcome and collapsed to their last lines; Small Talk
//! messages sit in the same stream with a bar, not in a separate section.

use super::text::{self, run};
use crate::PaneIntent;
use crate::doc::{Doc, Target};
use crate::style::{RULES, Token};
use crate::theme::{self, Theme};
use crate::{Body, Entry, ToolState};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// The most rows a tool call's output, or mail between others, takes until it is expanded,
/// wrapped lines counted.
const COLLAPSED_TOOL_LINES: usize = RULES.tool.collapsed_rows;

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
        theme: &Theme,
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
                entry.at.hash(&mut hasher);
                theme.hash(&mut hasher);
                entry.body.hash(&mut hasher);
                width.hash(&mut hasher);
                open.hash(&mut hasher);
                if running {
                    spinner.hash(&mut hasher);
                }
                hasher.finish()
            };
            let rendered = cache
                .entry(key)
                .or_insert_with(|| Rc::new(render_entry(entry, width, open, spinner, theme)))
                .clone();
            doc.entries.push((entry.id.clone(), doc.lines.len()));
            doc.append((*rendered).clone(), 0);
        }
        doc
    }
}

fn render_entry(entry: &Entry, width: usize, open: bool, spinner: &str, theme: &Theme) -> Doc {
    let mut doc = Doc::new();
    let inner = width.saturating_sub(2).max(10);
    match &entry.body {
        Body::User(body) => {
            let bg = RULES.user.fill.color(theme);
            let time = format!("{} ", entry.at);
            doc.line(Line::from(vec![
                Span::styled(
                    " ".repeat(width.saturating_sub(text::width(&time))),
                    Style::default().bg(bg),
                ),
                Span::styled(
                    time,
                    Style::default().fg(RULES.user.time.color(theme)).bg(bg),
                ),
            ]));
            doc.lines(
                text::wrap(
                    &text::inline(&text::sanitize(body), fg(RULES.user.text, theme), theme),
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
            for line in text::markdown(body, inner, fg(RULES.assistant, theme), theme) {
                doc.line(indent(line));
            }
        }
        Body::Thinking(body) => {
            let mut style = fg(RULES.thinking.color, theme);
            if RULES.thinking.italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            doc.lines(text::wrap(
                &[run(text::sanitize(body), style)],
                inner,
                &[run(format!(" {} ", RULES.thinking.mark), theme.dim())],
                &[run("   ", theme.dim())],
                None,
            ));
        }
        Body::Tool {
            title,
            state,
            output,
        } => {
            // Quiet until opened: an edge in the outcome's colour, dim text, no fill. Opened,
            // it reads at full brightness.
            let rules = RULES.tool;
            let (glyph, color) = match state {
                ToolState::Running => (spinner, rules.running.color(theme)),
                ToolState::Ok => (rules.ok.text, rules.ok.color.color(theme)),
                ToolState::Failed => (rules.failed.text, rules.failed.color.color(theme)),
            };
            let edge = || run("▎ ", theme::fg(color));
            let look = if open { rules.open } else { rules.quiet };
            let mut title_style = fg(look.title, theme);
            if look.title_bold {
                title_style = title_style.add_modifier(Modifier::BOLD);
            }
            let row_style = fg(look.rows, theme);
            let title = text::truncate(&text::sanitize(title), width.saturating_sub(6));
            doc.targets.push(Target {
                line: 0,
                column: 0,
                width: width as u16,
                hit: PaneIntent::Expand(entry.id.clone()),
            });
            doc.line(Line::from(vec![
                Span::styled(edge().text, edge().style),
                Span::styled(format!("{glyph} "), theme::fg(color)),
                Span::styled(title, title_style),
            ]));
            // Every row the output takes once wrapped; collapsed, a call shows its last few.
            let mut rows = Vec::new();
            for line in output {
                let line = text::sanitize(line);
                let style = if line.starts_with('+') {
                    fg(rules.added, theme)
                } else if line.starts_with('-') || line.contains("error") {
                    fg(rules.removed, theme)
                } else {
                    row_style
                };
                let style = if open {
                    style
                } else {
                    style.add_modifier(Modifier::DIM)
                };
                rows.extend(text::wrap(
                    &[run(line, style)],
                    width,
                    &[edge(), run("  ", style)],
                    &[edge(), run("  ", style)],
                    None,
                ));
            }
            let total = rows.len();
            let hidden = if open {
                0
            } else {
                total.saturating_sub(COLLAPSED_TOOL_LINES)
            };
            let control = |doc: &mut Doc, label: String| {
                fold_control(doc, &entry.id, width, edge(), label, theme)
            };
            if hidden > 0 {
                control(&mut doc, more(hidden));
            }
            // An open long call folds from the same place it opened, as well as from its end.
            if open && total > COLLAPSED_TOOL_LINES {
                control(&mut doc, rules.collapse.text.into());
            }
            doc.lines(rows.into_iter().skip(hidden));
            if open && total > COLLAPSED_TOOL_LINES {
                control(&mut doc, rules.collapse.text.into());
            }
        }
        Body::Mail {
            from,
            to,
            subject,
            body,
            delivered,
        } => {
            // Mail the person is part of leads; mail between others stays in the background
            // and folds like a tool call until opened.
            let rules = RULES.mail;
            let theirs = from == "you" || to == "you";
            let look = if theirs {
                rules.involving_you
            } else {
                rules.between_others
            };
            let bar = run("▎ ", fg(look.edge, theme));
            // The person's own mail says how far it got: ✓ st has it, ✓✓ the agent's harness
            // has it.
            let progress = match (from == "you", delivered) {
                (false, _) => run(String::new(), theme.dim()),
                (true, false) => run(
                    format!("  {}", rules.sent.text),
                    fg(rules.sent.color, theme),
                ),
                (true, true) => run(
                    format!("  {}", rules.delivered.text),
                    fg(rules.delivered.color, theme),
                ),
            };
            // Mail to the person stands out: full brightness on a tinted block, like their own
            // messages.
            let to_you = to == "you";
            let tint = to_you.then_some(rules.to_you_fill.color(theme));
            let on = |style: Style| match tint {
                Some(bg) => style.bg(bg),
                None => style,
            };
            let bar = run(bar.text, on(bar.style));
            let mut from_style = fg(look.from, theme);
            if look.from_bold {
                from_style = from_style.add_modifier(Modifier::BOLD);
            }
            let subject_style = if theirs {
                theme.bold()
            } else {
                fg(look.text, theme)
            };
            let folds = folds(&entry.body);
            if folds {
                doc.targets.push(Target {
                    line: doc.lines.len(),
                    column: 0,
                    width: width as u16,
                    hit: PaneIntent::Expand(entry.id.clone()),
                });
            }
            doc.lines(text::wrap(
                &[
                    run(
                        if to.is_empty() {
                            from.clone()
                        } else {
                            format!("{from} → {to}")
                        },
                        on(from_style),
                    ),
                    run(format!("  {}", text::sanitize(subject)), on(subject_style)),
                    run(format!("  {}", entry.at), on(theme.dim())),
                    run(progress.text, on(progress.style)),
                ],
                inner,
                std::slice::from_ref(&bar),
                std::slice::from_ref(&bar),
                tint,
            ));
            let mut rows = Vec::new();
            for line in text::markdown(
                body,
                inner.saturating_sub(2),
                on(fg(look.text, theme)),
                theme,
            ) {
                let mut spans = vec![Span::styled(bar.text.clone(), bar.style)];
                spans.extend(line.spans.into_iter().map(|span| {
                    let style = on(span.style);
                    span.style(style)
                }));
                let line = Line::from(spans);
                rows.push(match tint {
                    Some(bg) => pad(line, inner, bg),
                    None => line,
                });
            }
            let total = rows.len();
            let long = folds && total > COLLAPSED_TOOL_LINES;
            let shown = if long && !open {
                COLLAPSED_TOOL_LINES
            } else {
                total
            };
            let start = doc.lines.len();
            doc.lines(rows.into_iter().take(shown));
            if entry.id.starts_with("message/") && doc.lines.len() > start {
                doc.messages
                    .push((entry.id.clone(), start..doc.lines.len()));
            }
            if long {
                let label = if open {
                    RULES.tool.collapse.text.to_owned()
                } else {
                    more(total - shown)
                };
                fold_control(&mut doc, &entry.id, width, bar.clone(), label, theme);
            }
        }
        Body::Pending {
            text: body,
            failed,
            unconfirmed,
        } => {
            // Dim until st has it; red if it never got there, peach if st never said.
            let rules = RULES.pending;
            let (bar, label, style) = match (failed, unconfirmed) {
                (None, _) => (
                    rules.sending_edge.color(theme),
                    rules.sending.text.to_owned(),
                    fg(rules.sending.color, theme),
                ),
                (Some(error), true) => (
                    rules.unconfirmed.color(theme),
                    format!(
                        "you · unconfirmed, st did not answer ({error}) · r send again · x clear"
                    ),
                    fg(rules.unconfirmed, theme),
                ),
                (Some(error), false) => (
                    rules.failed.color(theme),
                    format!("you · not sent: {error} · r retry · x clear"),
                    fg(rules.failed, theme),
                ),
            };
            let bar = run("▎ ", theme::fg(bar));
            doc.lines(text::wrap(
                &[
                    run(label, style),
                    run(format!("  {}", entry.at), theme.dim()),
                ],
                inner,
                std::slice::from_ref(&bar),
                std::slice::from_ref(&bar),
                None,
            ));
            for line in text::markdown(body, inner.saturating_sub(2), fg(rules.text, theme), theme)
            {
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
                Span::styled("─".repeat(side.min(6)), fg(RULES.event.rule, theme)),
                Span::styled(label, fg(RULES.event.label, theme)),
            ]));
        }
    }
    doc
}

/// Whether an entry folds until opened: tool calls, and mail the person is not part of.
pub fn folds(body: &Body) -> bool {
    match body {
        Body::Tool { .. } => true,
        Body::Mail { from, to, .. } => from != "you" && to != "you",
        _ => false,
    }
}

fn fg(token: Token, theme: &Theme) -> Style {
    theme::fg(token.color(theme))
}

/// "… 14 more lines · o or click to show"
fn more(hidden: usize) -> String {
    format!("… {hidden} more lines · o or click to show")
}

/// A line that opens or folds an entry when clicked.
fn fold_control(
    doc: &mut Doc,
    id: &str,
    width: usize,
    edge: text::Run,
    label: String,
    theme: &Theme,
) {
    doc.targets.push(Target {
        line: doc.lines.len(),
        column: 0,
        width: width as u16,
        hit: PaneIntent::Expand(id.to_owned()),
    });
    doc.line(Line::from(vec![
        Span::styled(edge.text, edge.style),
        Span::styled(format!("  {label}"), fg(RULES.tool.collapse.color, theme)),
    ]));
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
        let first = cache.render(&entries, 40, &HashSet::new(), "⠋", &crate::tests::theme());
        let again = cache.render(&entries, 40, &HashSet::new(), "⠋", &crate::tests::theme());
        assert_eq!(first.lines, again.lines);
        entries.push(entry("3", Body::Event("step ready".into())));
        let grown = cache.render(&entries, 40, &HashSet::new(), "⠋", &crate::tests::theme());
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
        let doc = cache.render(&entries, 40, &HashSet::new(), "⠋", &crate::tests::theme());
        let text = doc.lines.iter().map(text::plain).collect::<Vec<_>>();
        assert!(text[1].contains("14 more lines"));
        assert_eq!(text.len(), 2 + 6, "a title, the more line, six rows");
        assert!(text.last().unwrap().contains("line 19"));
        // Rows count once wrapped, and a failed call collapses too.
        let wide = vec![entry(
            "w",
            Body::Tool {
                title: "$ test".into(),
                state: ToolState::Failed,
                output: vec!["word ".repeat(80)],
            },
        )];
        let doc = cache.render(&wide, 40, &HashSet::new(), "⠋", &crate::tests::theme());
        let text = doc.lines.iter().map(text::plain).collect::<Vec<_>>();
        assert!(text[1].contains("more lines"), "{text:?}");
        assert_eq!(text.len(), 2 + 6);
        let mut open = HashSet::new();
        open.insert("t".to_owned());
        assert!(
            cache
                .render(&entries, 40, &open, "⠋", &crate::tests::theme())
                .lines
                .len()
                > doc.lines.len()
        );
    }

    #[test]
    fn mail_between_others_folds_like_a_tool_call_and_mail_to_you_never_does() {
        let cache = Cache::default();
        let body = (0..20)
            .map(|n| format!("point {n}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mail = |id: &str, from: &str, to: &str| {
            entry(
                id,
                Body::Mail {
                    from: from.into(),
                    to: to.into(),
                    subject: "notes".into(),
                    body: body.clone(),
                    delivered: false,
                },
            )
        };
        let plain = |doc: &Doc| doc.lines.iter().map(text::plain).collect::<Vec<_>>();
        let others = vec![mail("message/a", "planner", "builder")];
        let folded = cache.render(&others, 60, &HashSet::new(), "⠋", &crate::tests::theme());
        let lines = plain(&folded);
        assert_eq!(lines.len(), 1 + COLLAPSED_TOOL_LINES + 1, "{lines:#?}");
        assert!(lines.last().unwrap().contains("more lines"));
        assert!(
            folded
                .targets
                .iter()
                .all(|target| target.hit == PaneIntent::Expand("message/a".into()))
        );
        let open = HashSet::from(["message/a".to_owned()]);
        let lines = plain(&cache.render(&others, 60, &open, "⠋", &crate::tests::theme()));
        assert!(lines.iter().any(|line| line.contains("point 19")));
        assert!(lines.last().unwrap().contains("▴ collapse"));
        for (from, to) in [("planner", "you"), ("you", "planner")] {
            let doc = cache.render(
                &[mail("message/b", from, to)],
                60,
                &HashSet::new(),
                "⠋",
                &crate::tests::theme(),
            );
            assert!(plain(&doc).iter().any(|line| line.contains("point 19")));
            assert!(doc.targets.is_empty());
        }
    }
}
