use crate::text;
use ratatui::text::Line;
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

impl Selection {
    pub fn text(&self, lines: &[Line<'_>]) -> String {
        let (start, end) = order(self.anchor, self.head);
        let mut out = Vec::new();
        for line in start.0..=end.0.min(lines.len().saturating_sub(1)) {
            let Some(line_text) = lines.get(line) else {
                break;
            };
            let plain = text::plain(line_text);
            let from = if line == start.0 { start.1 as usize } else { 0 };
            let to = if line == end.0 {
                end.1 as usize + 1
            } else {
                usize::MAX
            };
            out.push(columns(&plain, from, to).trim_end().to_owned());
        }
        out.join("\n")
    }
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
