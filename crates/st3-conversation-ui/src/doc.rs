use crate::text::{self, Run};
use ratatui::text::{Line, Span};

#[derive(Clone, Debug)]
pub struct Target<H = crate::PaneIntent> {
    pub line: usize,
    pub column: u16,
    pub width: u16,
    pub hit: H,
}

#[derive(Clone, Debug)]
pub struct Doc<H = crate::PaneIntent> {
    pub lines: Vec<Line<'static>>,
    pub targets: Vec<Target<H>>,
    /// Canonical messages whose body occupies these document lines. A client acknowledges only
    /// spans that intersect its rendered viewport, never a fetched page or a message header.
    pub messages: Vec<(String, std::ops::Range<usize>)>,
    /// The line each conversation entry starts on, so a pane read in the middle can keep its
    /// place by entry while lines are added or removed above it.
    pub entries: Vec<(String, usize)>,
}

impl<H> Doc<H> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn line(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }
    pub fn blank(&mut self) {
        self.lines.push(Line::default());
    }
    pub fn lines(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.lines.extend(lines);
    }
    pub fn wrap(&mut self, runs: &[Run], width: usize) {
        self.lines.extend(text::wrap(runs, width, &[], &[], None));
    }
    /// Append another document, shifting its lines and targets.
    pub fn append(&mut self, other: Doc<H>, indent: u16) {
        let base = self.lines.len();
        self.messages.extend(
            other
                .messages
                .into_iter()
                .map(|(id, range)| (id, range.start + base..range.end + base)),
        );
        self.entries.extend(
            other
                .entries
                .into_iter()
                .map(|(id, line)| (id, line + base)),
        );
        for target in other.targets {
            self.targets.push(Target {
                line: base + target.line,
                column: target.column + indent,
                ..target
            });
        }
        if indent == 0 {
            self.lines.extend(other.lines);
        } else {
            let pad = " ".repeat(indent as usize);
            self.lines.extend(other.lines.into_iter().map(|line| {
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(line.spans);
                Line::from(spans)
            }));
        }
    }
}

impl<H> Default for Doc<H> {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            targets: Vec::new(),
            messages: Vec::new(),
            entries: Vec::new(),
        }
    }
}
