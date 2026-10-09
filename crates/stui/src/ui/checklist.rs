//! First-run setup as one screen: a welcome, two text fields, a few checkboxes, a row for every
//! supported harness, one consent line and one action.
//!
//! The screen holds no setup logic. It shows a [`Form`], lets the person change it, hands the
//! result to a worker (a closure the caller supplies, which runs on its own thread so the screen
//! keeps drawing) and shows what the worker reports, one result per row. A row ends ready, needs a
//! sign-in or failed, with the whole error text on screen; one failed row never erases another's
//! result, and a retry runs only the rows that did not end ready.
//!
//! Four facts stay apart for every harness: whether its executable was found, whether it is
//! selected, whether st's integration is ready, and whether the person is signed in. A fact the
//! screen cannot know says so.

use super::{Guard, theme};
use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// The one sentence that authorises the setup shown on screen. Login, folder trust and
/// permission mode are separate choices and are not part of it.
pub const CONSENT: &str = "Set up st for the selected agents, adding its local integrations and required workspace configuration?";

/// A fact that may not be known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

/// What is true of one harness before setup runs, kept as four separate facts (the third and
/// fourth here; found and selected are on the row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// Where its executable was found; `None` when it was not.
    pub found: Option<String>,
    /// Whether st's integration for it is in place.
    pub integration: Tri,
    /// Whether the person is signed in to it.
    pub signed_in: Tri,
}

/// How one row ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready(String),
    NeedsLogin(String),
    Failed(String),
}

impl Outcome {
    fn ready(&self) -> bool {
        matches!(self, Outcome::Ready(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The harness name st uses: `claude`, `codex`, `opencode`, `pi`, `omp`.
    pub id: String,
    pub name: String,
    pub facts: Facts,
    /// What setup does for this harness and the exact local paths it touches.
    pub sets_up: String,
    /// What to do when it is not installed.
    pub install_hint: String,
    pub selected: bool,
    pub outcome: Option<Outcome>,
}

/// How far a Codex seat may go without asking, from least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Codex asks, as it does by default.
    Ask,
    /// Writes only inside the workspace; anything else fails instead of asking.
    Workspace,
    /// No sandbox and no questions.
    Full,
}

impl Access {
    /// The value stored as `codex_access`.
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Ask => "ask",
            Access::Workspace => "workspace",
            Access::Full => "full",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub name: String,
    pub machine: String,
    /// "Start st when I log in", pre-checked, with the platform's wording.
    pub login_label: String,
    pub login: bool,
    /// What it installs and how to remove it.
    pub login_note: String,
    /// "Keep running after I log out", offered only where that can be enabled without sudo.
    pub linger: Option<bool>,
    pub rows: Vec<Row>,
    /// The Codex control; present only when Codex was found. `Ask` is an unticked box.
    pub codex: Option<Access>,
}

/// One text field's name, for validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Machine,
}

/// What the worker may tell the screen.
#[derive(Clone)]
pub struct Reporter {
    events: Sender<Event_>,
}

enum Event_ {
    Step(String),
    Row(String, Outcome),
    Facts(String, Facts),
    Handover(String, Sender<()>),
    Done(Result<(), String>),
}

impl Reporter {
    /// The step under way, shown at the foot of the screen.
    pub fn step(&self, text: impl Into<String>) {
        let _ = self.events.send(Event_::Step(text.into()));
    }

    /// A row's result.
    pub fn row(&self, id: &str, outcome: Outcome) {
        let _ = self.events.send(Event_::Row(id.into(), outcome));
    }

    /// A row's facts, read again.
    pub fn facts(&self, id: &str, facts: Facts) {
        let _ = self.events.send(Event_::Facts(id.into(), facts));
    }

    /// Hand the terminal over so the person can finish a harness's own first run, and wait
    /// until they are back.
    pub fn handover(&self, id: &str) {
        let (reply, back) = mpsc::channel();
        if self.events.send(Event_::Handover(id.into(), reply)).is_ok() {
            let _ = back.recv();
        }
    }
}

/// What the person asked the worker to do.
pub struct Run {
    pub form: Form,
    /// Set up st itself and nothing for any agent.
    pub skip_agents: bool,
    /// Rows to set up this time; a retry names only the rows that did not end ready.
    pub only: Option<Vec<String>>,
}

/// How the screen ended.
pub struct Finished {
    pub form: Form,
    /// The person closed it without setting anything up.
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Name,
    Machine,
    Login,
    Linger,
    Row(usize),
    CodexAsk,
    CodexLevel(Access),
    SetUp,
    Skip,
    Retry,
    Continue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Editing,
    Running,
    /// Setup ran; the rows show their results. `failed` is an error from st itself.
    Done {
        failed: Option<String>,
    },
}

/// The first and last line an item covers, and the columns when two share a line.
type Mark = (usize, Item, usize, Option<(u16, u16)>);

/// The screen's state apart from the terminal.
pub struct Screen {
    form: Form,
    phase: Phase,
    focus: Item,
    step: String,
    /// The text field problems the validator reported, by field.
    problems: [Option<String>; 2],
    started: Instant,
    /// Set when the person pressed Enter on Continue or setup ended and nothing needs them.
    leave: bool,
    /// The person chose to set up st and no agent.
    skipped: bool,
    cancelled: bool,
    /// Where the visible part of the screen starts, kept so scrolling is stable.
    top: usize,
    /// What a click would hit, as drawn last: the item and the screen rows it covers.
    hits: Vec<(Rect, Item)>,
}

enum Reaction {
    None,
    /// Look for the agents again.
    Rescan,
    Start { skip_agents: bool, only: Option<Vec<String>> },
    Leave,
}

impl Screen {
    pub fn new(form: Form) -> Self {
        Self {
            form,
            phase: Phase::Editing,
            focus: Item::Name,
            step: String::new(),
            problems: [None, None],
            started: Instant::now(),
            leave: false,
            skipped: false,
            cancelled: false,
            top: 0,
            hits: Vec::new(),
        }
    }

    fn items(&self) -> Vec<Item> {
        match &self.phase {
            Phase::Running => Vec::new(),
            Phase::Done { .. } => {
                let mut items = Vec::new();
                if self.retryable().next().is_some() {
                    items.push(Item::Retry);
                }
                items.push(Item::Continue);
                items
            }
            Phase::Editing => {
                let mut items = vec![Item::Name, Item::Machine, Item::Login];
                if self.form.linger.is_some() {
                    items.push(Item::Linger);
                }
                for (index, row) in self.form.rows.iter().enumerate() {
                    if row.facts.found.is_some() {
                        items.push(Item::Row(index));
                    }
                }
                if let Some(access) = self.form.codex {
                    items.push(Item::CodexAsk);
                    if access != Access::Ask {
                        items.push(Item::CodexLevel(Access::Workspace));
                        items.push(Item::CodexLevel(Access::Full));
                    }
                }
                items.push(Item::SetUp);
                items.push(Item::Skip);
                items
            }
        }
    }

    /// The selected rows that did not end ready.
    fn retryable(&self) -> impl Iterator<Item = &Row> {
        self.form.rows.iter().filter(|row| {
            !self.skipped && row.selected && !row.outcome.as_ref().is_some_and(Outcome::ready)
        })
    }

    fn move_focus(&mut self, forward: bool) {
        let items = self.items();
        if items.is_empty() {
            return;
        }
        let at = items.iter().position(|item| *item == self.focus);
        let next = match (at, forward) {
            (Some(at), true) => (at + 1) % items.len(),
            (Some(at), false) => (at + items.len() - 1) % items.len(),
            (None, _) => 0,
        };
        self.focus = items[next];
    }

    fn field(&mut self) -> Option<&mut String> {
        match self.focus {
            Item::Name => Some(&mut self.form.name),
            Item::Machine => Some(&mut self.form.machine),
            _ => None,
        }
    }

    /// Check the text fields with the caller's rules; true when both are good.
    pub fn validate(&mut self, check: &dyn Fn(Field, &str) -> Result<(), String>) -> bool {
        self.problems = [
            check(Field::Name, &self.form.name).err(),
            check(Field::Machine, &self.form.machine).err(),
        ];
        self.problems.iter().all(Option::is_none)
    }

    fn toggle(&mut self, item: Item) {
        match item {
            Item::Login => self.form.login = !self.form.login,
            Item::Linger => self.form.linger = self.form.linger.map(|on| !on),
            Item::Row(index) => {
                if let Some(row) = self.form.rows.get_mut(index) {
                    row.selected = !row.selected;
                }
            }
            Item::CodexAsk => {
                self.form.codex = self.form.codex.map(|access| match access {
                    Access::Ask => Access::Workspace,
                    _ => Access::Ask,
                });
            }
            Item::CodexLevel(level) => {
                if self.form.codex.is_some() {
                    self.form.codex = Some(level);
                }
            }
            _ => {}
        }
    }

    fn press(
        &mut self,
        item: Item,
        check: &dyn Fn(Field, &str) -> Result<(), String>,
    ) -> Reaction {
        match item {
            Item::SetUp | Item::Skip => {
                if !self.validate(check) {
                    self.focus = if self.problems[0].is_some() {
                        Item::Name
                    } else {
                        Item::Machine
                    };
                    return Reaction::None;
                }
                Reaction::Start {
                    skip_agents: item == Item::Skip,
                    only: None,
                }
            }
            Item::Retry => Reaction::Start {
                skip_agents: false,
                only: Some(self.retryable().map(|row| row.id.clone()).collect()),
            },
            Item::Continue => Reaction::Leave,
            other => {
                self.toggle(other);
                Reaction::None
            }
        }
    }

    fn key(
        &mut self,
        key: KeyEvent,
        check: &dyn Fn(Field, &str) -> Result<(), String>,
    ) -> Reaction {
        if key.kind == KeyEventKind::Release {
            return Reaction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            if self.phase == Phase::Running {
                return Reaction::None;
            }
            self.cancelled = !matches!(self.phase, Phase::Done { .. });
            return Reaction::Leave;
        }
        if self.phase == Phase::Running {
            return Reaction::None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => self.move_focus(true),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(false),
            KeyCode::Esc => {
                // Before setup runs, Esc leaves without setting anything up; after it, it moves on.
                self.cancelled = matches!(self.phase, Phase::Editing);
                return Reaction::Leave;
            }
            KeyCode::Char('r') | KeyCode::Char('R')
                if !ctrl && self.field().is_none() && matches!(self.phase, Phase::Done { .. }) =>
            {
                if self.retryable().next().is_some() {
                    return self.press(Item::Retry, check);
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R')
                if !ctrl && self.field().is_none() && matches!(self.phase, Phase::Editing) =>
            {
                return Reaction::Rescan;
            }
            KeyCode::Char(' ') if self.field().is_none() => {
                let focus = self.focus;
                return self.press(focus, check);
            }
            KeyCode::Enter => {
                let focus = self.focus;
                if matches!(focus, Item::Name | Item::Machine) {
                    self.move_focus(true);
                    return Reaction::None;
                }
                return self.press(focus, check);
            }
            KeyCode::Backspace => {
                if let Some(field) = self.field() {
                    field.pop();
                    self.problems = [None, None];
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(field) = self.field() {
                    field.clear();
                    self.problems = [None, None];
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(field) = self.field() {
                    if field.len() < 64 {
                        field.push(c);
                    }
                    self.problems = [None, None];
                }
            }
            _ => {}
        }
        Reaction::None
    }

    fn click(
        &mut self,
        mouse: MouseEvent,
        check: &dyn Fn(Field, &str) -> Result<(), String>,
    ) -> Reaction {
        if self.phase == Phase::Running || mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Reaction::None;
        }
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|(rect, _)| {
                mouse.column >= rect.x
                    && mouse.column < rect.x + rect.width
                    && mouse.row >= rect.y
                    && mouse.row < rect.y + rect.height
            })
            .map(|(_, item)| *item);
        match hit {
            Some(item) if matches!(item, Item::Name | Item::Machine) => {
                self.focus = item;
                Reaction::None
            }
            Some(item) => {
                self.focus = item;
                self.press(item, check)
            }
            None => Reaction::None,
        }
    }

    fn event(&mut self, event: Event_) {
        match event {
            Event_::Step(text) => self.step = text,
            Event_::Row(id, outcome) => {
                if let Some(row) = self.form.rows.iter_mut().find(|row| row.id == id) {
                    row.outcome = Some(outcome);
                }
            }
            Event_::Facts(id, facts) => {
                if let Some(row) = self.form.rows.iter_mut().find(|row| row.id == id) {
                    row.facts = facts;
                }
            }
            Event_::Handover(..) => {}
            Event_::Done(result) => {
                self.step.clear();
                match result {
                    Ok(()) => {
                        let clean = self.retryable().next().is_none();
                        self.phase = Phase::Done { failed: None };
                        self.focus = if self.retryable().next().is_some() {
                            Item::Retry
                        } else {
                            Item::Continue
                        };
                        // Nothing needs the person: show the results for a moment, then go on.
                        self.leave = clean;
                    }
                    Err(error) => {
                        // st itself could not be set up: back to the form, with the reason.
                        self.phase = Phase::Editing;
                        self.focus = Item::SetUp;
                        self.problems = [None, None];
                        self.step = error;
                    }
                }
            }
        }
    }

    // ---- drawing -----------------------------------------------------------------------

    pub fn draw(&mut self, area: Rect, buffer: &mut Buffer) {
        buffer.set_style(area, Style::default().bg(theme::BASE).fg(theme::TEXT));
        let width = area.width.min(100).saturating_sub(4).max(20) as usize;
        let left = area.x + area.width.saturating_sub(width as u16) / 2;
        let (lines, marks) = self.compose(width);
        // Scroll so the focused item's first line is on screen, with the footer's row spared.
        let body = area.height.saturating_sub(2) as usize;
        let focus_line = marks
            .iter()
            .find(|(_, item, _, _)| *item == self.focus)
            .map(|(first, _, last, _)| (*first, *last));
        if let Some((first, last)) = focus_line {
            if first < self.top {
                // Back at the top of the form, show its heading too.
                self.top = if last < body { 0 } else { first };
            } else if last >= self.top + body {
                self.top = (last + 1).saturating_sub(body);
            }
        }
        self.top = self.top.min(lines.len().saturating_sub(body));
        self.hits.clear();
        for (row, line) in lines.iter().skip(self.top).take(body).enumerate() {
            buffer.set_line(left, area.y + 1 + row as u16, line, width as u16);
        }
        for (first, item, last, columns) in &marks {
            let from = (*first).max(self.top);
            let to = (*last + 1).min(self.top + body);
            if from < to {
                let (offset, across) = columns.unwrap_or((0, width as u16));
                self.hits.push((
                    Rect {
                        x: left + offset,
                        y: area.y + 1 + (from - self.top) as u16,
                        width: across,
                        height: (to - from) as u16,
                    },
                    *item,
                ));
            }
        }
        let (hint, style) = self.footer();
        buffer.set_stringn(
            area.x + 1,
            area.y + area.height.saturating_sub(1),
            hint,
            area.width.saturating_sub(2) as usize,
            style,
        );
    }

    fn footer(&self) -> (String, Style) {
        match &self.phase {
            Phase::Running => (
                format!(
                    "{} {}",
                    SPINNER[(self.started.elapsed().as_millis() / 100) as usize % SPINNER.len()],
                    if self.step.is_empty() { "Setting up…" } else { &self.step }
                ),
                theme::fg(theme::WORKING),
            ),
            Phase::Done { .. } => (
                "Enter presses   r retries the rows that did not finish   Esc continues".into(),
                theme::dim(),
            ),
            Phase::Editing => (
                "Tab and arrows move   Space ticks   Enter presses   r looks for agents again   Esc leaves".into(),
                theme::dim(),
            ),
        }
    }

    /// The screen's lines, and for each item the first and last line it covers.
    fn compose(&self, width: usize) -> (Vec<Line<'static>>, Vec<Mark>) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut marks = Vec::new();
        let wrap = |text: &str, indent: usize, style: Style| -> Vec<Line<'static>> {
            wrap_text(text, width.saturating_sub(indent))
                .into_iter()
                .map(|piece| Line::from(Span::styled(format!("{}{piece}", " ".repeat(indent)), style)))
                .collect()
        };
        let focus = |item: Item| self.focus == item && !matches!(self.phase, Phase::Running);

        lines.push(Line::from(Span::styled("Smalltalk setup", theme::strong(theme::ACCENT))));
        lines.push(Line::default());
        lines.extend(wrap(
            "Smalltalk runs your AI agents as durable seats that message each other, and keeps their work as missions. Five quick choices, about a minute.",
            0,
            theme::text(),
        ));
        lines.extend(wrap(
            "The Smalltalk Assistant, your first agent, runs on your own Claude or Codex account and uses that account's usage.",
            0,
            theme::soft(),
        ));
        lines.push(Line::default());

        let editing = matches!(self.phase, Phase::Editing);
        for (item, label, value, problem) in [
            (Item::Name, "Your name", &self.form.name, &self.problems[0]),
            (Item::Machine, "This machine", &self.form.machine, &self.problems[1]),
        ] {
            let first = lines.len();
            let focused = focus(item);
            let cursor = if focused { "▏" } else { "" };
            let boxed = format!("{value}{cursor}");
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}{label:<14}", if focused { "▌ " } else { "  " }),
                    if focused { theme::strong(theme::ACCENT) } else { theme::soft() },
                ),
                Span::styled(
                    format!(" {boxed:<32}"),
                    Style::default()
                        .bg(if editing { theme::SURFACE0 } else { theme::BASE })
                        .fg(theme::TEXT),
                ),
            ]));
            if let Some(problem) = problem {
                lines.extend(wrap(problem, 16, theme::fg(theme::FAULT)));
            }
            marks.push((first, item, lines.len() - 1, None));
        }
        lines.push(Line::default());

        // Start at login, and keep running after logout where it is offered.
        let check = |on: bool| if on { "[x]" } else { "[ ]" };
        let first = lines.len();
        lines.push(checkbox_line(focus(Item::Login), check(self.form.login), &self.form.login_label, theme::text()));
        lines.extend(wrap(&self.form.login_note, 6, theme::dim()));
        marks.push((first, Item::Login, lines.len() - 1, None));
        if let Some(on) = self.form.linger {
            let first = lines.len();
            lines.push(checkbox_line(focus(Item::Linger), check(on), "Keep running after I log out", theme::text()));
            marks.push((first, Item::Linger, lines.len() - 1, None));
        }
        lines.push(Line::default());

        lines.push(Line::from(Span::styled("Agents", theme::label())));
        for (index, row) in self.form.rows.iter().enumerate() {
            let first = lines.len();
            let item = Item::Row(index);
            let found = row.facts.found.is_some();
            let mark = if !found {
                "[–]"
            } else {
                check(row.selected)
            };
            let mut line = checkbox_line(
                focus(item),
                mark,
                &row.name,
                if found { theme::text() } else { theme::dim() },
            );
            line.spans.push(Span::styled(
                format!("   {}", facts_summary(row)),
                theme::dim(),
            ));
            lines.push(line);
            if found {
                lines.extend(wrap(&row.sets_up, 6, theme::dim()));
            } else {
                lines.extend(wrap(&row.install_hint, 6, theme::dim()));
            }
            match &row.outcome {
                Some(Outcome::Ready(note)) => {
                    lines.extend(wrap(&format!("✓ Ready. {note}"), 6, theme::fg(theme::IDLE)));
                }
                Some(Outcome::NeedsLogin(note)) => {
                    lines.extend(wrap(&format!("⚠ Needs you to sign in. {note}"), 6, theme::fg(theme::WAITING)));
                }
                Some(Outcome::Failed(error)) => {
                    lines.extend(wrap(&format!("✗ Failed. {error}"), 6, theme::fg(theme::FAULT)));
                }
                None => {}
            }
            marks.push((first, item, lines.len() - 1, None));
        }
        if let Some(access) = self.form.codex {
            lines.push(Line::default());
            let first = lines.len();
            lines.push(checkbox_line(
                focus(Item::CodexAsk),
                check(access != Access::Ask),
                "Let Codex work without asking",
                theme::text(),
            ));
            lines.extend(wrap(
                "Smalltalk is an autonomous software factory, and constant approval prompts stop it. Saved as codex_access; seats started by this version of st still launch Codex with full access until that setting takes effect.",
                6,
                theme::dim(),
            ));
            marks.push((first, Item::CodexAsk, lines.len() - 1, None));
            if access != Access::Ask {
                for (level, label, text) in [
                    (
                        Access::Workspace,
                        "Workspace only",
                        "Codex writes only inside the project's workspace and can reach st. Writes anywhere else fail instead of asking.",
                    ),
                    (
                        Access::Full,
                        "Full access",
                        "No sandbox and no questions: builds, caches and installs can write anywhere on this machine.",
                    ),
                ] {
                    let first = lines.len();
                    let item = Item::CodexLevel(level);
                    lines.push(radio_line(focus(item), access == level, label));
                    lines.extend(wrap(text, 10, theme::dim()));
                    marks.push((first, item, lines.len() - 1, None));
                }
            }
        }
        lines.push(Line::default());

        match &self.phase {
            Phase::Editing => {
                if !self.step.is_empty() {
                    lines.extend(wrap(&format!("✗ {}", self.step), 0, theme::fg(theme::FAULT)));
                    lines.push(Line::default());
                }
                lines.extend(wrap(CONSENT, 0, theme::text()));
                let first = lines.len();
                lines.push(Line::from(vec![
                    button(focus(Item::SetUp), "Set up"),
                    Span::raw("  "),
                    button(focus(Item::Skip), "Skip for now"),
                ]));
                // The two buttons share a line, so a click is told apart by the one that has the
                // keyboard focus; both cover the same rows.
                marks.push((first, Item::SetUp, first, Some((0, 8))));
                marks.push((first, Item::Skip, first, Some((10, 14))));
            }
            Phase::Running => {
                lines.extend(wrap("Setting up. Results appear beside each agent as they finish.", 0, theme::soft()));
            }
            Phase::Done { .. } => {
                let ready = self.form.rows.iter().filter(|row| row.selected && row.outcome.as_ref().is_some_and(Outcome::ready)).count();
                let pending = self.retryable().count();
                if pending == 0 {
                    let none_found = self.form.rows.iter().all(|row| row.facts.found.is_none());
                    lines.extend(wrap(
                        if self.skipped {
                            "st is set up. No agent was set up; run st setup to add one."
                        } else if none_found {
                            "st is set up. No agent is installed yet, so no Assistant was started. Install Claude Code or Codex, then run st setup again."
                        } else if ready == 0 {
                            "st is set up."
                        } else {
                            "Everything you selected is ready."
                        },
                        0,
                        theme::fg(theme::IDLE),
                    ));
                } else {
                    lines.extend(wrap(
                        &format!("{pending} agent{} did not finish. Everything else is set up; you can retry now or fix it later with st setup.", if pending == 1 { "" } else { "s" }),
                        0,
                        theme::fg(theme::WAITING),
                    ));
                }
                let first = lines.len();
                let mut spans = Vec::new();
                if pending > 0 {
                    spans.push(button(focus(Item::Retry), "Retry"));
                    spans.push(Span::raw("  "));
                }
                spans.push(button(focus(Item::Continue), "Continue"));
                lines.push(Line::from(spans));
                if pending > 0 {
                    marks.push((first, Item::Retry, first, Some((0, 7))));
                    marks.push((first, Item::Continue, first, Some((9, 10))));
                } else {
                    marks.push((first, Item::Continue, first, Some((0, 10))));
                }
            }
        }
        (lines, marks)
    }
}

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

fn checkbox_line(focused: bool, mark: &str, label: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if focused { "▌ " } else { "  " },
            theme::strong(theme::ACCENT),
        ),
        Span::styled(format!("{mark} "), if focused { theme::strong(theme::ACCENT) } else { theme::soft() }),
        Span::styled(label.to_owned(), if focused { style.add_modifier(ratatui::style::Modifier::BOLD) } else { style }),
    ])
}

fn radio_line(focused: bool, on: bool, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(if focused { "    ▌ " } else { "      " }, theme::strong(theme::ACCENT)),
        Span::styled(
            format!("{} ", if on { "(•)" } else { "( )" }),
            if focused { theme::strong(theme::ACCENT) } else { theme::soft() },
        ),
        Span::styled(label.to_owned(), theme::text()),
    ])
}

fn button(focused: bool, label: &str) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        if focused {
            Style::default().bg(theme::ACCENT).fg(theme::BASE).add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            Style::default().bg(theme::SURFACE1).fg(theme::TEXT)
        },
    )
}

/// The four facts, in words, for the row's first line.
fn facts_summary(row: &Row) -> String {
    let tri = |tri: Tri, yes: &str, no: &str, unknown: &str| match tri {
        Tri::Yes => yes.to_owned(),
        Tri::No => no.to_owned(),
        Tri::Unknown => unknown.to_owned(),
    };
    match &row.facts.found {
        None => "not installed".to_owned(),
        Some(path) => format!(
            "found {path} · {} · {}",
            tri(row.facts.integration, "st integration ready", "st integration not set up", "st integration not checked"),
            tri(row.facts.signed_in, "signed in", "not signed in", "sign-in not checked"),
        ),
    }
}

/// Rows found by a second look, with the choices already made kept: an agent found now starts
/// ticked, one that was there before keeps what the person chose, and results stay.
pub fn merge_rows(old: &[Row], fresh: Vec<Row>) -> Vec<Row> {
    fresh
        .into_iter()
        .map(|mut row| {
            if let Some(before) = old.iter().find(|before| before.id == row.id) {
                if before.facts.found.is_some() && row.facts.found.is_some() {
                    row.selected = before.selected;
                }
                row.outcome = before.outcome.clone();
            }
            row
        })
        .collect()
}

/// Words broken at spaces to fit `width`; a word longer than the line is cut.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut word = word.to_owned();
            while word.chars().count() > width {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                let cut: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                lines.push(cut);
            }
            let needed = line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
            if needed > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        lines.push(line);
    }
    lines
}

/// Show the screen and run the worker for it. `check` judges the text fields; `work` sets up
/// st and the selected agents, reporting as it goes, and runs on its own thread; `handover` lets
/// the person finish a harness's own first run in this terminal and returns when they are done;
/// `scan` looks for the agents again.
pub fn run<W, H>(
    form: Form,
    check: &(dyn Fn(Field, &str) -> Result<(), String> + Sync),
    work: W,
    handover: H,
    scan: &dyn Fn() -> Vec<Row>,
) -> Result<Finished>
where
    W: Fn(Run, &Reporter) -> Result<(), String> + Sync,
    H: Fn(&str) -> Result<(), String>,
{
    let mut screen = Screen::new(form);
    let mut guard = Some(Guard::enter(false)?);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let stopping = super::stop_flag()?;
    let (events, incoming): (Sender<Event_>, Receiver<Event_>) = mpsc::channel();
    let work = &work;
    std::thread::scope(|scope| -> Result<()> {
        let mut left_at: Option<Instant> = None;
        loop {
            terminal.draw(|frame| {
                let area = frame.area();
                screen.draw(area, frame.buffer_mut());
            })?;
            while let Ok(event) = incoming.try_recv() {
                if let Event_::Handover(id, reply) = event {
                    // Give the harness the whole terminal, then take it back as it was.
                    drop(guard.take());
                    let shown = handover(&id);
                    guard = Some(Guard::enter(false)?);
                    // A new terminal object redraws everything on the fresh alternate screen.
                    // Not `clear()`: it asks the terminal where its cursor is, and not every
                    // terminal answers.
                    terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                    terminal.hide_cursor()?;
                    if let Err(error) = shown {
                        screen.step = error;
                    }
                    let _ = reply.send(());
                } else {
                    screen.event(event);
                }
            }
            if screen.leave && matches!(screen.phase, Phase::Done { .. }) {
                // A clean finish stays up long enough to read.
                let since = *left_at.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(1500) {
                    break;
                }
            } else if screen.leave {
                break;
            }
            if stopping.load(std::sync::atomic::Ordering::Relaxed) || crate::stdin_hung_up() {
                screen.cancelled = !matches!(screen.phase, Phase::Done { .. });
                break;
            }
            if crossterm::event::poll(Duration::from_millis(60))? {
                let reaction = match crossterm::event::read()? {
                    Event::Key(key) => screen.key(key, check),
                    Event::Mouse(mouse) => screen.click(mouse, check),
                    Event::Paste(text) => {
                        for c in text.chars().filter(|c| !c.is_control()) {
                            if let Some(field) = screen.field() {
                                field.push(c);
                            }
                        }
                        Reaction::None
                    }
                    _ => Reaction::None,
                };
                match reaction {
                    Reaction::None => {}
                    Reaction::Rescan => {
                        let fresh = scan();
                        if !fresh.is_empty() {
                            screen.form.rows = merge_rows(&screen.form.rows, fresh);
                            if screen.form.codex.is_none()
                                && screen.form.rows.iter().any(|row| row.id == "codex" && row.facts.found.is_some())
                            {
                                screen.form.codex = Some(Access::Ask);
                            }
                            screen.focus = Item::Name;
                        }
                    }
                    Reaction::Leave => {
                        screen.leave = true;
                        left_at = Some(Instant::now() - Duration::from_secs(10));
                    }
                    Reaction::Start { skip_agents, only } => {
                        screen.phase = Phase::Running;
                        screen.skipped = skip_agents;
                        screen.step = "Setting up…".into();
                        for row in &mut screen.form.rows {
                            if only.as_ref().is_none_or(|only| only.contains(&row.id)) {
                                row.outcome = None;
                            }
                        }
                        let run = Run { form: screen.form.clone(), skip_agents, only };
                        let reporter = Reporter { events: events.clone() };
                        scope.spawn(move || {
                            let result = work(run, &reporter);
                            let _ = reporter.events.send(Event_::Done(result));
                        });
                    }
                }
            }
        }
        Ok(())
    })?;
    drop(guard);
    Ok(Finished { form: screen.form, cancelled: screen.cancelled })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn row(id: &str, name: &str, found: Option<&str>) -> Row {
        Row {
            id: id.into(),
            name: name.into(),
            facts: Facts {
                found: found.map(str::to_owned),
                integration: Tri::Unknown,
                signed_in: Tri::Unknown,
            },
            sets_up: format!("Sets up {name} in ~/.example/{id}."),
            install_hint: format!("Install {name} from its own site, then press r to look again."),
            selected: found.is_some(),
            outcome: None,
        }
    }

    fn form() -> Form {
        Form {
            name: "ada".into(),
            machine: "studio".into(),
            login_label: "Start st when I log in".into(),
            login: true,
            login_note: "Installs a user service. Remove it with st service uninstall.".into(),
            linger: Some(false),
            rows: vec![
                row("claude", "Claude", Some("/usr/bin/claude")),
                row("codex", "Codex", Some("/usr/bin/codex")),
                row("opencode", "OpenCode", None),
            ],
            codex: Some(Access::Ask),
        }
    }

    fn ok(_: Field, value: &str) -> Result<(), String> {
        if value.is_empty() || value == "local" {
            Err("Use 1–63 lowercase letters, digits, hyphens or underscores".into())
        } else {
            Ok(())
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn frame(screen: &mut Screen, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                screen.draw(area, frame.buffer_mut());
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(screen: &mut Screen, code: KeyCode) -> Reaction {
        screen.key(key(code), &ok)
    }

    #[test]
    fn the_screen_shows_everything_to_choose_and_the_one_consent_line() {
        let mut screen = Screen::new(form());
        let shown = frame(&mut screen, 110, 60);
        for text in [
            "Smalltalk setup",
            "your own Claude or Codex account",
            "Your name",
            "ada",
            "This machine",
            "studio",
            "[x] Start st when I log in",
            "Keep running after I log out",
            "[x] Claude",
            "found /usr/bin/claude",
            "[x] Codex",
            "[–] OpenCode",
            "not installed",
            "Install OpenCode from its own site",
            "Let Codex work without asking",
            "Set up st for the selected agents, adding its local integrations and required",
            "configuration?",
            "Set up",
            "Skip for now",
        ] {
            assert!(shown.contains(text), "{text}:\n{shown}");
        }
        assert!(!shown.contains("(•)"), "the levels wait for the box to be ticked");
    }

    #[test]
    fn the_four_facts_stay_apart_and_unknowns_say_so() {
        let mut screen = Screen::new(form());
        screen.form.rows[0].facts.integration = Tri::Yes;
        screen.form.rows[0].facts.signed_in = Tri::No;
        let shown = frame(&mut screen, 120, 60);
        assert!(shown.contains("st integration ready · not signed in"), "{shown}");
        assert!(shown.contains("st integration not checked · sign-in not checked"), "{shown}");
    }

    #[test]
    fn keys_move_tick_and_type_without_leaking_between_fields() {
        let mut screen = Screen::new(form());
        press(&mut screen, KeyCode::Backspace);
        press(&mut screen, KeyCode::Char('x'));
        assert_eq!(screen.form.name, "adx");
        press(&mut screen, KeyCode::Enter);
        assert_eq!(screen.focus, Item::Machine);
        press(&mut screen, KeyCode::Char('!'));
        assert_eq!(screen.form.machine, "studio!");
        press(&mut screen, KeyCode::Tab);
        assert_eq!(screen.focus, Item::Login);
        press(&mut screen, KeyCode::Char(' '));
        assert!(!screen.form.login);
        press(&mut screen, KeyCode::Tab);
        press(&mut screen, KeyCode::Char(' '));
        assert_eq!(screen.form.linger, Some(true));
        press(&mut screen, KeyCode::Tab);
        assert_eq!(screen.focus, Item::Row(0));
        press(&mut screen, KeyCode::Char(' '));
        assert!(!screen.form.rows[0].selected);
        // The missing harness is not on the focus path: nothing to select.
        press(&mut screen, KeyCode::Tab);
        assert_eq!(screen.focus, Item::Row(1));
        press(&mut screen, KeyCode::Tab);
        assert_eq!(screen.focus, Item::CodexAsk);
        press(&mut screen, KeyCode::BackTab);
        assert_eq!(screen.focus, Item::Row(1));
    }

    #[test]
    fn the_codex_box_offers_its_two_levels_once_ticked_and_stores_the_chosen_one() {
        let mut screen = Screen::new(form());
        screen.focus = Item::CodexAsk;
        press(&mut screen, KeyCode::Char(' '));
        assert_eq!(screen.form.codex, Some(Access::Workspace));
        let shown = frame(&mut screen, 120, 70);
        assert!(shown.contains("(•) Workspace only"), "{shown}");
        assert!(shown.contains("( ) Full access"), "{shown}");
        assert!(shown.contains("fail instead of asking"), "{shown}");
        press(&mut screen, KeyCode::Tab);
        press(&mut screen, KeyCode::Tab);
        assert_eq!(screen.focus, Item::CodexLevel(Access::Full));
        press(&mut screen, KeyCode::Enter);
        assert_eq!(screen.form.codex, Some(Access::Full));
        assert_eq!(Access::Full.as_str(), "full");
        screen.focus = Item::CodexAsk;
        press(&mut screen, KeyCode::Char(' '));
        assert_eq!(screen.form.codex, Some(Access::Ask));
        assert_eq!(Access::Ask.as_str(), "ask");
    }

    #[test]
    fn a_form_without_codex_has_no_codex_control() {
        let mut one = form();
        one.rows.retain(|row| row.id != "codex");
        one.codex = None;
        let mut screen = Screen::new(one);
        assert!(!frame(&mut screen, 110, 60).contains("Let Codex work without asking"));
        assert!(!screen.items().contains(&Item::CodexAsk));
    }

    #[test]
    fn a_bad_name_stops_the_action_and_says_why_beside_the_field() {
        let mut screen = Screen::new(form());
        screen.form.machine = "local".into();
        screen.focus = Item::SetUp;
        assert!(matches!(press(&mut screen, KeyCode::Enter), Reaction::None));
        assert_eq!(screen.focus, Item::Machine, "the cursor goes to the field to fix");
        let shown = frame(&mut screen, 110, 60);
        assert!(shown.contains("Use 1–63 lowercase letters"), "{shown}");
        screen.form.machine = "studio".into();
        screen.focus = Item::SetUp;
        assert!(matches!(
            press(&mut screen, KeyCode::Enter),
            Reaction::Start { skip_agents: false, only: None }
        ));
        screen.focus = Item::Skip;
        assert!(matches!(
            press(&mut screen, KeyCode::Enter),
            Reaction::Start { skip_agents: true, only: None }
        ));
    }

    #[test]
    fn each_row_ends_on_its_own_and_full_errors_stay_on_screen() {
        let mut screen = Screen::new(form());
        screen.phase = Phase::Running;
        screen.event(Event_::Row("claude".into(), Outcome::Ready("The st channel is installed.".into())));
        let long = "claude plugin install failed: ".to_owned() + &"the marketplace could not be reached; ".repeat(6) + "THE END";
        screen.event(Event_::Row("codex".into(), Outcome::Failed(long)));
        screen.event(Event_::Done(Ok(())));
        assert!(matches!(screen.phase, Phase::Done { .. }));
        assert!(!screen.leave, "a failure keeps the screen up for the person");
        let shown = frame(&mut screen, 110, 70);
        assert!(shown.contains("✓ Ready. The st channel is installed."), "{shown}");
        assert!(shown.contains("✗ Failed. claude plugin install failed"), "{shown}");
        assert!(shown.contains("THE END"), "the whole error is visible:\n{shown}");
        assert!(shown.contains("1 agent did not finish"), "{shown}");
        assert_eq!(screen.focus, Item::Retry);
        // A retry runs only the row that did not end ready.
        match press(&mut screen, KeyCode::Enter) {
            Reaction::Start { skip_agents: false, only: Some(only) } => assert_eq!(only, ["codex"]),
            other => panic!("{}", matches!(other, Reaction::None)),
        }
    }

    #[test]
    fn skipping_leaves_no_agent_pending() {
        let mut screen = Screen::new(form());
        screen.phase = Phase::Running;
        screen.skipped = true;
        screen.event(Event_::Done(Ok(())));
        assert!(screen.leave, "nothing is left to retry");
        assert!(frame(&mut screen, 110, 70).contains("No agent was set up"));
    }

    #[test]
    fn a_needs_sign_in_row_is_not_a_failure_and_not_ready() {
        let mut screen = Screen::new(form());
        screen.phase = Phase::Running;
        screen.event(Event_::Row("claude".into(), Outcome::Ready("ok".into())));
        screen.event(Event_::Row("codex".into(), Outcome::NeedsLogin("Run codex login in your own terminal.".into())));
        screen.event(Event_::Done(Ok(())));
        let shown = frame(&mut screen, 110, 70);
        assert!(shown.contains("⚠ Needs you to sign in. Run codex login"), "{shown}");
        assert_eq!(screen.retryable().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["codex"]);
    }

    #[test]
    fn a_clean_finish_leaves_by_itself_and_an_error_from_st_returns_to_the_form() {
        let mut screen = Screen::new(form());
        screen.phase = Phase::Running;
        screen.event(Event_::Row("claude".into(), Outcome::Ready("ok".into())));
        screen.event(Event_::Row("codex".into(), Outcome::Ready("ok".into())));
        screen.event(Event_::Done(Ok(())));
        assert!(screen.leave);
        assert!(frame(&mut screen, 110, 70).contains("Everything you selected is ready."));

        let mut screen = Screen::new(form());
        screen.phase = Phase::Running;
        screen.event(Event_::Done(Err("this store belongs to `harbor`; setup cannot rename its machine".into())));
        assert!(matches!(screen.phase, Phase::Editing));
        assert_eq!(screen.focus, Item::SetUp);
        let shown = frame(&mut screen, 110, 70);
        assert!(shown.contains("✗ this store belongs to `harbor`"), "{shown}");
    }

    #[test]
    fn keys_are_ignored_while_setup_runs_and_escape_leaves_only_before_it() {
        let mut screen = Screen::new(form());
        screen.phase = Phase::Running;
        assert!(matches!(press(&mut screen, KeyCode::Esc), Reaction::None));
        assert!(matches!(press(&mut screen, KeyCode::Char('x')), Reaction::None));
        assert_eq!(screen.form.name, "ada");
        let mut screen = Screen::new(form());
        assert!(matches!(press(&mut screen, KeyCode::Esc), Reaction::Leave));
        assert!(screen.cancelled);
    }

    #[test]
    fn clicking_a_box_ticks_it_and_clicking_a_button_presses_it() {
        let mut screen = Screen::new(form());
        let _ = frame(&mut screen, 110, 60);
        let at = |screen: &Screen, item: Item| {
            let (rect, _) = screen.hits.iter().find(|(_, hit)| *hit == item).unwrap();
            (rect.x + 3, rect.y)
        };
        let (column, row) = at(&screen, Item::Row(1));
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        screen.click(down, &ok);
        assert!(!screen.form.rows[1].selected);
        assert_eq!(screen.focus, Item::Row(1));
        let (column, row) = at(&screen, Item::Login);
        screen.click(MouseEvent { column, row, ..down }, &ok);
        assert!(!screen.form.login);
    }

    #[test]
    fn a_small_terminal_scrolls_to_keep_the_focus_in_view() {
        let mut screen = Screen::new(form());
        screen.focus = Item::Skip;
        let shown = frame(&mut screen, 80, 14);
        assert!(shown.contains("Skip for now"), "{shown}");
        assert!(!shown.contains("Smalltalk setup"), "the top scrolled away:\n{shown}");
        screen.focus = Item::Name;
        let shown = frame(&mut screen, 80, 14);
        assert!(shown.contains("Smalltalk setup"), "{shown}");
    }

    #[test]
    fn looking_again_keeps_choices_and_ticks_only_what_is_new() {
        let mut one = form();
        one.rows[0].selected = false;
        let mut fresh = form().rows;
        fresh[2] = row("opencode", "OpenCode", Some("/usr/bin/opencode"));
        let merged = merge_rows(&one.rows, fresh);
        assert!(!merged[0].selected, "the person unticked Claude and it stays unticked");
        assert!(merged[1].selected);
        assert!(merged[2].selected && merged[2].facts.found.is_some(), "a newly found agent starts ticked");
        let mut screen = Screen::new(one);
        assert!(matches!(press(&mut screen, KeyCode::Char('r')), Reaction::Rescan) == false, "r types into the name");
        screen.focus = Item::Login;
        assert!(matches!(press(&mut screen, KeyCode::Char('r')), Reaction::Rescan));
    }

    #[test]
    fn wrapping_breaks_at_words_and_cuts_only_what_cannot_fit() {
        assert_eq!(wrap_text("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap_text("abcdefghij", 8), ["abcdefgh", "ij"]);
        assert_eq!(wrap_text("first\nsecond", 20), ["first", "second"]);
    }
}
