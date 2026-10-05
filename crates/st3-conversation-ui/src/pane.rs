use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Application actions requested by a conversation pane, never performed by the renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneIntent {
    Send(String),
    Expand(String),
    Open(String),
    LoadOlder,
    /// Show an image a message carries.
    Image(crate::entry::MailImage),
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PaneState {
    pub top: usize,
    pub follow: bool,
    pub seen: usize,
    pub unseen: usize,
}

impl PaneState {
    pub fn reconcile(&mut self, total: usize, height: usize) {
        let max_top = total.saturating_sub(height);
        if self.follow {
            self.top = max_top;
            self.unseen = 0;
        } else {
            self.top = self.top.min(max_top);
            if total > self.seen {
                self.unseen += total - self.seen;
            }
        }
        self.seen = total;
    }

    pub fn scroll(&mut self, delta: isize, total: usize, height: usize, follow_at_end: bool) {
        let max_top = total.saturating_sub(height);
        self.top = self.top.saturating_add_signed(delta).min(max_top);
        self.follow = self.top >= max_top && follow_at_end;
        if self.follow {
            self.unseen = 0;
        }
    }

    pub fn follow_latest(&mut self) {
        self.follow = true;
        self.unseen = 0;
    }
}

/// Selection coordinates are document lines and display columns, not byte offsets.
#[derive(Clone, Debug)]
pub struct Selection {
    pub pane: String,
    pub anchor: (usize, u16),
    pub head: (usize, u16),
}

/// A displayed line's text and whether wrapping joined it to the previous line.
/// Implement this for an embedding UI's line type; the `ratatui` feature supplies its `Line`
/// implementation. Plain strings start their own lines.
pub trait SelectionLine {
    fn plain_text(&self) -> Cow<'_, str>;

    /// `Some(" ")` for wrapping at a space, `Some("")` inside a word, `None` for a real newline.
    fn continuation(&self) -> Option<&str> {
        None
    }
}

impl SelectionLine for str {
    fn plain_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(self)
    }
}

impl SelectionLine for String {
    fn plain_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(self)
    }
}

impl<T: SelectionLine + ?Sized> SelectionLine for &T {
    fn plain_text(&self) -> Cow<'_, str> {
        (*self).plain_text()
    }

    fn continuation(&self) -> Option<&str> {
        (*self).continuation()
    }
}

impl Selection {
    /// The selected text as the person would paste it: without the edge stui draws beside a
    /// message, the indent its body sits at, or the fences it draws around code.
    pub fn text<L: SelectionLine>(&self, lines: &[L]) -> String {
        let (start, end) = order(self.anchor, self.head);
        let mut out: Vec<String> = Vec::new();
        for line in start.0..=end.0.min(lines.len().saturating_sub(1)) {
            let Some(line_text) = lines.get(line) else {
                break;
            };
            let plain = line_text.plain_text();
            let from = if line == start.0 { start.1 as usize } else { 0 };
            let to = if line == end.0 {
                end.1 as usize + 1
            } else {
                usize::MAX
            };
            let piece = columns(&plain, from.max(edge(&plain)), to);
            if fence(&piece) {
                continue;
            }
            let piece = piece.trim_end();
            // A line drawn only because the one before it wrapped is copied onto that line: the
            // clipboard gets the real newlines, not the pane's (Nathan, 2026-10-03).
            match (line_text.continuation(), out.last_mut()) {
                (Some(join), Some(previous)) if line > start.0 => {
                    previous.push_str(join);
                    previous.push_str(piece.trim_start());
                }
                _ => out.push(piece.to_owned()),
            }
        }
        dedent(out).join("\n")
    }
}

/// The display columns a message's edge ("▎ ", or "● " on the first row of mail to the person,
/// after any indent) takes at a line's start.
fn edge(line: &str) -> usize {
    let indent = line.len() - line.trim_start_matches(' ').len();
    match line[indent..].strip_prefix(['▎', '●']) {
        Some(rest) if rest.is_empty() || rest.starts_with(' ') => indent + 2,
        _ => 0,
    }
}

/// A code fence the renderer drew: "```" and an optional language, nothing else.
fn fence(line: &str) -> bool {
    line.trim()
        .strip_prefix("```")
        .is_some_and(|language| !language.contains(char::is_whitespace))
}

/// Remove the indent every non-empty line shares.
fn dedent(lines: Vec<String>) -> Vec<String> {
    let indent = lines
        .iter()
        .filter(|line| !line.is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    lines
        .into_iter()
        .map(|line| line.get(indent..).unwrap_or_default().to_owned())
        .collect()
}

/// Keep one state per embedding surface; keys retain scroll and drafts across pane switches.
#[derive(Default)]
pub struct State {
    pub panes: RefCell<HashMap<String, PaneState>>,
    pub expanded: HashSet<String>,
    pub drafts: HashMap<String, String>,
    pub selection: Option<Selection>,
}

impl State {
    pub fn expand(&mut self, id: String) -> PaneIntent {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id.clone());
        }
        PaneIntent::Expand(id)
    }

    /// Does not clear the draft: the caller clears it only after accepting the send intent.
    pub fn send(&self, key: &str) -> Option<PaneIntent> {
        self.drafts
            .get(key)
            .filter(|draft| !draft.trim().is_empty())
            .map(|draft| PaneIntent::Send(draft.clone()))
    }

    pub fn load_older(&self, has_more: bool) -> Option<PaneIntent> {
        has_more.then_some(PaneIntent::LoadOlder)
    }
}

pub fn order(a: (usize, u16), b: (usize, u16)) -> ((usize, u16), (usize, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The part of `line` between two display columns.
pub fn columns(line: &str, from: usize, to: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut column = 0;
    let mut out = String::new();
    for character in line.chars() {
        let width = character.width().unwrap_or(0);
        if column >= from && column < to {
            out.push(character);
        }
        column += width;
    }
    out
}
