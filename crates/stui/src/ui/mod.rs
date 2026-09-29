//! The new stui rendering layer: a shell that draws a `view::World`.
//!
//! The shell owns layout, the click map, scrolling, selection and keys. Screens only build
//! documents (`screens`), and the conversation renderer (`conversation`) turns entries into
//! cached lines. `stui --demo` runs it on an invented world; the live client will feed it the
//! same `World`.

pub mod adapt;
#[cfg(test)]
mod contract;
pub mod conversation;
pub mod demo;
pub mod doc;
pub mod live;
pub mod screens;
pub mod text;
pub mod theme;
// The live client fills the variants and fields the demo does not use.
#[allow(dead_code)]
pub mod view;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use doc::{Doc, Hit};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend, TestBackend},
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use screens::{Drafts, Item, ListState, Listing, TABS};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{self, Write},
    rc::Rc,
    time::{Duration, Instant},
};
use view::*;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Default, Clone, Copy)]
struct PaneState {
    top: usize,
    follow: bool,
    seen: usize,
    unseen: usize,
}

struct FramePane {
    key: String,
    rect: Rect,
    top: usize,
    total: usize,
    lines: Rc<Vec<Line<'static>>>,
}

#[derive(Default)]
struct FrameInfo {
    hits: Vec<(Rect, Hit)>,
    panes: Vec<FramePane>,
    sidebar: Rect,
    sidebar_lines: usize,
    sidebar_height: usize,
}

#[derive(Clone)]
struct Selection {
    pane: String,
    anchor: (usize, u16),
    head: (usize, u16),
}

struct Demo {
    started: Instant,
    loaded: bool,
    streamed: usize,
    mail: bool,
    tool_done: bool,
    /// When the person first opened these conversations: their demo motion starts then.
    cos_seen: Option<Instant>,
    harbor_seen: Option<Instant>,
}

/// A request the live loop sends to st. The demo never produces these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Attention {
        id: String,
        action: String,
        reason: Option<String>,
    },
    LaunchRevise {
        id: String,
        feedback: String,
    },
    Reply {
        id: String,
        to: String,
        text: String,
    },
    Send {
        agent: String,
        text: String,
    },
    /// Cancel a mission's latest run.
    CancelRun {
        mission: String,
    },
    OpenTerminal {
        agent: String,
    },
    TerminalKey(KeyEvent),
    CloseTerminal,
    CreateLaunch {
        title: String,
        request: String,
        mission: String,
        workspace: String,
    },
    RevokeDevice {
        id: String,
    },
    /// "Chat about this": a new message to `to`, titled after the item, with its context.
    Discuss {
        to: String,
        title: String,
        text: String,
    },
}

/// An agent's live terminal screen, drawn in place of its conversation.
pub(crate) struct TerminalView {
    /// The agent's name; the title adds the program's own title once a screen names it.
    pub(crate) name: String,
    pub(crate) title: String,
    pub(crate) lines: Vec<Line<'static>>,
    /// The visible cursor's row and column on the screen.
    pub(crate) cursor: Option<(usize, usize)>,
    /// Why the screen shown is not current: still connecting, or reconnecting after a drop.
    pub(crate) stale: Option<String>,
    pub(crate) ended: Option<String>,
}

/// An open "chat about this" on a Home item.
#[derive(Clone, Debug)]
struct ChatState {
    item: String,
    to: String,
    to_name: String,
    editing: bool,
}

pub struct Ui {
    world: World,
    tab: usize,
    selected: [usize; 5],
    list_top: RefCell<[usize; 5]>,
    /// The selection each sidebar last scrolled into view. The wheel may scroll away from
    /// the selection; only a new selection brings it back.
    list_follows: RefCell<[Option<usize>; 5]>,
    panes: RefCell<HashMap<String, PaneState>>,
    expanded: HashSet<String>,
    cache: conversation::Cache,
    drafts: HashMap<String, String>,
    editing: bool,
    confirm: Option<char>,
    flash: Option<(String, Instant)>,
    tick: u64,
    help: bool,
    sidebar: bool,
    system: bool,
    frame: RefCell<FrameInfo>,
    selection: Option<Selection>,
    dragging: bool,
    demo: Option<Demo>,
    quit: bool,
    /// Live: actions become `effects` for the live loop instead of demo edits.
    live: bool,
    effects: Vec<Effect>,
    popover: Option<String>,
    chat: Option<ChatState>,
    /// The agent details pane beside the conversation.
    details: bool,
    /// The Missions tab shows the selected mission's whole declaration.
    kdl: bool,
    /// Agents and Missions list as the graph's path tree instead of grouped by state.
    tree: bool,
    /// An attached terminal shown in place of the conversation.
    pub(crate) terminal: Option<TerminalView>,
    /// A Ctrl-C or Ctrl-D pressed once in a terminal, waiting for its confirming second press.
    terminal_confirm: Option<(KeyCode, Instant)>,
    /// The New mission form: title, request, mission id, workspace; and the focused field.
    new_mission: Option<([String; 4], usize)>,
    /// A device awaiting a confirmed revoke.
    revoke: Option<String>,
    /// Home items put off until later. Demo only: kept in memory on this machine.
    snoozed: HashSet<String>,
}

impl Ui {
    pub fn new(world: World) -> Self {
        Self {
            world,
            tab: 0,
            selected: [0; 5],
            list_top: RefCell::new([0; 5]),
            list_follows: RefCell::new([None; 5]),
            panes: RefCell::new(HashMap::new()),
            expanded: HashSet::new(),
            cache: conversation::Cache::default(),
            drafts: HashMap::new(),
            editing: false,
            confirm: None,
            flash: None,
            tick: 0,
            help: false,
            sidebar: true,
            system: false,
            frame: RefCell::new(FrameInfo::default()),
            selection: None,
            dragging: false,
            demo: None,
            quit: false,
            live: false,
            effects: Vec::new(),
            popover: None,
            chat: None,
            details: true,
            kdl: false,
            tree: false,
            terminal: None,
            terminal_confirm: None,
            new_mission: None,
            revoke: None,
            snoozed: HashSet::new(),
        }
    }

    /// Replace the world, keeping each tab's selection on the same item.
    pub fn set_world(&mut self, world: World) {
        let tab = self.tab;
        let mut chosen = Vec::new();
        for index in 0..TABS.len() {
            self.tab = index;
            chosen.push(self.selected_id());
        }
        self.world = world;
        for (index, id) in chosen.into_iter().enumerate() {
            self.tab = index;
            if let Some(id) = id
                && let Some(position) = self.ids().iter().position(|candidate| *candidate == id)
            {
                self.selected[index] = position;
            }
        }
        self.tab = tab;
    }

    /// The tab and the id selected in it.
    pub fn focus(&self) -> (usize, Option<String>) {
        (self.tab, self.selected_id())
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    fn listing(&self, width: usize) -> Listing {
        match self.tab {
            0 => screens::home_list(&self.world, &self.snoozed),
            1 if self.tree => screens::agents_tree(&self.world, self.spinner()),
            1 => screens::agents_list(&self.world, self.spinner(), width),
            2 if self.tree => screens::missions_tree(&self.world, self.spinner(), self.system),
            2 => screens::missions_list(&self.world, self.spinner(), self.system),
            3 => screens::fleet_list(&self.world),
            _ => screens::worktrees_list(&self.world),
        }
    }

    fn ids(&self) -> Vec<String> {
        self.listing(40).ids
    }

    fn selected_id(&self) -> Option<String> {
        let ids = self.ids();
        ids.get(self.selected[self.tab].min(ids.len().saturating_sub(1)))
            .cloned()
    }

    fn flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), Instant::now()));
    }

    fn draft_key(&self) -> Option<String> {
        if self.tab == 1 {
            self.selected_id()
        } else {
            self.attention_focus()
        }
    }

    // ------------------------------------------------------------------ drawing

    pub fn render(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let buf = frame.buffer_mut();
        *self.frame.borrow_mut() = FrameInfo::default();
        buf.set_style(area, Style::default().bg(theme::BASE).fg(theme::TEXT));
        if area.width < 20 || area.height < 6 {
            buf.set_stringn(
                area.x,
                area.y,
                "stui needs a bigger window",
                area.width as usize,
                theme::text(),
            );
            return;
        }
        self.top_bar(buf, Rect { height: 1, ..area });
        let body = Rect {
            y: area.y + 1,
            height: area.height - 2,
            ..area
        };
        let side = if self.sidebar && area.width >= 70 {
            (area.width * 3 / 10).clamp(30, 46)
        } else {
            0
        };
        if side > 0 {
            let sidebar = Rect {
                width: side,
                ..body
            };
            self.draw_sidebar(buf, sidebar);
            for y in body.y..body.y + body.height {
                buf[(area.x + side, y)]
                    .set_symbol("│")
                    .set_style(Style::default().fg(theme::SURFACE0).bg(theme::BASE));
            }
        }
        let main = Rect {
            x: area.x + side + if side > 0 { 2 } else { 1 },
            width: area
                .width
                .saturating_sub(side + if side > 0 { 3 } else { 2 }),
            ..body
        };
        self.draw_main(buf, main);
        self.footer(
            buf,
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
        if let Some(subject) = &self.popover {
            self.draw_popover(buf, area, subject);
        }
        if self.help {
            self.draw_help(buf, area);
        }
    }

    fn hit(&self, rect: Rect, hit: Hit) {
        self.frame.borrow_mut().hits.push((rect, hit));
    }

    fn top_bar(&self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, Style::default().bg(theme::CRUST));
        let mut x = area.x;
        buf.set_stringn(
            x,
            area.y,
            " ≡ st ",
            6,
            theme::strong(theme::ACCENT).bg(theme::CRUST),
        );
        self.hit(Rect { width: 6, ..area }, Hit::Key('s'));
        x += 7;
        for (index, name) in TABS.iter().enumerate() {
            let badge = match index {
                0 => self
                    .world
                    .attention
                    .ready()
                    .map(|items| {
                        items
                            .iter()
                            .filter(|item| !self.snoozed.contains(&item.id))
                            .fold((0, false), |(total, blocking), item| {
                                (total + 1, blocking || item.tier == Tier::Stopped)
                            })
                    })
                    .filter(|(total, _)| *total > 0),
                _ => None,
            };
            let label = format!(" {} {name} ", index + 1);
            let selected = index == self.tab;
            let style = if selected {
                Style::default()
                    .fg(theme::CRUST)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::SUBTEXT0).bg(theme::CRUST)
            };
            let start = x;
            buf.set_stringn(
                x,
                area.y,
                &label,
                area.width.saturating_sub(x) as usize,
                style,
            );
            x += text::width(&label) as u16;
            if let Some((count, blocking)) = badge {
                let badge = if blocking {
                    format!("◆{count} ")
                } else {
                    format!("{count} ")
                };
                buf.set_stringn(
                    x,
                    area.y,
                    &badge,
                    4,
                    Style::default()
                        .fg(if selected {
                            theme::CRUST
                        } else if blocking {
                            theme::PERSON
                        } else {
                            theme::SUBTEXT0
                        })
                        .bg(if selected {
                            theme::ACCENT
                        } else {
                            theme::CRUST
                        })
                        .add_modifier(Modifier::BOLD),
                );
                x += text::width(&badge) as u16;
            }
            if index == 4 {
                // The graph has no worktrees yet; this tab shows invented data and says so.
                buf.set_stringn(
                    x,
                    area.y,
                    " demo ",
                    6,
                    Style::default()
                        .fg(theme::CRUST)
                        .bg(theme::YELLOW)
                        .add_modifier(Modifier::BOLD),
                );
                x += 6;
            }
            self.hit(
                Rect {
                    x: start,
                    width: x - start,
                    ..area
                },
                Hit::Tab(index),
            );
            x += 1;
        }
        let (glyph, word, color) = match &self.world.link {
            // Live, but showing a graph that exchanges cannot correct.
            Link::Live if !self.world.diverged.is_empty() => ("⚠", "diverged", theme::RED),
            Link::Live => ("●", "live", theme::GREEN),
            Link::Connecting => (self.spinner(), "connecting", theme::YELLOW),
            Link::Offline(_) => ("○", "offline", theme::RED),
        };
        let person = self.world.person.trim_start_matches("person/");
        let right = format!("{glyph} {word}  {} · {person} ", self.world.host);
        let width = text::width(&right) as u16;
        if x + width < area.x + area.width {
            let start = area.x + area.width - width;
            buf.set_stringn(
                start,
                area.y,
                format!("{glyph} {word}"),
                20,
                Style::default().fg(color).bg(theme::CRUST),
            );
            let rest = format!("  {} · {person} ", self.world.host);
            buf.set_stringn(
                start + text::width(&format!("{glyph} {word}")) as u16,
                area.y,
                rest,
                40,
                Style::default().fg(theme::OVERLAY1).bg(theme::CRUST),
            );
            self.hit(
                Rect {
                    x: start,
                    width,
                    ..area
                },
                Hit::Help,
            );
        }
    }

    fn footer(&self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, Style::default().bg(theme::CRUST));
        let hints: Vec<(&str, &str)> = if self.terminal.is_some() && self.tab == 1 {
            vec![
                ("ctrl+\\", "return"),
                ("keys", "go to the agent"),
                ("ctrl-c twice", "interrupt"),
            ]
        } else if self.new_mission.is_some() && self.tab == 2 {
            vec![
                ("tab", "next field"),
                ("alt+enter", "new line"),
                ("enter", "next / create"),
                ("esc", "cancel"),
            ]
        } else if self.editing {
            vec![("enter", "send"), ("esc", "stop editing"), ("⌫", "delete")]
        } else if self.confirm.is_some() {
            vec![("y", "confirm"), ("esc", "cancel")]
        } else {
            let mut hints = vec![("1-4", "tabs"), ("↑↓", "select")];
            match self.tab {
                0 => hints.extend([("keys", "on the card"), ("c", "write")]),
                1 => hints.extend([
                    ("c", "message"),
                    ("i", "details"),
                    ("o", "expand tools"),
                    ("end", "latest"),
                    ("drag", "select + copy"),
                ]),
                2 => hints.extend([("n", "new mission"), ("t", "tree"), ("x", "system")]),
                _ => {}
            }
            hints.extend([("?", "help"), ("q", "quit")]);
            hints
        };
        let mut x = area.x + 1;
        for (key, label) in hints {
            let key_text = format!("{key} ");
            let label_text = format!("{label}   ");
            if x + (text::width(&key_text) + text::width(&label_text)) as u16 > area.x + area.width
            {
                break;
            }
            buf.set_stringn(
                x,
                area.y,
                &key_text,
                20,
                theme::strong(theme::ACCENT).bg(theme::CRUST),
            );
            x += text::width(&key_text) as u16;
            buf.set_stringn(
                x,
                area.y,
                &label_text,
                40,
                Style::default().fg(theme::OVERLAY1).bg(theme::CRUST),
            );
            x += text::width(&label_text) as u16;
        }
        if let Some((message, _)) = &self.flash {
            let message = format!(" {message} ");
            let width = text::width(&message) as u16;
            // A message wins over the key hints: on a narrow window it must still show.
            let start = (area.x + area.width).saturating_sub(width + 1).max(area.x);
            let trouble = [
                "Failed",
                "Could not",
                "not sent",
                "Detach failed",
                "Key not sent",
            ]
            .iter()
            .any(|word| message.contains(word));
            buf.set_stringn(
                start,
                area.y,
                &message,
                (area.x + area.width - start) as usize,
                Style::default()
                    .fg(theme::CRUST)
                    .bg(if trouble { theme::RED } else { theme::GREEN })
                    .add_modifier(Modifier::BOLD),
            );
        } else if self.demo.is_some() {
            let label = " demo · invented data · nothing is sent ";
            let width = text::width(label) as u16;
            if x + width < area.x + area.width {
                buf.set_stringn(
                    area.x + area.width - width - 1,
                    area.y,
                    label,
                    width as usize,
                    Style::default().fg(theme::OVERLAY0).bg(theme::CRUST),
                );
            }
        }
    }

    fn draw_sidebar(&self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, Style::default().bg(theme::MANTLE));
        // One column for the frame edge and one kept free for the scrollbar.
        let width = area.width.saturating_sub(2) as usize;
        let listing = self.listing(width);
        let legend_height = if listing.legend.is_empty() {
            0
        } else {
            listing.legend.len() as u16 + 1
        };
        let list = Rect {
            height: area.height.saturating_sub(legend_height),
            ..area
        };
        {
            let mut info = self.frame.borrow_mut();
            info.sidebar = list;
        }
        // Legend, pinned to the bottom.
        if legend_height > 0 {
            let y = area.y + area.height - legend_height;
            buf.set_stringn(
                area.x,
                y,
                "─".repeat(area.width as usize),
                area.width as usize,
                Style::default().fg(theme::SURFACE0).bg(theme::MANTLE),
            );
            for (index, line) in listing.legend.iter().enumerate() {
                buf.set_line(
                    area.x + 1,
                    y + 1 + index as u16,
                    line,
                    area.width.saturating_sub(1),
                );
            }
        }
        match &listing.state {
            ListState::Loading(message) => {
                buf.set_stringn(
                    list.x + 1,
                    list.y + 1,
                    format!("{} {message}", self.spinner()),
                    width,
                    Style::default().fg(theme::OVERLAY1).bg(theme::MANTLE),
                );
                return;
            }
            ListState::Empty(message) => {
                buf.set_stringn(
                    list.x + 1,
                    list.y + 1,
                    format!("✓ {message}"),
                    width,
                    Style::default().fg(theme::GREEN).bg(theme::MANTLE),
                );
                return;
            }
            ListState::Failed(error) => {
                let lines = text::wrap(
                    &[text::run(
                        format!("Could not load: {error}"),
                        theme::fg(theme::RED),
                    )],
                    width.saturating_sub(1),
                    &[],
                    &[],
                    None,
                );
                for (index, line) in lines.iter().enumerate().take(list.height as usize) {
                    buf.set_line(list.x + 1, list.y + 1 + index as u16, line, width as u16);
                }
                return;
            }
            ListState::Ready => {}
        }
        // Lay the items out as lines, remembering where the selected row sits.
        let selected = self.selected[self.tab].min(listing.ids.len().saturating_sub(1));
        let mut rows: Vec<(Option<usize>, Line<'static>, bool)> = Vec::new();
        let mut selected_range = (0, 0);
        for item in &listing.items {
            match item {
                Item::Header {
                    title,
                    count,
                    color,
                } => {
                    if !rows.is_empty() {
                        rows.push((None, Line::default(), false));
                    }
                    let label = format!(" {title}");
                    let count = format!("{count} ");
                    let fill = width.saturating_sub(text::width(&label) + text::width(&count) + 1);
                    rows.push((
                        None,
                        Line::from(vec![
                            Span::styled(label, theme::strong(*color)),
                            Span::styled(
                                format!(" {}", "─".repeat(fill.saturating_sub(1))),
                                theme::fg(theme::SURFACE0),
                            ),
                            Span::styled(format!(" {count}"), theme::dim()),
                        ]),
                        false,
                    ));
                }
                Item::Row {
                    index,
                    first,
                    right,
                    second,
                } => {
                    let is_selected = *index == selected;
                    if is_selected {
                        let height = if second.is_empty() { 1 } else { 2 };
                        selected_range = (rows.len(), rows.len() + height);
                    }
                    let right_width = right.iter().map(Span::width).sum::<usize>();
                    let mut spans = truncate_spans(first, width.saturating_sub(right_width + 2));
                    let used = spans.iter().map(Span::width).sum::<usize>();
                    spans.push(Span::raw(
                        " ".repeat(width.saturating_sub(used + right_width + 1)),
                    ));
                    spans.extend(right.iter().cloned());
                    rows.push((Some(*index), Line::from(spans), is_selected));
                    if !second.is_empty() {
                        rows.push((
                            Some(*index),
                            Line::from(truncate_spans(second, width)),
                            is_selected,
                        ));
                    }
                }
                Item::Folder(line) => rows.push((None, line.clone(), false)),
                Item::Note(line) => {
                    if !rows.is_empty() {
                        rows.push((None, Line::default(), false));
                    }
                    rows.push((None, line.clone(), false));
                }
            }
        }
        let height = list.height as usize;
        let mut top = self.list_top.borrow()[self.tab];
        let followed = self.list_follows.borrow()[self.tab];
        if followed != Some(selected) {
            if selected_range.1 > top + height {
                top = selected_range.1 - height;
            }
            if selected_range.0 < top {
                top = selected_range.0.saturating_sub(1);
            }
            self.list_follows.borrow_mut()[self.tab] = Some(selected);
        }
        top = top.min(rows.len().saturating_sub(height));
        self.list_top.borrow_mut()[self.tab] = top;
        {
            let mut info = self.frame.borrow_mut();
            info.sidebar_lines = rows.len();
            info.sidebar_height = height;
        }
        for (offset, (index, line, is_selected)) in rows.iter().skip(top).take(height).enumerate() {
            let y = list.y + offset as u16;
            let row = Rect {
                x: list.x,
                y,
                width: list.width,
                height: 1,
            };
            if *is_selected {
                buf.set_style(row, Style::default().bg(theme::ROW_SELECTED));
                buf[(list.x, y)]
                    .set_symbol("▌")
                    .set_style(Style::default().fg(theme::ACCENT).bg(theme::ROW_SELECTED));
            }
            buf.set_line(list.x + 1, y, line, list.width.saturating_sub(1));
            if let Some(index) = index {
                self.hit(row, Hit::Row(*index));
            }
        }
        if rows.len() > height {
            scrollbar(
                buf,
                Rect {
                    x: list.x + list.width - 1,
                    ..list
                },
                top,
                rows.len(),
            );
        }
    }

    /// The attention item the current screen acts on: the Home selection, or the decision
    /// embedded in the selected mission.
    fn attention_focus(&self) -> Option<String> {
        match self.tab {
            0 => self.selected_id(),
            2 => {
                let id = self.selected_id()?;
                self.world
                    .missions
                    .items()
                    .iter()
                    .find(|mission| mission.id == id)?
                    .decision
                    .clone()
                    .filter(|decision| {
                        self.world
                            .attention
                            .items()
                            .iter()
                            .any(|item| &item.id == decision)
                    })
            }
            _ => None,
        }
    }

    fn drafts_for(&self, key: &str, width: usize) -> Drafts<'_> {
        let chat_key = format!("chat:{key}");
        let chat = self.chat.as_ref().filter(|chat| chat.item == key).map(|chat| {
            let title = self
                .world
                .attention
                .items()
                .iter()
                .find(|item| item.id == key)
                .map(|item| format!("About: {}", item.title))
                .unwrap_or_default();
            let thread = match self.world.conversations.get(&chat.to) {
                Some(Load::Ready(entries)) => {
                    let about = entries
                        .iter()
                        .filter(|entry| matches!(&entry.body, Body::Mail { subject, .. } if *subject == title))
                        .cloned()
                        .collect::<Vec<_>>();
                    self.cache
                        .render(&about, width.saturating_sub(4), &self.expanded, self.spinner())
                        .lines
                }
                _ => Vec::new(),
            };
            screens::Chat {
                to: chat.to_name.clone(),
                text: self.drafts.get(&chat_key).map(String::as_str).unwrap_or(""),
                editing: chat.editing,
                thread,
            }
        });
        Drafts {
            text: self.drafts.get(key).map(String::as_str),
            editing: self.editing,
            confirm: self.confirm,
            chat,
        }
    }

    fn draw_main(&self, buf: &mut Buffer, area: Rect) {
        let width = area.width.saturating_sub(1) as usize;
        let id = self.selected_id();
        match self.tab {
            0 => {
                let key = id.clone().unwrap_or_default();
                let drafts = self.drafts_for(&key, width);
                let doc = screens::home_detail(&self.world, id.as_deref(), width, &drafts);
                self.pane(buf, &format!("home:{key}"), area, doc, false);
            }
            1 if self.terminal.is_some() => self.draw_terminal(buf, area),
            1 => self.draw_agent(buf, area, id.as_deref()),
            2 if self.new_mission.is_some() => {
                let (fields, focus) = self.new_mission.as_ref().unwrap();
                let doc = screens::new_mission_form(fields, *focus, width);
                self.pane(buf, "new-mission", area, doc, false);
            }
            2 if self.kdl => {
                let doc = screens::mission_kdl(&self.world, id.as_deref(), width);
                self.pane(
                    buf,
                    &format!("kdl:{}", id.unwrap_or_default()),
                    area,
                    doc,
                    false,
                );
            }
            2 => {
                let decision = self.attention_focus().map(|decision| {
                    let drafts = self.drafts_for(&decision, width);
                    screens::home_detail(&self.world, Some(&decision), width, &drafts)
                });
                let doc = screens::mission_detail(
                    &self.world,
                    id.as_deref(),
                    width,
                    self.spinner(),
                    decision,
                    &self.expanded,
                );
                self.pane(
                    buf,
                    &format!("mission:{}", id.unwrap_or_default()),
                    area,
                    doc,
                    false,
                );
            }
            3 => {
                let doc = screens::fleet_detail(&self.world, id.as_deref(), width, self.spinner());
                self.pane(
                    buf,
                    &format!("fleet:{}", id.unwrap_or_default()),
                    area,
                    doc,
                    false,
                );
            }
            _ => {
                let doc =
                    screens::worktree_detail(&self.world, id.as_deref(), width, self.spinner());
                self.pane(
                    buf,
                    &format!("tree:{}", id.unwrap_or_default()),
                    area,
                    doc,
                    false,
                );
            }
        }
    }

    fn draw_agent(&self, buf: &mut Buffer, area: Rect, id: Option<&str>) {
        let width = area.width.saturating_sub(1) as usize;
        let Some(agent) = id.and_then(|id| {
            self.world
                .agents
                .items()
                .iter()
                .find(|agent| agent.id == id)
        }) else {
            let message = match self.world.agents {
                Load::Loading => format!("{} Loading agents…", self.spinner()),
                _ => "Select an agent.".into(),
            };
            buf.set_stringn(area.x, area.y + 1, message, width, theme::dim());
            return;
        };
        let full = area;
        let area = if self.details && !agent.unmanaged && area.width >= 90 {
            let side = (area.width / 3).clamp(30, 44);
            let pane = Rect {
                x: area.x + area.width - side,
                width: side,
                ..area
            };
            for y in area.y..area.y + area.height {
                buf[(pane.x - 1, y)]
                    .set_symbol("│")
                    .set_style(Style::default().fg(theme::SURFACE0).bg(theme::BASE));
            }
            let doc = screens::agent_details(&self.world, agent, side as usize - 3, self.spinner());
            self.pane(
                buf,
                &format!("details:{}", agent.id),
                Rect {
                    x: pane.x + 1,
                    width: side - 1,
                    ..pane
                },
                doc,
                false,
            );
            Rect {
                width: area.width - side - 2,
                ..area
            }
        } else {
            area
        };
        let width = area.width.saturating_sub(1) as usize;
        let header = screens::agent_header(&self.world, agent, width, self.spinner());
        let header_height = header.lines.len() as u16 + 1;
        self.pane(
            buf,
            "agent-header",
            Rect {
                height: header_height.min(area.height),
                ..area
            },
            header,
            false,
        );
        let rule_y = area.y + header_height - 1;
        buf.set_stringn(
            area.x,
            rule_y,
            "─".repeat(area.width as usize),
            area.width as usize,
            theme::fg(theme::SURFACE0),
        );
        if !agent.unmanaged && full.width >= 90 {
            let label = if self.details {
                " i hide details ▸ "
            } else {
                " ◂ i details "
            };
            let width = text::width(label) as u16;
            let x = area.x + area.width.saturating_sub(width + 1);
            buf.set_stringn(x, rule_y, label, width as usize, theme::fg(theme::OVERLAY1));
            self.hit(
                Rect {
                    x,
                    y: rule_y,
                    width,
                    height: 1,
                },
                Hit::Key('i'),
            );
        }
        // The composer grows with the draft (up to eight lines) and keeps the cursor in view.
        let composer = if agent.unmanaged {
            Vec::new()
        } else {
            self.composer_lines(agent, width)
        };
        let composer_height = if agent.unmanaged {
            0
        } else {
            composer.len() as u16 + 2
        };
        let body = Rect {
            y: area.y + header_height,
            height: area.height.saturating_sub(header_height + composer_height),
            ..area
        };
        let doc = match self.world.conversations.get(&agent.id) {
            None | Some(Load::Loading) => {
                let mut doc = Doc::new();
                doc.line(Line::from(Span::styled(
                    format!(" {} Loading the conversation…", self.spinner()),
                    theme::dim(),
                )));
                doc
            }
            Some(Load::Failed(reason)) => {
                let mut doc = Doc::new();
                doc.blank();
                let mut inner = Doc::new();
                inner.wrap(
                    &[text::run(reason.clone(), theme::soft())],
                    width.saturating_sub(4),
                );
                doc.card(
                    "no conversation to show",
                    theme::OVERLAY1,
                    false,
                    inner,
                    width,
                );
                doc
            }
            Some(Load::Ready(entries)) if entries.is_empty() => {
                let mut doc = Doc::new();
                doc.line(Line::from(Span::styled(" No messages yet.", theme::dim())));
                doc
            }
            Some(Load::Ready(entries)) => {
                let mut doc = Doc::new();
                doc.blank();
                doc.append(
                    self.cache
                        .render(entries, width, &self.expanded, self.spinner()),
                    0,
                );
                doc.blank();
                doc
            }
        };
        self.pane(buf, &format!("chat:{}", agent.id), body, doc, true);
        if composer_height > 0 {
            let y = body.y + body.height;
            buf.set_stringn(
                area.x,
                y,
                "─".repeat(area.width as usize),
                area.width as usize,
                theme::fg(if self.editing {
                    theme::ACCENT
                } else {
                    theme::SURFACE0
                }),
            );
            for (offset, line) in composer.iter().enumerate() {
                buf.set_line(area.x, y + 1 + offset as u16, line, area.width);
            }
            self.hit(
                Rect {
                    x: area.x,
                    y: y + 1,
                    width: area.width,
                    height: composer.len().max(1) as u16,
                },
                Hit::Composer,
            );
        }
    }

    /// Draw a document into a scrolling pane, registering its click targets.
    fn pane(&self, buf: &mut Buffer, key: &str, area: Rect, doc: Doc, follow_default: bool) {
        let height = area.height as usize;
        let total = doc.lines.len();
        let max_top = total.saturating_sub(height);
        let state = {
            let mut panes = self.panes.borrow_mut();
            let state = panes.entry(key.to_owned()).or_insert(PaneState {
                follow: follow_default,
                seen: total,
                ..PaneState::default()
            });
            if state.follow {
                state.top = max_top;
                state.unseen = 0;
            } else {
                state.top = state.top.min(max_top);
                if total > state.seen {
                    state.unseen += total - state.seen;
                }
            }
            state.seen = total;
            *state
        };
        let top = state.top;
        let lines = Rc::new(doc.lines);
        for (offset, line) in lines.iter().skip(top).take(height).enumerate() {
            buf.set_line(
                area.x,
                area.y + offset as u16,
                line,
                area.width.saturating_sub(1),
            );
        }
        for target in &doc.targets {
            if target.line >= top && target.line < top + height {
                let rect = Rect {
                    x: area.x + target.column,
                    y: area.y + (target.line - top) as u16,
                    width: target.width.min(area.width.saturating_sub(target.column)),
                    height: 1,
                };
                self.hit(rect, target.hit.clone());
            }
        }
        if let Some(selection) = &self.selection
            && selection.pane == key
        {
            let (start, end) = order(selection.anchor, selection.head);
            for line in start.0.max(top)..=end.0.min(top + height.saturating_sub(1)) {
                let y = area.y + (line - top) as u16;
                let from = if line == start.0 { start.1 } else { 0 };
                let to = if line == end.0 {
                    end.1
                } else {
                    area.width.saturating_sub(1)
                };
                for x in from..=to.min(area.width.saturating_sub(2)) {
                    buf[(area.x + x, y)].set_bg(theme::SELECTION_BG);
                }
            }
        }
        if total > height {
            scrollbar(
                buf,
                Rect {
                    x: area.x + area.width - 1,
                    ..area
                },
                top,
                total,
            );
        }
        if !state.follow && state.unseen > 0 && follow_default {
            let label = format!(" ↓ {} new lines · end ", state.unseen);
            let width = text::width(&label) as u16;
            let x = area.x + area.width.saturating_sub(width) / 2;
            let y = area.y + area.height.saturating_sub(1);
            buf.set_stringn(
                x,
                y,
                &label,
                width as usize,
                Style::default()
                    .fg(theme::CRUST)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            );
            self.hit(
                Rect {
                    x,
                    y,
                    width,
                    height: 1,
                },
                Hit::JumpLatest,
            );
        }
        self.frame.borrow_mut().panes.push(FramePane {
            key: key.to_owned(),
            rect: area,
            top,
            total,
            lines,
        });
    }

    /// A floating card for one graph subject, with a way to go to it.
    fn draw_popover(&self, buf: &mut Buffer, area: Rect, subject: &str) {
        let width = 72.min(area.width.saturating_sub(4));
        let inner = screens::peek(&self.world, subject, width as usize - 4, self.spinner());
        let mut doc = Doc::new();
        let title = match subject.split('/').next().unwrap_or("") {
            "agent" | "session" => "agent",
            "mission" => "mission",
            "attention" => "needs you",
            _ => "details",
        };
        doc.card(
            &format!("{title} · esc closes"),
            theme::ACCENT,
            false,
            inner,
            width as usize,
        );
        let height = (doc.lines.len() as u16).min(area.height.saturating_sub(2));
        let rect = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 3,
            width,
            height,
        };
        // Everything outside the card closes it.
        self.hit(area, Hit::Escape);
        buf.set_style(rect, Style::default().bg(theme::MANTLE));
        for (offset, line) in doc.lines.iter().take(height as usize).enumerate() {
            buf.set_line(rect.x, rect.y + offset as u16, line, rect.width);
        }
        self.hit(rect, Hit::Peek(subject.to_owned()));
        for target in &doc.targets {
            if (target.line as u16) < height {
                self.hit(
                    Rect {
                        x: rect.x + target.column,
                        y: rect.y + target.line as u16,
                        width: target.width,
                        height: 1,
                    },
                    target.hit.clone(),
                );
            }
        }
    }

    fn draw_terminal(&self, buf: &mut Buffer, area: Rect) {
        let Some(view) = &self.terminal else { return };
        let header = format!(" ← Return · Ctrl+\\   {}", view.title);
        buf.set_stringn(
            area.x,
            area.y,
            &header,
            area.width as usize,
            theme::strong(theme::ACCENT),
        );
        self.hit(Rect { height: 1, ..area }, Hit::Detach);
        let status = match (&view.ended, &view.stale) {
            (Some(reason), _) => format!("ended: {reason}"),
            (None, Some(reason)) if view.lines.is_empty() => format!("{reason}…"),
            (None, Some(reason)) => format!("not current, reconnecting: {reason}"),
            (None, None) => "Ctrl-C and Ctrl-D need a second press to reach the agent".into(),
        };
        buf.set_stringn(
            area.x,
            area.y + 1,
            format!(" {status}"),
            area.width as usize,
            theme::dim(),
        );
        let rows = area.height.saturating_sub(2) as usize;
        let skip = terminal_rows_skipped(view.lines.len(), rows, view.cursor);
        for (offset, line) in view.lines.iter().skip(skip).take(rows).enumerate() {
            buf.set_line(area.x, area.y + 2 + offset as u16, line, area.width);
        }
        // The cursor is drawn as an inverted cell where the screen says it is.
        if let Some((row, column)) = view.cursor
            && view.ended.is_none()
            && row >= skip
            && row < skip + rows
            && column < usize::from(area.width)
        {
            let position = (area.x + column as u16, area.y + 2 + (row - skip) as u16);
            if let Some(cell) = buf.cell_mut(position) {
                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            }
        }
    }

    /// The message box under a conversation, wrapped, newest lines last.
    fn composer_lines(&self, agent: &Agent, width: usize) -> Vec<Line<'static>> {
        let draft = self.drafts.get(&agent.id).cloned().unwrap_or_default();
        if draft.is_empty() && !self.editing {
            return vec![Line::from(vec![
                Span::styled("› ", theme::dim()),
                Span::styled(format!("Message {} · c or click", agent.name), theme::dim()),
            ])];
        }
        let style = if self.editing {
            theme::text()
        } else {
            theme::soft()
        };
        let mut lines = Vec::new();
        let paragraphs = draft.split('\n').collect::<Vec<_>>();
        for (index, paragraph) in paragraphs.iter().enumerate() {
            let mut runs = vec![text::run(paragraph.to_string(), style)];
            if self.editing && index == paragraphs.len() - 1 {
                runs.push(text::run("█", theme::fg(theme::ACCENT)));
            }
            let first = if index == 0 {
                text::run(
                    "› ",
                    if self.editing {
                        theme::strong(theme::ACCENT)
                    } else {
                        theme::dim()
                    },
                )
            } else {
                text::run("  ", theme::dim())
            };
            lines.extend(text::wrap(
                &runs,
                width.saturating_sub(1),
                &[first],
                &[text::run("  ", theme::dim())],
                None,
            ));
        }
        let keep = 8;
        if lines.len() > keep {
            lines.drain(..lines.len() - keep);
        }
        lines
    }

    fn draw_help(&self, buf: &mut Buffer, area: Rect) {
        let width = 64.min(area.width.saturating_sub(4));
        let mut inner = Doc::new();
        let w = width as usize - 4;
        inner.section("what the marks mean", None, w);
        for (glyph, color, meaning) in [
            (
                "◆",
                theme::PERSON,
                "a person is needed: you. Mauve is only ever this.",
            ),
            (
                self.spinner(),
                theme::WORKING,
                "working: an agent is producing output now",
            ),
            ("●", theme::IDLE, "idle: running, nothing to do"),
            ("◇", theme::WAITING, "unclaimed: ready, nobody has taken it"),
            ("▲", theme::FAULT, "stalled: an owner that is not moving"),
            ("✕", theme::FAULT, "broken or failed"),
            ("○", theme::QUIET, "stopped"),
            (
                "?",
                theme::QUIET,
                "not started by st, so st cannot see inside it",
            ),
            ("✉", theme::SAPPHIRE, "a Small Talk message"),
        ] {
            inner.lines(text::wrap(
                &[text::run(meaning, theme::soft())],
                w,
                &[text::run(format!(" {glyph}  "), theme::strong(color))],
                &[text::run("    ", theme::dim())],
                None,
            ));
        }
        inner.blank();
        inner.section("keys", None, w);
        for (key, meaning) in [
            ("1-4 or click", "switch tabs"),
            ("↑↓ j k or click", "select in the list"),
            ("wheel pgup pgdn", "scroll the pane under the pointer"),
            ("end", "jump to the newest message and follow it"),
            ("drag", "select text in one pane; release copies it"),
            ("o", "expand or collapse tool output"),
            ("c", "write: a message, feedback, a reply"),
            ("s", "hide the sidebar"),
            ("q", "quit"),
        ] {
            inner.line(Line::from(vec![
                Span::styled(format!(" {key:<18}"), theme::strong(theme::ACCENT)),
                Span::styled(meaning, theme::soft()),
            ]));
        }
        let mut doc = Doc::new();
        doc.card(
            "help · ? or esc closes",
            theme::ACCENT,
            false,
            inner,
            width as usize,
        );
        let height = (doc.lines.len() as u16).min(area.height.saturating_sub(2));
        let rect = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        buf.set_style(rect, Style::default().bg(theme::MANTLE));
        for (offset, line) in doc.lines.iter().take(height as usize).enumerate() {
            buf.set_line(rect.x, rect.y + offset as u16, line, rect.width);
        }
        self.hit(area, Hit::Help);
    }

    // ------------------------------------------------------------------- input

    fn select(&mut self, index: usize) {
        let count = self.ids().len();
        if count == 0 {
            return;
        }
        self.selected[self.tab] = index.min(count - 1);
        self.editing = false;
        self.chat = None;
        self.confirm = None;
        self.selection = None;
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab.min(TABS.len() - 1);
        self.editing = false;
        self.chat = None;
        self.popover = None;
        self.kdl = false;
        self.confirm = None;
        self.selection = None;
    }

    fn open(&mut self, id: &str) {
        let tab = if id.starts_with("attention/") {
            0
        } else if id.starts_with("mission/") {
            2
        } else {
            1
        };
        self.switch_tab(tab);
        if let Some(index) = self.ids().iter().position(|candidate| candidate == id) {
            self.select(index);
        }
    }

    fn scroll_pane(&self, key: &str, delta: isize) {
        let info = self.frame.borrow();
        let Some(pane) = info.panes.iter().find(|pane| pane.key == key) else {
            return;
        };
        let max_top = pane.total.saturating_sub(pane.rect.height as usize);
        let mut panes = self.panes.borrow_mut();
        let state = panes.entry(key.to_owned()).or_default();
        let top = (pane.top as isize + delta).clamp(0, max_top as isize) as usize;
        state.top = top;
        state.follow = top >= max_top && key.starts_with("chat:");
        if state.follow {
            state.unseen = 0;
        }
    }

    fn main_pane_key(&self) -> Option<String> {
        let info = self.frame.borrow();
        info.panes
            .iter()
            .filter(|pane| pane.key != "agent-header" && !pane.key.starts_with("details:"))
            .map(|pane| pane.key.clone())
            .next()
    }

    fn follow_latest(&self) {
        if let Some(key) = self.main_pane_key() {
            let mut panes = self.panes.borrow_mut();
            let state = panes.entry(key).or_default();
            state.follow = true;
            state.unseen = 0;
        }
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // An attached terminal gets every key first, Ctrl-C included.
        if self.terminal.is_some() && self.tab == 1 {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // Terminals send Ctrl+\\ as 0x1c, which crossterm reports as Ctrl+4.
                KeyCode::Char('\\' | '4') if control => {
                    self.effects.push(Effect::CloseTerminal);
                }
                KeyCode::Char(letter @ ('c' | 'd')) if control => {
                    let code = KeyCode::Char(letter);
                    if self.terminal_confirm.is_some_and(|(pending, at)| {
                        pending == code && at.elapsed() < Duration::from_secs(2)
                    }) {
                        self.terminal_confirm = None;
                        self.effects.push(Effect::TerminalKey(key));
                    } else {
                        self.terminal_confirm = Some((code, Instant::now()));
                        self.flash(format!(
                            "Press Ctrl-{} again within 2s to send it to the agent",
                            letter.to_ascii_uppercase()
                        ));
                    }
                }
                _ => self.effects.push(Effect::TerminalKey(key)),
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            if self.editing {
                self.editing = false;
            } else {
                self.quit = true;
            }
            return;
        }
        if key.code == KeyCode::F(1) {
            self.help = !self.help;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if let Some((fields, focus)) = self.new_mission.as_mut() {
            let focus_now = *focus;
            match key.code {
                KeyCode::Esc => self.new_mission = None,
                KeyCode::Tab => *focus = (focus_now + 1) % 4,
                KeyCode::BackTab => *focus = (focus_now + 3) % 4,
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                {
                    fields[focus_now].push('\n')
                }
                KeyCode::Enter if focus_now < 3 => *focus = focus_now + 1,
                KeyCode::Enter => self.create_launch(),
                KeyCode::Backspace => {
                    fields[focus_now].pop();
                }
                KeyCode::Char(character) => {
                    fields[focus_now].push(character);
                    if focus_now == 0 && fields[2].is_empty() {
                        // Suggest a mission id from the title as it is typed.
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(chat) = self.chat.clone().filter(|chat| chat.editing) {
            let key_id = format!("chat:{}", chat.item);
            match key.code {
                KeyCode::Esc => {
                    if let Some(chat) = &mut self.chat {
                        chat.editing = false;
                    }
                }
                KeyCode::Enter => self.submit_chat(),
                KeyCode::Backspace => {
                    self.drafts.entry(key_id).or_default().pop();
                }
                KeyCode::Char(character) => self.drafts.entry(key_id).or_default().push(character),
                _ => {}
            }
            return;
        }
        if let Some(subject) = self.popover.clone() {
            self.popover = None;
            match key.code {
                KeyCode::Char('g') | KeyCode::Enter => self.open(&subject),
                KeyCode::Char('t') if subject.starts_with("agent/") => {
                    self.open(&subject);
                    self.editing = true;
                }
                _ => {}
            }
            return;
        }
        if self.editing {
            let key_id = self.draft_key().unwrap_or_default();
            let newline = (key.code == KeyCode::Enter
                && key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT))
                || (key.code == KeyCode::Char('j')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if newline {
                self.drafts.entry(key_id).or_default().push('\n');
                return;
            }
            match key.code {
                KeyCode::Esc => self.editing = false,
                KeyCode::Enter => self.submit(),
                KeyCode::Backspace => {
                    self.drafts.entry(key_id).or_default().pop();
                }
                KeyCode::Char(character) => self.drafts.entry(key_id).or_default().push(character),
                _ => {}
            }
            return;
        }
        if let Some(action) = self.confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.confirm = None;
                    self.act(action);
                }
                _ => self.confirm = None,
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(digit @ '1'..='4') => self.switch_tab(digit as usize - '1' as usize),
            KeyCode::Tab => self.switch_tab((self.tab + 1) % TABS.len()),
            KeyCode::BackTab => self.switch_tab((self.tab + TABS.len() - 1) % TABS.len()),
            KeyCode::Up | KeyCode::Char('k') => {
                self.select(self.selected[self.tab].saturating_sub(1))
            }
            KeyCode::Down | KeyCode::Char('j')
                if !(self.tab == 0 && self.current_kind() == Some("revision"))
                    || key.code == KeyCode::Down =>
            {
                self.select(self.selected[self.tab] + 1)
            }
            KeyCode::PageUp => self.scroll_main(-(self.page() as isize)),
            KeyCode::PageDown => self.scroll_main(self.page() as isize),
            KeyCode::Home => self.scroll_main(-100_000),
            KeyCode::End => {
                self.follow_latest();
                self.scroll_main(100_000);
            }
            KeyCode::Char('s') => self.sidebar = !self.sidebar,
            KeyCode::Char('x') if self.tab == 2 => self.system = !self.system,
            KeyCode::Char('o') if self.tab == 1 => self.toggle_all_tools(),
            KeyCode::Char('i') if self.tab == 1 => self.details = !self.details,
            KeyCode::Char('t') if matches!(self.tab, 1 | 2) => {
                let id = self.selected_id();
                self.tree = !self.tree;
                if let Some(index) =
                    id.and_then(|id| self.ids().iter().position(|candidate| *candidate == id))
                {
                    self.selected[self.tab] = index;
                }
                self.flash(if self.tree {
                    "Tree view · t for groups"
                } else {
                    "Grouped view · t for the tree"
                });
            }
            KeyCode::Enter if self.tab == 1 => self.open_terminal(),
            KeyCode::Char('n') if self.tab == 2 => {
                self.new_mission = Some((Default::default(), 0));
            }
            KeyCode::Enter if self.tab == 2 => {
                if let Some(decision) = self.selected_id().and_then(|id| {
                    self.world
                        .missions
                        .items()
                        .iter()
                        .find(|mission| mission.id == id)
                        .and_then(|mission| mission.decision.clone())
                }) {
                    self.open(&decision);
                }
            }
            KeyCode::Esc => {
                if self.chat.is_some() {
                    self.chat = None;
                } else {
                    self.selection = None;
                }
            }
            KeyCode::Char(character) => self.action_key(character),
            _ => {}
        }
    }

    fn page(&self) -> usize {
        self.main_pane_key()
            .and_then(|key| {
                self.frame
                    .borrow()
                    .panes
                    .iter()
                    .find(|pane| pane.key == key)
                    .map(|pane| pane.rect.height as usize)
            })
            .unwrap_or(10)
            .saturating_sub(2)
            .max(1)
    }

    fn scroll_main(&self, delta: isize) {
        if let Some(key) = self.main_pane_key() {
            self.scroll_pane(&key, delta);
        }
    }

    fn current_kind(&self) -> Option<&'static str> {
        let id = self.attention_focus()?;
        self.world
            .attention
            .items()
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.kind.word())
    }

    fn toggle_all_tools(&mut self) {
        let Some(id) = self.selected_id() else { return };
        let Some(Load::Ready(entries)) = self.world.conversations.get(&id) else {
            return;
        };
        let tools = entries
            .iter()
            .filter(|entry| matches!(entry.body, Body::Tool { .. }))
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        if tools.iter().all(|tool| self.expanded.contains(tool)) {
            for tool in tools {
                self.expanded.remove(&tool);
            }
        } else {
            self.expanded.extend(tools);
        }
    }

    fn action_key(&mut self, key: char) {
        if self.tab == 2 {
            match key {
                'k' => {
                    self.kdl = !self.kdl;
                    return;
                }
                'R' | 'X' => {
                    self.mission_action(key);
                    return;
                }
                'r' if self.attention_focus().is_none() => {
                    self.mission_action(key);
                    return;
                }
                _ => {}
            }
        }
        match self.tab {
            0 | 2 if self.attention_focus().is_some() => {
                let Some(kind) = self.current_kind() else {
                    return;
                };
                match (kind, key) {
                    (_, 'g') => self.go_to_subject(),
                    (_, 't') => self.start_chat(),
                    ("message", 'l') => {
                        if let Some(id) = self.selected_id() {
                            self.snoozed.insert(id);
                            let index = self.selected[0];
                            self.select(index);
                            self.flash("Put off until later · demo, this machine only");
                        }
                    }
                    ("review" | "feedback" | "launch" | "message" | "revision" | "request", 'c') => {
                        self.editing = true
                    }
                    ("review" | "feedback" | "launch" | "revision", 'a') => {
                        self.confirm = Some('a')
                    }
                    ("launch", 'd') | ("revision", 'j') | ("fault" | "request", 'r') => {
                        self.confirm = Some(key)
                    }
                    ("message", 'm') => self.act('m'),
                    _ => {}
                }
            }
            1 if key == 'i' => self.details = !self.details,
            1 if key == 'c'
                && self.world.agents.items().iter().any(|agent| {
                    Some(&agent.id) == self.selected_id().as_ref() && !agent.unmanaged
                }) =>
            {
                self.editing = true;
            }
            _ => {}
        }
    }

    /// Retry, restart or cancel from a stalled mission's "what you can do" card.
    fn mission_action(&mut self, key: char) {
        let what = match key {
            'r' => "Retry the step",
            'R' => "Restart the agent",
            _ => "Cancel this run",
        };
        if key == 'X' {
            self.confirm = Some('X');
            self.flash("Cancel this run and stop its work? y to confirm");
            return;
        }
        if self.live {
            // st's client API lets a person neither retry a step nor restart an agent yet.
            let hint = match key {
                'r' => "st work retry STEP",
                _ => "st agents start AGENT",
            };
            self.flash(format!(
                "{what}: st does not let stui do this yet · use {hint}"
            ));
        } else {
            self.flash(format!("{what} · demo: nothing was sent"));
        }
    }

    fn open_terminal(&mut self) {
        let Some(agent) = self.selected_id().and_then(|id| {
            self.world
                .agents
                .items()
                .iter()
                .find(|agent| agent.id == id)
                .cloned()
        }) else {
            return;
        };
        if !agent.terminal {
            self.flash(format!("{} has no terminal to open", agent.name));
            return;
        }
        if self.live {
            self.effects.push(Effect::OpenTerminal { agent: agent.id });
            self.flash("Opening the terminal…");
        } else {
            self.terminal = Some(TerminalView {
                title: format!("{} · demo terminal", agent.name),
                name: agent.name.clone(),
                lines: demo::terminal(&agent.name),
                cursor: None,
                stale: None,
                ended: None,
            });
        }
    }

    fn create_launch(&mut self) {
        let Some((fields, _)) = self.new_mission.clone() else {
            return;
        };
        if let Some(missing) = fields.iter().position(|field| field.trim().is_empty()) {
            if let Some((_, focus)) = self.new_mission.as_mut() {
                *focus = missing;
            }
            self.flash(format!(
                "Fill in {}",
                screens::NEW_MISSION_FIELDS[missing].0
            ));
            return;
        }
        let [title, request, mission, workspace] = fields;
        self.new_mission = None;
        if self.live {
            self.effects.push(Effect::CreateLaunch {
                title,
                request,
                mission,
                workspace,
            });
            self.flash("Creating the launch…");
        } else {
            self.flash("Launch created · demo: nothing was sent");
        }
    }

    fn current_item(&self) -> Option<&Attention> {
        let id = self.attention_focus()?;
        self.world
            .attention
            .items()
            .iter()
            .find(|item| item.id == id)
    }

    /// Go to what a Home item is about: its mission, or else its agent.
    fn go_to_subject(&mut self) {
        let Some(item) = self.current_item() else {
            return;
        };
        if let Some(target) = item.mission.clone().or_else(|| item.agent.clone()) {
            self.open(&target);
        }
    }

    /// Who to talk to about an item: the agent involved, or the chief of staff.
    fn chat_target(&self, item: &Attention) -> Option<(String, String)> {
        let agents = self.world.agents.items();
        let named = |id: &str| {
            agents
                .iter()
                .find(|agent| agent.id == id)
                .map(|agent| (agent.id.clone(), agent.name.clone()))
        };
        item.agent.as_deref().and_then(named).or_else(|| {
            agents
                .iter()
                .find(|agent| agent.id.ends_with("/cos") || agent.name == "Chief of Staff")
                .map(|agent| (agent.id.clone(), agent.name.clone()))
        })
    }

    fn start_chat(&mut self) {
        let Some(item) = self.current_item().cloned() else {
            return;
        };
        match self.chat_target(&item) {
            Some((to, to_name)) => {
                self.chat = Some(ChatState {
                    item: item.id.clone(),
                    to,
                    to_name,
                    editing: true,
                });
                self.editing = false;
                self.confirm = None;
            }
            None => self.flash("Nobody to chat with about this yet"),
        }
    }

    fn submit_chat(&mut self) {
        let Some(chat) = self.chat.clone() else {
            return;
        };
        let key = format!("chat:{}", chat.item);
        let text = self.drafts.get(&key).cloned().unwrap_or_default();
        if text.trim().is_empty() {
            self.flash("Write something first");
            return;
        }
        let Some(item) = self
            .world
            .attention
            .items()
            .iter()
            .find(|item| item.id == chat.item)
            .cloned()
        else {
            return;
        };
        let title = format!("About: {}", item.title);
        self.drafts.remove(&key);
        if let Some(state) = &mut self.chat {
            state.editing = false;
        }
        if self.live {
            let mut context = format!("{text}\n\n---\nThis is about {} ({}", item.title, item.id);
            if let Some(mission) = &item.mission {
                context.push_str(&format!(", mission {mission}"));
            }
            context.push(')');
            self.effects.push(Effect::Discuss {
                to: chat.to,
                title,
                text: context,
            });
            self.flash("Sending…");
            return;
        }
        let at = chrono::Local::now().format("%H:%M").to_string();
        if let Some(Load::Ready(entries)) = self.world.conversations.get_mut(&chat.to) {
            entries.push(Entry {
                id: format!("about-{}", entries.len()),
                at: at.clone(),
                body: Body::Mail {
                    from: "you".into(),
                    to: chat.to_name.clone(),
                    subject: title.clone(),
                    body: text,
                },
            });
            entries.push(Entry {
                id: format!("about-{}", entries.len()),
                at,
                body: Body::Mail {
                    from: chat.to_name.clone(),
                    to: "you".into(),
                    subject: title,
                    body: "Good question. Here is what I know, and what I would need from you to go on. (demo reply)".into(),
                },
            });
        }
        self.flash("Sent · demo: nothing left this machine");
    }

    fn submit(&mut self) {
        let Some(id) = self.draft_key() else { return };
        let draft = self.drafts.get(&id).cloned().unwrap_or_default();
        if draft.trim().is_empty() {
            self.flash("Write something first");
            return;
        }
        self.editing = false;
        if self.live {
            let effect = match self.tab {
                1 => Some(Effect::Send {
                    agent: id.clone(),
                    text: draft,
                }),
                _ => match self
                    .world
                    .attention
                    .items()
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.kind.clone())
                {
                    Some(AttentionKind::Review { .. }) => Some(Effect::Attention {
                        id: id.clone(),
                        action: "review.reject".into(),
                        reason: Some(draft),
                    }),
                    Some(AttentionKind::Launch { .. }) => Some(Effect::LaunchRevise {
                        id: id.clone(),
                        feedback: draft,
                    }),
                    Some(AttentionKind::Request { from_id, .. }) => {
                        let title = self
                            .current_item()
                            .map(|item| item.title.clone())
                            .unwrap_or_default();
                        Some(Effect::Discuss {
                            to: from_id,
                            title: format!("Re: {title}"),
                            text: draft,
                        })
                    }
                    Some(AttentionKind::Message { from, .. }) => Some(Effect::Reply {
                        id: id.clone(),
                        to: from,
                        text: draft,
                    }),
                    Some(AttentionKind::Revision { .. }) => self
                        .current_item()
                        .and_then(|item| {
                            self.chat_target(item)
                                .map(|(to, _)| (to, item.title.clone()))
                        })
                        .map(|(to, title)| Effect::Discuss {
                            to,
                            title: format!("Changes to: {title}"),
                            text: draft,
                        }),
                    _ => None,
                },
            };
            match effect {
                Some(effect) => {
                    self.effects.push(effect);
                    self.drafts.remove(&id);
                    self.follow_latest();
                    self.flash("Sending…");
                }
                None => self.flash("This needs the st CLI for now"),
            }
            return;
        }
        match self.tab {
            1 => {
                let name = self
                    .world
                    .agents
                    .items()
                    .iter()
                    .find(|agent| agent.id == id)
                    .map(|agent| agent.name.clone())
                    .unwrap_or_default();
                if let Some(Load::Ready(entries)) = self.world.conversations.get_mut(&id) {
                    entries.push(Entry {
                        id: format!("you-{}", entries.len()),
                        at: chrono::Local::now().format("%H:%M").to_string(),
                        body: Body::Mail {
                            from: "you".into(),
                            to: name,
                            subject: String::new(),
                            body: draft,
                        },
                    });
                }
                self.drafts.remove(&id);
                self.follow_latest();
                self.flash("Sent · demo: nothing left this machine");
            }
            _ => {
                self.drafts.remove(&id);
                self.resolve(&id, "Sent to the agent");
            }
        }
    }

    fn act(&mut self, action: char) {
        if action == 'v' {
            if let Some(id) = self.revoke.take() {
                if self.live {
                    self.effects.push(Effect::RevokeDevice { id });
                    self.flash("Revoking…");
                } else {
                    if let Load::Ready(devices) = &mut self.world.devices {
                        devices.retain(|device| device.id != id);
                    }
                    self.flash("Device revoked · demo: nothing was sent");
                }
            }
            return;
        }
        if action == 'X' {
            let Some(mission) = self.selected_id().filter(|_| self.tab == 2) else {
                return;
            };
            if self.live {
                self.effects.push(Effect::CancelRun { mission });
                self.flash("Cancelling…");
            } else {
                self.flash("Run cancelled · demo: nothing was sent");
            }
            return;
        }
        let Some(id) = self.attention_focus() else {
            return;
        };
        if self.live {
            let kind = self.current_kind().unwrap_or("");
            let name = match (kind, action) {
                ("review", 'a') => "review.approve",
                ("launch", 'a') => "launch.approve",
                ("launch", 'd') => "launch.cancel",
                ("revision", 'a') => "mission.approve-revision",
                ("revision", 'j') => "mission.cancel-revision",
                ("fault" | "request", 'r') => "attention.resolve",
                ("message", 'm') => "message.read",
                _ => return,
            };
            let offered = self
                .world
                .attention
                .items()
                .iter()
                .find(|item| item.id == id)
                .is_some_and(|item| item.actions.iter().any(|action| action == name));
            if offered {
                self.effects.push(Effect::Attention {
                    id,
                    action: name.into(),
                    reason: None,
                });
                self.flash("Sending…");
            } else {
                self.flash(format!("st does not offer {name} here"));
            }
            return;
        }
        let message = match action {
            'a' => "Approved",
            'd' => "Launch cancelled",
            'j' => "Revision rejected",
            'r' => "Marked resolved",
            'm' => "Marked read",
            _ => "Done",
        };
        self.resolve(&id, message);
    }

    fn resolve(&mut self, id: &str, message: &str) {
        if let Load::Ready(items) = &mut self.world.attention {
            items.retain(|item| item.id != id);
        }
        for mission in self.world.missions.items().to_vec() {
            if mission.decision.as_deref() == Some(id)
                && let Load::Ready(missions) = &mut self.world.missions
                && let Some(mission) = missions
                    .iter_mut()
                    .find(|candidate| candidate.id == mission.id)
            {
                mission.decision = None;
                mission.word = Word::Working;
                for step in &mut mission.steps {
                    if step.state == StepState::NeedsYou {
                        step.state = StepState::Working;
                        step.note = Some("you answered; the agent is on it".into());
                    }
                }
            }
        }
        if self.tab == 0 {
            self.select(self.selected[0]);
        }
        self.flash(format!("{message} · demo: nothing was sent"));
    }

    pub fn mouse(&mut self, mouse: MouseEvent) {
        if self.help {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = {
                    let info = self.frame.borrow();
                    info.hits
                        .iter()
                        .rev()
                        .find(|(rect, _)| contains(*rect, mouse.column, mouse.row))
                        .map(|(_, hit)| hit.clone())
                };
                self.selection = None;
                if let Some(hit) = hit {
                    self.click(hit);
                    return;
                }
                if let Some((key, line, column)) = self.pane_point(mouse.column, mouse.row) {
                    self.selection = Some(Selection {
                        pane: key,
                        anchor: (line, column),
                        head: (line, column),
                    });
                    self.dragging = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                let edge = {
                    let info = self.frame.borrow();
                    self.selection.as_ref().and_then(|selection| {
                        info.panes
                            .iter()
                            .find(|pane| pane.key == selection.pane)
                            .map(|pane| (pane.rect, pane.top))
                    })
                };
                if let (Some(selection), Some((rect, top))) = (self.selection.clone(), edge) {
                    if mouse.row < rect.y {
                        self.scroll_pane(&selection.pane, -1);
                    } else if mouse.row >= rect.y + rect.height {
                        self.scroll_pane(&selection.pane, 1);
                    }
                    let row = mouse
                        .row
                        .clamp(rect.y, rect.y + rect.height.saturating_sub(1));
                    let column = mouse
                        .column
                        .clamp(rect.x, rect.x + rect.width.saturating_sub(2))
                        - rect.x;
                    if let Some(selection) = &mut self.selection {
                        selection.head = (top + (row - rect.y) as usize, column);
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                if let Some(selection) = &self.selection {
                    if selection.anchor == selection.head {
                        self.selection = None;
                    } else if let Some(copied) = self.selected_text() {
                        let count = copied.lines().count();
                        copy(&copied);
                        self.flash(format!(
                            "Copied {count} line{}",
                            if count == 1 { "" } else { "s" }
                        ));
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -3
                } else {
                    3
                };
                let (in_sidebar, pane) = {
                    let info = self.frame.borrow();
                    (
                        contains(info.sidebar, mouse.column, mouse.row),
                        info.panes
                            .iter()
                            .find(|pane| contains(pane.rect, mouse.column, mouse.row))
                            .map(|pane| pane.key.clone()),
                    )
                };
                if in_sidebar {
                    let (lines, height) = {
                        let info = self.frame.borrow();
                        (info.sidebar_lines, info.sidebar_height)
                    };
                    let mut tops = self.list_top.borrow_mut();
                    tops[self.tab] = (tops[self.tab] as isize + delta)
                        .clamp(0, lines.saturating_sub(height) as isize)
                        as usize;
                } else if let Some(key) = pane {
                    self.scroll_pane(&key, delta);
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, hit: Hit) {
        match hit {
            Hit::Tab(tab) => self.switch_tab(tab),
            Hit::Row(index) => self.select(index),
            Hit::Key(key) if self.popover.is_some() => {
                let subject = self.popover.take().unwrap_or_default();
                match key {
                    'g' => self.open(&subject),
                    't' if subject.starts_with("agent/") => {
                        self.open(&subject);
                        self.editing = true;
                    }
                    _ => {}
                }
            }
            Hit::Key('\t') => {
                if let Some((_, focus)) = self.new_mission.as_mut() {
                    *focus = (*focus + 1) % 4;
                }
            }
            Hit::Key(key) => {
                if key == 'y' {
                    if let Some(action) = self.confirm.take() {
                        self.act(action);
                    }
                } else {
                    self.confirm = None;
                    self.action_key(key);
                }
            }
            Hit::Enter if self.new_mission.is_some() => self.create_launch(),
            Hit::Enter => {
                if self.chat.is_some() {
                    self.submit_chat()
                } else {
                    self.submit()
                }
            }
            Hit::Escape if self.new_mission.is_some() => self.new_mission = None,
            Hit::Escape => {
                if self.popover.take().is_none() {
                    self.editing = false;
                    self.confirm = None;
                    if let Some(chat) = &mut self.chat {
                        if chat.editing {
                            chat.editing = false;
                        } else {
                            self.chat = None;
                        }
                    }
                }
            }
            Hit::ToggleTool(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
            }
            Hit::JumpLatest => self.follow_latest(),
            Hit::Composer => {
                if let Some(chat) = &mut self.chat {
                    chat.editing = true;
                } else {
                    self.editing = true;
                }
            }
            Hit::Help => self.help = !self.help,
            Hit::Open(id) => self.open(&id),
            Hit::Field(index) => {
                if let Some((_, focus)) = self.new_mission.as_mut() {
                    *focus = index;
                }
            }
            Hit::Revoke(id) => {
                self.revoke = Some(id);
                self.confirm = Some('v');
                self.flash("Revoke this device? y to confirm");
            }
            Hit::Detach => {
                if self.live {
                    self.effects.push(Effect::CloseTerminal);
                } else {
                    self.terminal = None;
                }
            }
            Hit::Peek(id) => {
                if self.popover.as_deref() != Some(id.as_str()) {
                    self.popover = Some(id);
                }
            }
        }
    }

    fn pane_point(&self, column: u16, row: u16) -> Option<(String, usize, u16)> {
        let info = self.frame.borrow();
        let pane = info
            .panes
            .iter()
            .find(|pane| contains(pane.rect, column, row))?;
        Some((
            pane.key.clone(),
            pane.top + (row - pane.rect.y) as usize,
            column - pane.rect.x,
        ))
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let info = self.frame.borrow();
        let pane = info.panes.iter().find(|pane| pane.key == selection.pane)?;
        let (start, end) = order(selection.anchor, selection.head);
        let mut out = Vec::new();
        for line in start.0..=end.0.min(pane.lines.len().saturating_sub(1)) {
            let plain = text::plain(&pane.lines[line]);
            let from = if line == start.0 { start.1 as usize } else { 0 };
            let to = if line == end.0 {
                end.1 as usize + 1
            } else {
                usize::MAX
            };
            out.push(columns(&plain, from, to).trim_end().to_owned());
        }
        Some(out.join("\n"))
    }

    // -------------------------------------------------------------------- demo

    pub fn step_demo(&mut self) {
        let Some(demo) = &mut self.demo else { return };
        let elapsed = demo.started.elapsed();
        if !demo.loaded {
            let full = demo::world();
            let stage = elapsed.as_millis() / 350;
            if stage >= 1 {
                self.world.link = Link::Live;
                self.world.attention = full.attention.clone();
                self.world.quiet_missions = full.quiet_missions;
            }
            if stage >= 2 {
                self.world.agents = full.agents.clone();
            }
            if stage >= 3 {
                self.world.missions = full.missions.clone();
                self.world.machines = full.machines.clone();
                self.world.worktrees = full.worktrees.clone();
                self.world.conversations = full.conversations;
                if let Some(Load::Ready(entries)) = self
                    .world
                    .conversations
                    .get_mut("agent/fleet/harbor/reviewer")
                {
                    entries.push(Entry {
                        id: "h3".into(),
                        at: "09:14".into(),
                        body: Body::Tool {
                            title: "$ cargo test -p harbor --test retry".into(),
                            state: ToolState::Running,
                            output: vec!["compiling harbor v0.9.0".into()],
                        },
                    });
                }
                demo.loaded = true;
            }
            return;
        }
        let viewing = (self.tab == 1)
            .then(|| {
                let ids = screens::agent_order(&self.world)
                    .into_iter()
                    .map(|agent| agent.id.clone())
                    .collect::<Vec<_>>();
                ids.get(self.selected[1]).cloned()
            })
            .flatten();
        let Some(demo) = &mut self.demo else { return };
        let now = Instant::now();
        match viewing.as_deref() {
            Some("agent/fleet/cos") if demo.cos_seen.is_none() => demo.cos_seen = Some(now),
            Some("agent/fleet/harbor/reviewer") if demo.harbor_seen.is_none() => {
                demo.harbor_seen = Some(now)
            }
            _ => {}
        }
        let cos = demo
            .cos_seen
            .map(|at| at.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        let harbor = demo
            .harbor_seen
            .map(|at| at.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        if cos > 1.5 {
            let words = demo::STREAM.split(' ').collect::<Vec<_>>();
            let want = (((cos - 1.5) * 6.0) as usize).min(words.len());
            if want > demo.streamed
                && let Some(Load::Ready(entries)) =
                    self.world.conversations.get_mut("agent/fleet/cos")
            {
                let text = words[..want].join(" ");
                if let Some(entry) = entries.iter_mut().find(|entry| entry.id == "stream") {
                    entry.body = Body::Assistant(text);
                } else {
                    entries.push(Entry {
                        id: "stream".into(),
                        at: "09:50".into(),
                        body: Body::Assistant(text),
                    });
                }
                demo.streamed = want;
            }
        }
        if harbor > 3.0 && !demo.tool_done {
            demo.tool_done = true;
            if let Some(Load::Ready(entries)) = self
                .world
                .conversations
                .get_mut("agent/fleet/harbor/reviewer")
                && let Some(entry) = entries.iter_mut().find(|entry| entry.id == "h3")
            {
                entry.body = Body::Tool {
                    title: "$ cargo test -p harbor --test retry".into(),
                    state: ToolState::Ok,
                    output: vec![
                        "compiling harbor v0.9.0".into(),
                        "running 3 tests".into(),
                        "test retry_after_is_milliseconds ... ok".into(),
                        "test paused_clock_backoff ... ok".into(),
                        "test gives_up_after_five ... ok".into(),
                        "".into(),
                        "test result: ok. 3 passed; 0 failed".into(),
                    ],
                };
            }
        }
        if cos > 8.0 && !demo.mail {
            demo.mail = true;
            if let Some(Load::Ready(entries)) = self.world.conversations.get_mut("agent/fleet/cos")
            {
                entries.push(demo::late_mail());
            }
        }
    }
}

fn order(a: (usize, u16), b: (usize, u16)) -> ((usize, u16), (usize, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// The part of `line` between two display columns.
fn columns(line: &str, from: usize, to: usize) -> String {
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

fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = span.width();
        if used + span_width <= width {
            out.push(span.clone());
            used += span_width;
        } else {
            let room = width.saturating_sub(used);
            if room > 0 {
                out.push(Span::styled(
                    text::truncate(&span.content, room),
                    span.style,
                ));
            }
            break;
        }
    }
    out
}

fn scrollbar(buf: &mut Buffer, area: Rect, top: usize, total: usize) {
    let height = area.height as usize;
    if total <= height || height == 0 {
        return;
    }
    let thumb = (height * height / total).max(1);
    let max_top = total - height;
    let start = (height - thumb) * top / max_top.max(1);
    for row in 0..height {
        let on = row >= start && row < start + thumb;
        buf[(area.x, area.y + row as u16)]
            .set_symbol(if on { "┃" } else { " " })
            .set_fg(theme::OVERLAY0);
    }
}

/// Copy through OSC 52, which kitty, iTerm2, WezTerm and tmux (with set-clipboard) accept.
fn copy(text: &str) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(TABLE[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
    let _ = stdout.flush();
}

// ------------------------------------------------------------------------ run

struct Guard;

/// How many of a terminal's rows to skip so it fits the pane. A terminal taller than the pane
/// shows its bottom, where the prompt usually is, unless that would hide the cursor; then the
/// cursor's row is the pane's last.
fn terminal_rows_skipped(lines: usize, rows: usize, cursor: Option<(usize, usize)>) -> usize {
    let bottom = lines.saturating_sub(rows);
    match cursor {
        Some((row, _)) if row < bottom => (row + 1).saturating_sub(rows),
        _ => bottom,
    }
}

/// A flag set by SIGINT, SIGTERM or SIGHUP, so the loop exits and `Guard` restores the terminal.
fn stop_flag() -> Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, stopping.clone())?;
    }
    Ok(stopping)
}

impl Guard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// `stui --demo`: the new interface on invented data. `--dump` prints one frame as text.
pub fn run_demo(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "--dump") {
        return dump(args);
    }
    let _guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let mut ui = Ui::new(demo::loading());
    ui.demo = Some(Demo {
        started: Instant::now(),
        loaded: false,
        streamed: 0,
        mail: false,
        tool_done: false,
        cos_seen: None,
        harbor_seen: None,
    });
    let started = Instant::now();
    let stopping = stop_flag()?;
    while !ui.quit
        && !stopping.load(std::sync::atomic::Ordering::Relaxed)
        && !crate::stdin_hung_up()
    {
        ui.tick = (started.elapsed().as_millis() / 100) as u64;
        ui.step_demo();
        for effect in std::mem::take(&mut ui.effects) {
            match effect {
                Effect::CloseTerminal => ui.terminal = None,
                Effect::TerminalKey(_) => ui.flash("demo: keys are not sent"),
                _ => {}
            }
        }
        if ui
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(3))
        {
            ui.flash = None;
        }
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        terminal.draw(|frame| ui.render(frame))?;
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        if event::poll(Duration::from_millis(80))? {
            // Drain everything queued so a fast wheel does not lag behind.
            loop {
                match event::read()? {
                    Event::Key(key) => ui.key(key),
                    Event::Mouse(mouse) => ui.mouse(mouse),
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn dump(args: &[String]) -> Result<()> {
    let width = arg(args, "--width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(140);
    let height = arg(args, "--height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(44);
    let mut ui = Ui::new(if args.iter().any(|arg| arg == "--loading") {
        demo::loading()
    } else {
        demo::world()
    });
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    // Keys: each character is a key; "\n" is Enter, "<esc>", "<end>", "<pgdn>", "<pgup>", "<down>".
    if let Some(keys) = arg(args, "--keys") {
        let mut rest = keys.as_str();
        while !rest.is_empty() {
            terminal.draw(|frame| ui.render(frame))?;
            let (code, len) =
                if let Some(end) = rest.strip_prefix('<').and_then(|tail| tail.find('>')) {
                    let name = &rest[1..=end];
                    let code = match name {
                        "esc" => KeyCode::Esc,
                        "end" => KeyCode::End,
                        "pgdn" => KeyCode::PageDown,
                        "pgup" => KeyCode::PageUp,
                        "down" => KeyCode::Down,
                        "up" => KeyCode::Up,
                        "enter" => KeyCode::Enter,
                        "tab" => KeyCode::Tab,
                        _ => KeyCode::Null,
                    };
                    (code, end + 2)
                } else {
                    let character = rest.chars().next().unwrap_or(' ');
                    (KeyCode::Char(character), character.len_utf8())
                };
            ui.key(KeyEvent::new(code, KeyModifiers::NONE));
            rest = &rest[len..];
        }
    }
    if let Some(click) = arg(args, "--click") {
        terminal.draw(|frame| ui.render(frame))?;
        let mut parts = click.split(',').filter_map(|v| v.parse::<u16>().ok());
        if let (Some(column), Some(row)) = (parts.next(), parts.next()) {
            ui.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
        }
    }
    terminal.draw(|frame| ui.render(frame))?;
    // Draw twice so panes that follow the end settle.
    terminal.draw(|frame| ui.render(frame))?;
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..buffer.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buffer[(x, y)].symbol();
            skip = text::width(symbol).saturating_sub(1);
            line.push_str(symbol);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    let _ = terminal.backend_mut().flush();
    print!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ui: &Ui, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn loading_never_says_empty() {
        let mut ui = Ui::new(demo::loading());
        for tab in 0..5 {
            ui.tab = tab;
            let screen = frame(&ui, 120, 30).join("\n");
            assert!(!screen.contains("No agents"), "tab {tab}");
            assert!(!screen.contains("Nothing needs you"), "tab {tab}");
            assert!(
                screen.contains("Loading") || screen.contains("Checking"),
                "tab {tab}: {screen}"
            );
        }
    }

    #[test]
    fn a_diverged_host_says_so_instead_of_live() {
        let mut world = demo::world();
        let top = |world: &World| frame(&Ui::new(world.clone()), 140, 30)[0].clone();
        assert!(top(&world).contains("● live"));
        world.diverged = vec!["harbor".into()];
        let header = top(&world);
        assert!(header.contains("⚠ diverged"), "{header}");
        assert!(!header.contains("live"), "{header}");
    }

    #[test]
    fn every_tab_is_clickable() {
        let mut ui = Ui::new(demo::world());
        frame(&ui, 140, 40);
        let tabs = ui
            .frame
            .borrow()
            .hits
            .iter()
            .filter(|(_, hit)| matches!(hit, Hit::Tab(_)))
            .map(|(rect, hit)| (*rect, hit.clone()))
            .collect::<Vec<_>>();
        assert_eq!(tabs.len(), TABS.len());
        let (rect, _) = tabs[3];
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(ui.tab, 3);
    }

    #[test]
    fn a_conversation_follows_new_lines_until_scrolled_up() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        let cos = ui
            .ids()
            .iter()
            .position(|id| id == "agent/fleet/cos")
            .unwrap();
        ui.select(cos);
        frame(&ui, 140, 30);
        let key = "chat:agent/fleet/cos".to_owned();
        assert!(ui.panes.borrow()[&key].follow);
        ui.scroll_pane(&key, -5);
        assert!(!ui.panes.borrow()[&key].follow);
        if let Some(Load::Ready(entries)) = ui.world.conversations.get_mut("agent/fleet/cos") {
            entries.push(demo::late_mail());
        }
        let screen = frame(&ui, 140, 30).join("\n");
        assert!(screen.contains("new lines"), "{screen}");
        ui.follow_latest();
        frame(&ui, 140, 30);
        assert!(ui.panes.borrow()[&key].follow);
    }

    #[test]
    fn selection_copies_only_its_own_pane() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        frame(&ui, 140, 30);
        let pane = ui
            .frame
            .borrow()
            .panes
            .iter()
            .find(|pane| pane.key.starts_with("chat:"))
            .map(|pane| pane.rect)
            .unwrap();
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: pane.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            ui.selection.is_none(),
            "a press in the sidebar is not a text selection"
        );
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pane.x + 1,
            row: pane.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 0,
            row: pane.y + 4,
            modifiers: KeyModifiers::NONE,
        });
        let selection = ui.selection.clone().unwrap();
        assert!(selection.pane.starts_with("chat:"));
        assert_eq!(
            selection.head.1, 0,
            "dragging past the pane edge clamps to the pane"
        );
    }

    #[test]
    fn every_attention_kind_draws_its_own_card() {
        let mut ui = Ui::new(demo::world());
        let expectations = [
            ("attention/1", "Request changes"),
            ("attention/2", "YOUR FEEDBACK"),
            ("attention/3", "Approve launch"),
            ("attention/4", "SUGGESTED FIX"),
            ("attention/5", "Approve revision"),
            ("attention/6", "Reply"),
        ];
        for (id, text) in expectations {
            let index = ui
                .ids()
                .iter()
                .position(|candidate| candidate == id)
                .unwrap();
            ui.select(index);
            let screen = frame(&ui, 150, 60).join("\n");
            assert!(screen.contains(text), "{id} should show {text}:\n{screen}");
        }
    }

    #[test]
    fn frames_never_exceed_their_width() {
        let mut ui = Ui::new(demo::world());
        for width in [72u16, 100, 160] {
            for tab in 0..5 {
                ui.tab = tab;
                for line in frame(&ui, width, 40) {
                    assert!(text::width(&line) <= width as usize);
                }
            }
        }
    }
    fn press(ui: &mut Ui, code: KeyCode) {
        ui.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn g_goes_to_the_mission_behind_a_home_item() {
        let mut ui = Ui::new(demo::world());
        let index = ui.ids().iter().position(|id| id == "attention/1").unwrap();
        ui.select(index);
        press(&mut ui, KeyCode::Char('g'));
        assert_eq!(ui.tab, 2);
        assert_eq!(
            ui.selected_id().as_deref(),
            Some("mission/fleet/atlas/store-move")
        );
    }

    #[test]
    fn chat_about_this_starts_a_thread_with_the_agent_involved() {
        let mut ui = Ui::new(demo::world());
        let index = ui.ids().iter().position(|id| id == "attention/1").unwrap();
        ui.select(index);
        press(&mut ui, KeyCode::Char('t'));
        assert_eq!(ui.chat.as_ref().unwrap().to, "agent/fleet/atlas/builder");
        for character in "why now?".chars() {
            press(&mut ui, KeyCode::Char(character));
        }
        press(&mut ui, KeyCode::Enter);
        let screen = frame(&ui, 150, 70).join("\n");
        assert!(
            screen.contains("CHAT WITH ATLAS BUILDER")
                || screen.contains("chat with Atlas Builder"),
            "{screen}"
        );
        assert!(screen.contains("why now?"), "{screen}");
    }

    #[test]
    fn a_fault_with_nobody_attached_is_discussed_with_the_chief_of_staff() {
        let mut ui = Ui::new(demo::world());
        let mut world = demo::world();
        if let Load::Ready(items) = &mut world.attention {
            for item in items.iter_mut() {
                item.agent = None;
            }
        }
        ui.set_world(world);
        let item = ui.world.attention.items()[3].clone();
        assert_eq!(ui.chat_target(&item).unwrap().0, "agent/fleet/cos");
    }

    #[test]
    fn remind_me_later_hides_a_message_and_the_badge_counts_what_is_left() {
        let mut ui = Ui::new(demo::world());
        let index = ui.ids().iter().position(|id| id == "attention/6").unwrap();
        ui.select(index);
        press(&mut ui, KeyCode::Char('l'));
        assert!(!ui.ids().contains(&"attention/6".to_owned()));
        let top = frame(&ui, 150, 30)[0].clone();
        assert!(top.contains("◆5"), "{top}");
    }

    #[test]
    fn clicking_an_agent_in_a_mission_opens_a_popover_not_another_tab() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 2;
        frame(&ui, 150, 60);
        let (rect, _) = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::Peek(id) if id.starts_with("agent/")))
            .cloned()
            .unwrap();
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(ui.tab, 2);
        assert!(ui.popover.is_some());
        let screen = frame(&ui, 150, 60).join("\n");
        assert!(screen.contains("Go to agent"), "{screen}");
        press(&mut ui, KeyCode::Char('g'));
        assert_eq!(ui.tab, 1);
        assert!(ui.popover.is_none());
    }
    #[test]
    fn a_launch_without_a_preview_says_why_and_offers_no_approval() {
        let mut world = demo::world();
        if let Load::Ready(items) = &mut world.attention {
            for item in items.iter_mut() {
                if let AttentionKind::Launch { preview, .. } = &mut item.kind {
                    *preview = Load::Failed(
                        "The planner's latest candidate (draft) has no preview.".into(),
                    );
                }
            }
        }
        let mut ui = Ui::new(world);
        let index = ui.ids().iter().position(|id| id == "attention/3").unwrap();
        ui.select(index);
        let screen = frame(&ui, 150, 50).join("\n");
        assert!(screen.contains("NOTHING TO APPROVE YET"), "{screen}");
        assert!(!screen.contains("Approve launch"), "{screen}");
        assert!(!screen.contains("asks you"), "{screen}");
    }
    #[test]
    fn the_wheel_scrolls_the_sidebar_away_from_the_selection_and_stays_there() {
        let mut world = demo::world();
        if let Load::Ready(agents) = &mut world.agents {
            let template = agents[5].clone();
            for index in 0..40 {
                let mut agent = template.clone();
                agent.id = format!("agent/fleet/extra/{index}");
                agent.name = format!("Extra {index:02}");
                agents.push(agent);
            }
        }
        let mut ui = Ui::new(world);
        ui.tab = 1;
        frame(&ui, 140, 30);
        for _ in 0..60 {
            ui.mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 2,
                row: 10,
                modifiers: KeyModifiers::NONE,
            });
            frame(&ui, 140, 30);
        }
        let (lines, height) = {
            let info = ui.frame.borrow();
            (info.sidebar_lines, info.sidebar_height)
        };
        assert_eq!(
            ui.list_top.borrow()[1],
            lines - height,
            "scrolled to the bottom and stayed"
        );
        press(&mut ui, KeyCode::Down);
        frame(&ui, 140, 30);
        assert!(
            ui.list_top.borrow()[1] < lines - height,
            "a new selection comes back into view"
        );
    }
    #[test]
    fn the_message_box_grows_with_the_draft_and_keeps_new_lines() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        press(&mut ui, KeyCode::Char('c'));
        for character in
            "first line of a long message that has to wrap across the box at least once or twice"
                .chars()
        {
            press(&mut ui, KeyCode::Char(character));
        }
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        for character in "second paragraph".chars() {
            press(&mut ui, KeyCode::Char(character));
        }
        let screen = frame(&ui, 110, 40).join("\n");
        assert!(screen.contains("first line of a long message"), "{screen}");
        assert!(screen.contains("second paragraph"), "{screen}");
        assert!(ui.editing, "Alt+Enter adds a line instead of sending");
    }
    #[test]
    fn a_terminal_keeps_its_cursor_row_in_view_and_says_when_its_screen_is_not_current() {
        // Taller than the pane: the bottom shows, unless the cursor is above it.
        assert_eq!(terminal_rows_skipped(40, 10, None), 30);
        assert_eq!(terminal_rows_skipped(40, 10, Some((35, 0))), 30);
        assert_eq!(terminal_rows_skipped(40, 10, Some((20, 0))), 11);
        assert_eq!(terminal_rows_skipped(40, 10, Some((5, 0))), 0);
        assert_eq!(terminal_rows_skipped(8, 10, Some((5, 0))), 0);

        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        ui.terminal = Some(TerminalView {
            name: "Keeper".into(),
            title: "Keeper · vim".into(),
            lines: (0..40)
                .map(|row| Line::from(format!("row {row}")))
                .collect(),
            cursor: Some((5, 2)),
            stale: Some("st closed the connection".into()),
            ended: None,
        });
        let screen = frame(&ui, 120, 20).join("\n");
        assert!(screen.contains("row 5"), "{screen}");
        assert!(!screen.contains("row 39"), "{screen}");
        assert!(
            screen.contains("not current, reconnecting: st closed the connection"),
            "{screen}"
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        // Column 2 of "row 5" is its "w".
        let (row, column) = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width.saturating_sub(2)).map(move |x| (y, x)))
            .find(|&(y, x)| {
                buffer[(x, y)].symbol() == "w"
                    && buffer[(x + 1, y)].symbol() == " "
                    && buffer[(x + 2, y)].symbol() == "5"
            })
            .expect("row 5 is drawn");
        assert!(
            buffer[(column, row)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "the cursor cell at column 2 is inverted"
        );
    }

    #[test]
    fn enter_opens_an_agents_terminal_and_ctrl_backslash_returns() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        press(&mut ui, KeyCode::Enter);
        assert!(ui.terminal.is_some());
        let screen = frame(&ui, 120, 30).join("\n");
        assert!(screen.contains("Return"), "{screen}");
        ui.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(ui.effects.is_empty(), "the first Ctrl-C only arms");
        ui.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(ui.effects.last(), Some(Effect::TerminalKey(_))));
        ui.key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL));
        assert!(
            matches!(ui.effects.last(), Some(Effect::CloseTerminal)),
            "0x1c arrives as Ctrl+4"
        );
    }

    #[test]
    fn a_new_mission_needs_every_field_before_it_creates_a_launch() {
        let mut ui = Ui::new(demo::world());
        ui.live = true;
        ui.tab = 2;
        press(&mut ui, KeyCode::Char('n'));
        for character in "Audit".chars() {
            press(&mut ui, KeyCode::Char(character));
        }
        for _ in 0..3 {
            press(&mut ui, KeyCode::Enter);
        }
        press(&mut ui, KeyCode::Enter);
        assert!(ui.effects.is_empty(), "missing fields keep the form open");
        assert_eq!(
            ui.new_mission.as_ref().unwrap().1,
            1,
            "focus moves to the first empty field"
        );
        let screen = frame(&ui, 140, 50).join("\n");
        assert!(screen.contains("NEW MISSION"), "{screen}");
        for (field, value) in [
            (1, "Audit the dependencies"),
            (2, "fleet/harbor/audit"),
            (3, "~/src/harbor"),
        ] {
            ui.new_mission.as_mut().unwrap().1 = field;
            for character in value.chars() {
                press(&mut ui, KeyCode::Char(character));
            }
        }
        press(&mut ui, KeyCode::Enter);
        assert!(
            matches!(ui.effects.last(), Some(Effect::CreateLaunch { mission, .. }) if mission == "fleet/harbor/audit")
        );
    }

    #[test]
    fn a_device_is_revoked_only_after_confirming() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 3;
        let screen = frame(&ui, 140, 60).join("\n");
        assert!(screen.contains("YOUR DEVICES"), "{screen}");
        let (rect, _) = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::Revoke(_)))
            .cloned()
            .unwrap();
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(ui.world.devices.items().len(), 2, "not before confirming");
        press(&mut ui, KeyCode::Char('y'));
        assert_eq!(ui.world.devices.items().len(), 1);
    }
}
