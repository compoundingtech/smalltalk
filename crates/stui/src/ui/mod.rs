//! The new stui rendering layer: a shell that draws a `view::World`.
//!
//! The shell owns layout, the click map, scrolling, selection and keys. Screens only build
//! documents (`screens`), and the conversation renderer (`conversation`) turns entries into
//! cached lines. `stui --demo` runs it on an invented world; the live client will feed it the
//! same `World`.

pub mod adapt;
mod attach;
#[cfg(test)]
mod contract;
pub mod conversation;
pub mod demo;
pub mod doc;
mod edit;
mod glass;
pub use glass::set_glasses_version;
mod glass_store;
pub mod layout;
pub mod live;
pub mod pane;
mod prefs;
mod pty;
pub mod screens;
pub mod text;
pub mod theme;
mod usage;
mod voice;
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
use doc::{Doc, DocExt, Hit};
use pane::Pane;
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend, TestBackend},
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use screens::{Drafts, Item, ListState, Listing, TABS};
use st3_conversation_ui::pane::order;
use st3_conversation_ui::{PaneIntent, PaneState, Selection};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io::{self, Write},
    rc::Rc,
    time::{Duration, Instant},
};
use view::*;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
    /// The section the sidebar list drawn is for.
    sidebar_tab: usize,
    /// The Ctrl+S sidebar in a glass, all of it.
    glass_sidebar: Rect,
    /// Glasses: where each group's content was drawn, in group order.
    glass_leaves: Vec<Rect>,
    /// The borders between a glass's splits, as drawn, to drag.
    glass_dividers: Vec<layout::Divider>,
    /// Glasses: where Home was drawn over the glass, while it is open.
    home: Option<Rect>,
    read_messages: HashSet<String>,
    /// The focused agent's pane was too narrow for details beside its conversation.
    agent_narrow: bool,
    /// The first palette row drawn.
    palette_top: usize,
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
        /// st's message tags, such as `dictated`.
        tags: Vec<String>,
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
    /// Keep a glass in st, or delete it there.
    SaveGlass(glass::GlassWrite),
    /// Interrupt an agent's turn, as Esc does in its harness's own TUI.
    StopAgent {
        agent: String,
    },
    /// Start a new agent; its first message is what the person asked of it.
    /// Start a plain shell for the person; it opens in a new tab.
    CreateTerminal {
        name: String,
    },
    CreateAgent {
        name: String,
        harness: String,
        model: Option<String>,
        effort: Option<String>,
        host: Option<String>,
        message: Option<String>,
    },
    /// Send a failed or unconfirmed message again, as the same request.
    Resend {
        entry: String,
    },
    /// Drop a failed or unconfirmed message from the conversation.
    Forget {
        entry: String,
    },
}

/// An agent's live terminal screen, drawn in place of its conversation.
pub(crate) struct TerminalView {
    /// The agent whose terminal this is.
    pub(crate) agent: String,
    /// The agent's name; the title adds the program's own title once a screen names it.
    pub(crate) name: String,
    pub(crate) title: String,
    pub(crate) lines: Vec<Line<'static>>,
    /// The visible cursor's row and column on the screen.
    pub(crate) cursor: Option<(usize, usize)>,
    /// Why the screen shown is not current: still connecting, or reconnecting after a drop.
    pub(crate) stale: Option<String>,
    pub(crate) ended: Option<String>,
    /// The terminal attached through its PTY session, once that connects; until then, or when
    /// st cannot give a direct stream, the screens above are st's view of it.
    pub(crate) native: Option<pty::NativeTerminal>,
}

/// The entry at the top of a pane read back, how far into it, and the top it gave.
struct Anchor {
    entry: String,
    offset: usize,
    top: usize,
}

/// `/` in a conversation: what to find there, and which match is current.
struct Find {
    /// The agent whose conversation is searched.
    agent: String,
    query: String,
    /// The current match, counted from the newest: 0 is the latest one.
    current: usize,
    /// How many matches the last draw found.
    count: Cell<usize>,
    /// Scroll to the current match on the next draw.
    jump: Cell<bool>,
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
    selected: [usize; 6],
    list_top: RefCell<[usize; 6]>,
    /// The selection each sidebar last scrolled into view. The wheel may scroll away from
    /// the selection; only a new selection brings it back.
    list_follows: RefCell<[Option<usize>; 6]>,
    conversation_state: st3_conversation_ui::State,
    cache: conversation::Cache,
    editing: bool,
    confirm: Option<char>,
    flash: Option<(String, Instant)>,
    tick: u64,
    help: bool,
    sidebar: bool,
    system: bool,
    frame: RefCell<FrameInfo>,
    dragging: bool,
    demo: Option<Demo>,
    quit: bool,
    /// Live: actions become `effects` for the live loop instead of demo edits.
    live: bool,
    effects: Vec<Effect>,
    popover: Option<String>,
    chat: Option<ChatState>,
    /// Voice mode: the speech helper listening for one input.
    pub(crate) voice: Option<voice::VoiceState>,
    /// Inputs whose text came from voice; their next message is tagged `dictated`.
    dictated: HashSet<String>,
    /// The agent details pane beside the conversation.
    details: bool,
    /// The Missions tab shows the selected mission's whole declaration.
    kdl: bool,
    /// Agents and Missions list as the graph's path tree instead of grouped by state.
    tree: bool,
    /// What the Usage tab groups by, and over how many hours.
    usage_by: usage::By,
    pub(crate) usage_hours: u64,
    /// Simplified conversations here (Shift+O): this device's choice, kept in prefs.json.
    pub(crate) simple: bool,
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
    /// `stui --glasses`: named glasses of tabs and split panes, and a palette, in place of the
    /// sidebar layout.
    pub(crate) glasses: Option<glass::Glasses>,
    /// The footer names this build (version, revision, age); off in tests, whose screens must
    /// not change with every commit.
    pub(crate) build: bool,
    /// A pane too narrow for details beside the conversation shows them instead of it.
    details_here: bool,
    /// Finding text in a conversation.
    find: Option<Find>,
    /// The new agent form, kept while the person looks elsewhere.
    new_agent: Option<screens::AgentForm>,
    /// The Agents tab shows the new agent form rather than the selected agent.
    agent_form: bool,
    /// An agent just started from here, to select once st lists it.
    started: Option<String>,
    /// Images attached to each draft, by its key, until it is sent.
    attachments: HashMap<String, Vec<attach::Attachment>>,
    /// Where typing goes in the input that has the keyboard.
    cursor: edit::Cursor,
    /// Where each pane being read back was, by entry, so it keeps its place.
    anchors: RefCell<HashMap<String, Anchor>>,
    /// The rows and columns the terminal pane last had, to attach at.
    pub(crate) terminal_size: Cell<(u16, u16)>,
    /// Where the attached terminal's screen was last drawn, for its mouse.
    terminal_body: Cell<Option<Rect>>,
    /// How this terminal draws images (kitty, sixel, iTerm2, half blocks), asked once at start.
    pub(crate) picker: Option<ratatui_image::picker::Picker>,
    /// Each attachment's thumbnail, encoded once so a redraw never sends the image again.
    thumbnails: RefCell<HashMap<std::path::PathBuf, Option<ratatui_image::protocol::Protocol>>>,
    /// When st last sent each conversation something, shown above its message box.
    updated: HashMap<String, Instant>,
    /// Why a conversation could not be brought up to date, until st sends it again.
    stalled: HashMap<String, String>,
}

impl Ui {
    /// Call only after the terminal successfully presented this frame.
    pub(crate) fn visible_messages(&self) -> HashSet<String> {
        if self.help
            || self.popover.is_some()
            || self
                .glasses
                .as_ref()
                .is_some_and(|glasses| glasses.palette_open())
        {
            return HashSet::new();
        }
        self.frame.borrow().read_messages.clone()
    }
    pub fn new(world: World) -> Self {
        Self {
            world,
            tab: 0,
            selected: [0; 6],
            list_top: RefCell::new([0; 6]),
            list_follows: RefCell::new([None; 6]),
            conversation_state: st3_conversation_ui::State::default(),
            cache: conversation::Cache::default(),
            editing: false,
            confirm: None,
            flash: None,
            tick: 0,
            help: false,
            sidebar: true,
            system: false,
            frame: RefCell::new(FrameInfo::default()),
            dragging: false,
            demo: None,
            quit: false,
            live: false,
            effects: Vec::new(),
            popover: None,
            chat: None,
            voice: None,
            dictated: HashSet::new(),
            details: true,
            kdl: false,
            tree: false,
            usage_by: usage::By::default(),
            usage_hours: usage::PERIODS[0],
            simple: false,
            terminal: None,
            terminal_confirm: None,
            new_mission: None,
            revoke: None,
            snoozed: HashSet::new(),
            glasses: None,
            build: false,
            details_here: false,
            find: None,
            new_agent: None,
            agent_form: false,
            started: None,
            attachments: HashMap::new(),
            cursor: edit::Cursor::default(),
            terminal_size: Cell::new((24, 80)),
            terminal_body: Cell::new(None),
            anchors: RefCell::new(HashMap::new()),
            picker: None,
            thumbnails: RefCell::new(HashMap::new()),
            updated: HashMap::new(),
            stalled: HashMap::new(),
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
        self.select_started();
        if self.glasses.is_some() {
            self.resync_focus();
        }
    }

    /// The conversations to keep live, the focused one first: the selected agent's in the
    /// sidebar layout; in a glass, every agent pane on screen, as many as st follows at once.
    pub(crate) fn live_conversations(&self) -> Vec<String> {
        let mut targets = Vec::new();
        if self.tab == 1
            && let Some(id) = self.selected_id()
        {
            targets.push(id);
        }
        if let Some(glasses) = &self.glasses {
            for id in glasses.shown_agents() {
                if !targets.contains(&id) {
                    targets.push(id);
                }
            }
        }
        targets.truncate(crate::feed::MAX_CONVERSATIONS);
        targets
    }

    /// This device's remembered choices, read once when stui starts.
    pub(crate) fn load_prefs(&mut self) {
        if let Some(path) = prefs::path() {
            self.simple = prefs::load(&path).simple == Some(true);
        }
    }

    fn density(&self) -> st3_conversation_ui::Density {
        if self.simple {
            st3_conversation_ui::Density::Simple
        } else {
            st3_conversation_ui::Density::Full
        }
    }

    /// Every conversation simplified (a tool call to a line, a run of calls to one line) or in
    /// full: this device's choice, remembered.
    pub(crate) fn toggle_simple(&mut self) {
        self.simple = !self.simple;
        // Tests never touch the device's own choice.
        #[cfg(not(test))]
        if let Some(path) = prefs::path() {
            let _ = prefs::save(
                &path,
                &prefs::Prefs {
                    simple: Some(self.simple),
                },
            );
        }
        self.flash(if self.simple {
            "Simplified conversations · Shift+O for the full view"
        } else {
            "Full conversations · Shift+O to simplify"
        });
    }

    /// Usage grouped by the next dimension: agent, mission, step, model, account, host.
    pub(crate) fn usage_by_next(&mut self) {
        self.usage_by = self.usage_by.next();
        self.selected[4] = 0;
        if let Some(glasses) = self.glasses.as_mut() {
            glasses.sidebar.selected[4] = 0;
        }
    }

    /// Usage over the next period (a day, a week, thirty days); it is read again at once.
    pub(crate) fn usage_period_next(&mut self) {
        self.usage_hours = usage::next_period(self.usage_hours);
        self.world.usage = Load::Loading;
        self.flash(format!(
            "Usage over {}",
            usage::period_name(self.usage_hours)
        ));
    }

    /// The period to read usage over while something on screen shows it, or `None`.
    pub(crate) fn usage_wanted(&self) -> Option<u64> {
        let shown = match &self.glasses {
            Some(glasses) => glasses.shows_usage(),
            None => self.tab == 4,
        };
        shown.then_some(self.usage_hours)
    }

    /// Details beside the conversation, or in its place when the pane is too narrow for both.
    fn toggle_details(&mut self) {
        if self.frame.borrow().agent_narrow {
            self.details_here = !self.details_here;
        } else {
            self.details = !self.details;
        }
    }

    pub(crate) fn conversation_updated(&mut self, target: &str) {
        self.updated.insert(target.to_owned(), Instant::now());
        self.stalled.remove(target);
    }

    pub(crate) fn conversation_failed(&mut self, target: &str, error: &str) {
        self.stalled.insert(target.to_owned(), error.to_owned());
    }

    /// The agent whose message box has the keyboard: the selected agent while the Agents tab
    /// is the focused one.
    fn composing(&self, agent: &str) -> bool {
        self.tab == 1 && self.selected_id().as_deref() == Some(agent)
    }

    /// Make the links drawn in `area` clickable: addresses written out, and markdown links by
    /// the text they were drawn with.
    fn links(&self, buf: &Buffer, area: Rect) {
        for y in area.y..area.y + area.height {
            let cells = (area.x..area.x + area.width)
                .map(|x| (x, &buf[(x, y)]))
                .collect::<Vec<_>>();
            let row = cells
                .iter()
                .map(|(_, cell)| cell.symbol().chars().next().unwrap_or(' '))
                .collect::<Vec<_>>();
            let text = row.iter().collect::<String>();
            // Written-out addresses.
            let mut from = 0;
            while let Some(offset) = text[from..].find("http") {
                let start = from + offset;
                let tail = &text[start..];
                if !(tail.starts_with("https://") || tail.starts_with("http://")) {
                    from = start + 4;
                    continue;
                }
                let url = tail
                    .split(char::is_whitespace)
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(['.', ',', ')', ']', ';', ':', '"', '\'', '>']);
                let column = text[..start].chars().count();
                let width = url.chars().count();
                if let Some((x, _)) = cells.get(column) {
                    self.hit(
                        Rect {
                            x: *x,
                            y,
                            width: width as u16,
                            height: 1,
                        },
                        Hit::Link(url.to_owned()),
                    );
                }
                from = start + url.len().max(1);
            }
            // Markdown links: underlined runs whose text names a link.
            let mut index = 0;
            while index < cells.len() {
                if !cells[index].1.modifier.contains(Modifier::UNDERLINED) {
                    index += 1;
                    continue;
                }
                let start = index;
                while index < cells.len() && cells[index].1.modifier.contains(Modifier::UNDERLINED)
                {
                    index += 1;
                }
                let words = row[start..index].iter().collect::<String>();
                if let Some(url) = st3_conversation_ui::text::link_for(&words) {
                    self.hit(
                        Rect {
                            x: cells[start].0,
                            y,
                            width: (index - start) as u16,
                            height: 1,
                        },
                        Hit::Link(url),
                    );
                }
            }
        }
    }

    /// One attachment's thumbnail, encoded on first sight and kept.
    fn draw_thumbnail(&self, buf: &mut Buffer, area: Rect, attachment: &attach::Attachment) {
        let Some(picker) = &self.picker else { return };
        let mut thumbnails = self.thumbnails.borrow_mut();
        let thumbnail = thumbnails
            .entry(attachment.path.clone())
            .or_insert_with(|| {
                let image = image::ImageReader::open(&attachment.path)
                    .ok()?
                    .with_guessed_format()
                    .ok()?
                    .decode()
                    .ok()?;
                picker
                    .new_protocol(
                        image,
                        ratatui::layout::Size::new(area.width, area.height),
                        ratatui_image::Resize::Fit(None),
                    )
                    .ok()
            });
        match thumbnail {
            Some(protocol) => {
                use ratatui::widgets::Widget as _;
                ratatui_image::Image::new(protocol).render(area, buf);
            }
            None => {
                buf.set_stringn(area.x, area.y, "▣", 1, theme::fg(theme::LAVENDER));
            }
        }
    }

    /// ` ● live · updated 12s ago `, or paused when st is not following it.
    fn freshness(&self, agent: &str) -> Span<'static> {
        let age = self.updated.get(agent).map(|at| {
            let seconds = at.elapsed().as_secs();
            match seconds {
                0..60 => format!("{seconds}s ago"),
                60..3600 => format!("{}m ago", seconds / 60),
                _ => format!("{}h ago", seconds / 3600),
            }
        });
        let live = self.live_conversations().iter().any(|id| id == agent);
        if live && let Some(error) = self.stalled.get(agent) {
            let age = age
                .map(|age| format!(" · updated {age}"))
                .unwrap_or_default();
            return Span::styled(
                format!(" ⚠ retrying{age} · {} ", text::truncate(error, 60)),
                theme::fg(theme::YELLOW),
            );
        }
        let (text, color) = match (live, age) {
            (true, Some(age)) => (format!(" ● live · updated {age} "), theme::OVERLAY1),
            (true, None) => (" ● live · waiting for st ".to_owned(), theme::OVERLAY1),
            (false, Some(age)) => (
                format!(" ○ paused · updated {age} · focus to follow "),
                theme::YELLOW,
            ),
            (false, None) => (" ○ paused · focus to follow ".to_owned(), theme::YELLOW),
        };
        Span::styled(text, theme::fg(color))
    }

    /// Text the terminal pasted (bracketed paste): it goes where typing goes, whole, newlines
    /// included, so a paste never sends anything by itself. A pasted path to an image, such as a
    /// file dropped on the terminal, attaches that image to a message to an agent.
    pub fn paste(&mut self, text: String) {
        if self.terminal_focused()
            && let Some(native) = self.native_terminal()
        {
            native.paste(&text);
            return;
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let first = text.lines().next().unwrap_or("").to_owned();
        if let Some(find) = self.find.as_mut() {
            edit::insert(&mut find.query, &self.cursor, "find", &first);
            find.current = 0;
            find.jump.set(true);
            return;
        }
        if self.paste_into_palette(&first) {
            return;
        }
        if self.mission_form_focused()
            && let Some((fields, focus)) = self.new_mission.as_mut()
        {
            edit::insert(
                &mut fields[*focus],
                &self.cursor,
                &format!("mission:{focus}"),
                &text,
            );
            return;
        }
        if let Some(chat) = self.chat.clone().filter(|chat| chat.editing) {
            let input = format!("chat:{}", chat.item);
            let draft = self
                .conversation_state
                .drafts
                .entry(input.clone())
                .or_default();
            edit::insert(draft, &self.cursor, &input, &text);
            return;
        }
        // A paste over an agent's conversation starts a message to it.
        if !self.editing && self.tab == 1 && self.selected_id().is_some() {
            self.editing = true;
        }
        if !self.editing {
            return;
        }
        let Some(key) = self.draft_key() else { return };
        if self.tab == 1
            && let Some(attachment) = attach::from_path(&text)
        {
            self.flash(format!("Attached {}", attachment.label()));
            self.attachments.entry(key).or_default().push(attachment);
            return;
        }
        let draft = self
            .conversation_state
            .drafts
            .entry(key.clone())
            .or_default();
        edit::insert(draft, &self.cursor, &key, &text);
    }

    /// Attach the image on this machine's clipboard to the message being written.
    fn attach_clipboard(&mut self) {
        let Some(key) = self.draft_key() else { return };
        // kitty hands over the person's own clipboard through the terminal, wherever stui
        // runs; elsewhere this machine's clipboard is the person's.
        let attached = if attach::terminal_clipboard() {
            attach::from_terminal().or_else(|error| attach::from_clipboard().map_err(|_| error))
        } else {
            attach::from_clipboard()
        };
        match attached {
            Ok(attachment) => {
                self.flash(format!("Attached {}", attachment.label()));
                self.attachments.entry(key).or_default().push(attachment);
            }
            Err(error) => self.flash(format!("Nothing attached: {error}")),
        }
    }

    /// The selected agent's newest message that failed or went unconfirmed, by entry id.
    fn undelivered(&self) -> Option<String> {
        let agent = self.selected_id()?;
        let Some(Load::Ready(entries)) = self.world.conversations.get(&agent) else {
            return None;
        };
        entries
            .iter()
            .rev()
            .find(|entry| {
                matches!(
                    &entry.body,
                    Body::Pending {
                        failed: Some(_),
                        ..
                    }
                )
            })
            .map(|entry| entry.id.clone())
    }

    /// The tab and the id selected in it.
    pub fn focus(&self) -> (usize, Option<String>) {
        (self.tab, self.selected_id())
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    fn listing(&self, width: usize) -> Listing {
        self.listing_for(self.tab, width)
    }

    fn listing_for(&self, tab: usize, width: usize) -> Listing {
        match tab {
            0 => screens::home_list(&self.world, &self.snoozed),
            1 if self.tree => screens::agents_tree(&self.world, self.spinner(), width),
            1 => screens::agents_list(&self.world, self.spinner(), width),
            2 if self.tree => screens::missions_tree(&self.world, self.spinner(), self.system),
            2 => screens::missions_list(&self.world, self.spinner(), self.system),
            3 => screens::fleet_list(&self.world),
            4 => usage::list(&self.world, self.usage_by, self.usage_hours),
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
        if self.glasses.is_some() {
            self.render_glass(buf, area);
        } else {
            self.top_bar(buf, Rect { height: 1, ..area });
            self.draw_body(
                buf,
                Rect {
                    y: area.y + 1,
                    height: area.height - 2,
                    ..area
                },
            );
            self.footer(
                buf,
                Rect {
                    y: area.y + area.height - 1,
                    height: 1,
                    ..area
                },
            );
        }
        if let Some(subject) = &self.popover {
            self.draw_popover(buf, area, subject);
        }
        if self.help {
            self.draw_help(buf, area);
        }
    }

    /// The sidebar and the main area under the top bar.
    fn draw_body(&self, buf: &mut Buffer, body: Rect) {
        let area = body;
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
            self.draw_pane(buf, sidebar, &Pane::List(self.tab));
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
    }

    /// One pane alone, filling the frame: how a glass will draw each of its panes.
    fn render_pane(&self, frame: &mut ratatui::Frame<'_>, pane: &Pane) {
        let area = frame.area();
        let buf = frame.buffer_mut();
        *self.frame.borrow_mut() = FrameInfo::default();
        buf.set_style(area, Style::default().bg(theme::BASE).fg(theme::TEXT));
        self.draw_pane(buf, area, pane);
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
            if index == 5 {
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
        if let Link::Offline(message) = &self.world.link {
            buf.set_stringn(
                area.x + 1,
                area.y,
                message,
                area.width.saturating_sub(2) as usize,
                Style::default().fg(theme::YELLOW).bg(theme::CRUST),
            );
            return;
        }
        let sidebar = self
            .glasses
            .as_ref()
            .is_some_and(|glasses| glasses.sidebar.shown && glasses.sidebar.focused);
        let hints: Vec<(&str, &str)> = if sidebar && !self.editing {
            vec![
                ("↑↓", "select"),
                ("←→", "section"),
                ("enter", "open in a tab"),
                ("esc", "hide"),
                ("ctrl+k", "find"),
            ]
        } else if self.terminal_focused() {
            vec![
                ("ctrl+\\", "return"),
                ("keys", "go to the terminal"),
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
            vec![
                ("enter", "send"),
                ("esc", "stop editing"),
                ("ctrl+w", "delete word"),
                ("ctrl+u", "clear"),
            ]
        } else if self.confirm.is_some() {
            vec![("y", "confirm"), ("esc", "cancel")]
        } else {
            let mut hints = match &self.glasses {
                Some(_) if self.split_shown() => vec![
                    ("ctrl+k", "open"),
                    ("[ ]", "tabs"),
                    ("alt+←→↑↓", "splits"),
                    ("ctrl+w", "close"),
                ],
                Some(_) if !self.on_home() => {
                    vec![("ctrl+k", "open"), ("[ ]", "tabs"), ("ctrl+w", "close")]
                }
                Some(_) => vec![("ctrl+k", "open"), ("↑↓", "select")],
                None => vec![("1-5", "tabs"), ("↑↓", "select")],
            };
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
        // The build, always at the right edge; the hints give way to it.
        let build = self
            .build
            .then(|| format!(" {} ", crate::version::short(crate::version::now())));
        let reserved = build
            .as_deref()
            .map_or(0, |build| text::width(build) as u16 + 1);
        let mut x = area.x + 1;
        for (key, label) in hints {
            let key_text = format!("{key} ");
            let label_text = format!("{label}   ");
            if x + (text::width(&key_text) + text::width(&label_text)) as u16 + reserved
                > area.x + area.width
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
        if let Some(build) = &build
            && x + reserved <= area.x + area.width
        {
            buf.set_stringn(
                area.x + area.width - reserved,
                area.y,
                build,
                reserved as usize,
                Style::default().fg(theme::OVERLAY0).bg(theme::CRUST),
            );
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

    /// A tab's list, as the sidebar draws it.
    fn draw_list(&self, buf: &mut Buffer, area: Rect, tab: usize) {
        self.draw_list_as(buf, area, tab, self.selected[tab], Hit::Row);
    }

    /// A tab's list with `selected` marked, each row a `hit` to click.
    pub(crate) fn draw_list_as(
        &self,
        buf: &mut Buffer,
        area: Rect,
        tab: usize,
        selected: usize,
        hit: fn(usize) -> Hit,
    ) {
        buf.set_style(area, Style::default().bg(theme::MANTLE));
        // One column for the frame edge and one kept free for the scrollbar.
        let width = area.width.saturating_sub(2) as usize;
        let listing = self.listing_for(tab, width);
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
            info.sidebar_tab = tab;
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
        let selected = selected.min(listing.ids.len().saturating_sub(1));
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
                    children,
                } => {
                    let is_selected = *index == selected;
                    if is_selected {
                        let height = 1 + usize::from(!second.is_empty()) + children.len();
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
                    // Beneath the row and part of it: they select and click as the row.
                    for child in children {
                        rows.push((
                            Some(*index),
                            Line::from(truncate_spans(&child.spans, width)),
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
        let mut top = self.list_top.borrow()[tab];
        let followed = self.list_follows.borrow()[tab];
        if followed != Some(selected) {
            if selected_range.1 > top + height {
                top = selected_range.1 - height;
            }
            if selected_range.0 < top {
                top = selected_range.0.saturating_sub(1);
            }
            self.list_follows.borrow_mut()[tab] = Some(selected);
        }
        top = top.min(rows.len().saturating_sub(height));
        self.list_top.borrow_mut()[tab] = top;
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
                self.hit(row, hit(*index));
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
            2 => self.mission_decision(&self.selected_id()?),
            _ => None,
        }
    }

    /// The open decision a mission is waiting on, when Home still has it.
    fn mission_decision(&self, id: &str) -> Option<String> {
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
                        .render(
                            &about,
                            width.saturating_sub(4),
                            &self.conversation_state.expanded,
                            self.spinner(),
                            self.density(),
                        )
                        .lines
                }
                _ => Vec::new(),
            };
            let text = self.conversation_state.drafts.get(&chat_key).map(String::as_str).unwrap_or("");
            screens::Chat {
                to: chat.to_name.clone(),
                text,
                cursor: self.cursor.at(&chat_key, text),
                editing: chat.editing,
                thread,
            }
        });
        let text = self.conversation_state.drafts.get(key).map(String::as_str);
        Drafts {
            text,
            cursor: self.cursor.at(key, text.unwrap_or("")),
            editing: self.editing,
            confirm: self.confirm,
            chat,
        }
    }

    /// What the main area shows for the current tab and selection.
    fn main_pane(&self) -> Pane {
        let id = self.selected_id();
        match self.tab {
            0 => Pane::Home(id),
            1 if self.agent_form => Pane::NewAgent,
            1 => match &self.terminal {
                Some(view) => Pane::Terminal(view.agent.clone()),
                None => Pane::Agent(id),
            },
            2 if self.new_mission.is_some() => Pane::NewMission,
            2 if self.kdl => Pane::Declaration(id),
            2 => Pane::Mission(id),
            3 => Pane::Machine(id.map(|name| format!("machine/{name}"))),
            4 => Pane::Usage(id),
            _ => Pane::Worktree(id),
        }
    }

    fn draw_main(&self, buf: &mut Buffer, area: Rect) {
        self.draw_pane(buf, area, &self.main_pane());
    }

    /// Draw any pane into any rectangle: every screen stui has goes through here. A pane that
    /// scrolls as one document keeps its scroll under the pane's own key.
    fn draw_pane(&self, buf: &mut Buffer, area: Rect, pane: &Pane) {
        let width = area.width.saturating_sub(1) as usize;
        let doc = match pane {
            Pane::List(tab) => return self.draw_list(buf, area, *tab),
            Pane::Agent(id) => return self.draw_agent(buf, area, id.as_deref()),
            Pane::Terminal(agent) => return self.draw_terminal(buf, area, agent),
            Pane::Home(id) => {
                let drafts = self.drafts_for(id.as_deref().unwrap_or_default(), width);
                screens::home_detail(&self.world, id.as_deref(), width, &drafts)
            }
            Pane::NewMission => {
                let empty = Default::default();
                let (fields, focus) = self.new_mission.as_ref().unwrap_or(&empty);
                screens::new_mission_form(
                    fields,
                    *focus,
                    self.cursor.at(&format!("mission:{focus}"), &fields[*focus]),
                    width,
                )
            }
            Pane::NewAgent => {
                let empty = Default::default();
                let form = self.new_agent.as_ref().unwrap_or(&empty);
                screens::new_agent_form(
                    form,
                    &self.other_hosts(),
                    [
                        self.cursor.at("agent:task", &form.task),
                        self.cursor.at("agent:name", &form.name),
                    ],
                    width,
                )
            }
            Pane::Declaration(id) => screens::mission_kdl(&self.world, id.as_deref(), width),
            Pane::Mission(id) => {
                let decision =
                    id.as_deref()
                        .and_then(|id| self.mission_decision(id))
                        .map(|decision| {
                            let drafts = self.drafts_for(&decision, width);
                            screens::home_detail(&self.world, Some(&decision), width, &drafts)
                        });
                screens::mission_detail(
                    &self.world,
                    id.as_deref(),
                    width,
                    self.spinner(),
                    decision,
                    &self.conversation_state.expanded,
                )
            }
            Pane::Machine(id) => {
                let name = id
                    .as_deref()
                    .map(|id| id.strip_prefix("machine/").unwrap_or(id));
                screens::fleet_detail(&self.world, name, width, self.spinner())
            }
            Pane::Worktree(id) => {
                screens::worktree_detail(&self.world, id.as_deref(), width, self.spinner())
            }
            Pane::Usage(id) => usage::detail(&self.world, id.as_deref(), self.usage_hours, width),
        };
        self.pane(buf, &pane.key(), area, doc, false);
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
        let narrow = area.width < 90;
        if self.composing(&agent.id) {
            self.frame.borrow_mut().agent_narrow = narrow;
        }
        // Too narrow for details beside the conversation: `i` shows them in its place.
        if narrow && self.details_here && !agent.unmanaged && self.composing(&agent.id) {
            buf.set_stringn(
                area.x,
                area.y,
                " i back to the conversation ",
                area.width as usize,
                theme::fg(theme::OVERLAY1),
            );
            self.hit(Rect { height: 1, ..area }, Hit::Key('i'));
            let doc = screens::agent_details(
                &self.world,
                agent,
                (area.width as usize).saturating_sub(3),
                self.spinner(),
            );
            self.pane(
                buf,
                &format!("details:{}", agent.id),
                Rect {
                    y: area.y + 1,
                    height: area.height.saturating_sub(1),
                    ..area
                },
                doc,
                false,
            );
            return;
        }
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
        if !agent.unmanaged && (!narrow || self.composing(&agent.id)) {
            let label = if narrow {
                " i details "
            } else if self.details {
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
        // While finding, the find bar takes the message box's place.
        let find = self.find.as_ref().filter(|find| find.agent == agent.id);
        let doc = match self.world.conversations.get(&agent.id) {
            None | Some(Load::Loading) => {
                let mut doc = Doc::new();
                // A first page st keeps refusing says why while stui tries again, rather than
                // loading forever.
                match self.stalled.get(&agent.id) {
                    Some(error) => doc.wrap(
                        &[
                            text::run(format!(" {} ", self.spinner()), theme::dim()),
                            text::run(
                                format!("Not loaded yet: {error}. Trying again."),
                                theme::fg(theme::YELLOW),
                            ),
                        ],
                        width.saturating_sub(2),
                    ),
                    None => doc.line(Line::from(Span::styled(
                        format!(" {} Loading the conversation…", self.spinner()),
                        theme::dim(),
                    ))),
                }
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
                // Folded tool output with a match inside opens while finding.
                let query = find
                    .map(|find| find.query.to_lowercase())
                    .unwrap_or_default();
                let mut expanded = self.conversation_state.expanded.clone();
                if !query.is_empty() {
                    expanded.extend(entries.iter().filter_map(|entry| {
                        match &entry.body {
                            Body::Tool { output, .. }
                                if output
                                    .iter()
                                    .any(|line| line.to_lowercase().contains(&query)) =>
                            {
                                Some(entry.id.clone())
                            }
                            _ => None,
                        }
                    }));
                }
                let mut doc = Doc::new();
                doc.blank();
                doc.append(
                    self.cache
                        .render(entries, width, &expanded, self.spinner(), self.density()),
                    0,
                );
                doc.blank();
                doc
            }
        };
        let key = format!("chat:{}", agent.id);
        let matches = find
            .map(|find| find_matches(&doc, &find.query))
            .unwrap_or_default();
        if let Some(find) = find {
            find.count.set(matches.len());
        }
        // The composer grows with the draft (up to eight lines) and keeps the cursor in view.
        let composer = if let Some(find) = find {
            vec![self.find_bar(find)]
        } else if agent.unmanaged {
            Vec::new()
        } else {
            self.composer_lines(agent, width)
        };
        // Attached images show as small thumbnails above the box, where the terminal draws them.
        let thumbnails = match (&self.picker, find, self.attachments.get(&agent.id)) {
            (Some(_), None, Some(list)) if !list.is_empty() => list.clone(),
            _ => Vec::new(),
        };
        let strip = if thumbnails.is_empty() {
            0
        } else {
            THUMBNAIL.height
        };
        let composer_height = if composer.is_empty() {
            0
        } else {
            composer.len() as u16 + 2 + strip
        };
        let body = Rect {
            y: area.y + header_height,
            height: area.height.saturating_sub(header_height + composer_height),
            ..area
        };
        if let Some(find) = find {
            let current = matches
                .len()
                .checked_sub(1 + find.current.min(matches.len().saturating_sub(1)))
                .and_then(|index| matches.get(index));
            if find.jump.replace(false)
                && let Some((line, _, _)) = current
            {
                let mut panes = self.conversation_state.panes.borrow_mut();
                let state = panes.entry(key.clone()).or_default();
                state.top = line.saturating_sub(body.height as usize / 3);
                state.follow = false;
            }
        }
        self.pane(buf, &key, body, doc, true);
        // Every match on screen is marked; the current one stands out.
        if let Some(find) = find {
            let top = self
                .conversation_state
                .panes
                .borrow()
                .get(&key)
                .map_or(0, |state| state.top);
            let current = matches
                .len()
                .saturating_sub(1 + find.current.min(matches.len().saturating_sub(1)));
            for (index, (line, column, width)) in matches.iter().enumerate() {
                if *line < top || *line >= top + body.height as usize {
                    continue;
                }
                let y = body.y + (line - top) as u16;
                let style = if index == current {
                    Style::default().fg(theme::CRUST).bg(theme::YELLOW)
                } else {
                    Style::default().fg(theme::TEXT).bg(theme::SURFACE2)
                };
                for x in *column..column + width {
                    if let Some(cell) = buf.cell_mut((body.x + x as u16, y))
                        && x < body.width as usize
                    {
                        cell.set_style(style);
                    }
                }
            }
        }
        if composer_height > 0 {
            let y = body.y + body.height;
            buf.set_stringn(
                area.x,
                y,
                "─".repeat(area.width as usize),
                area.width as usize,
                theme::fg(if self.editing && self.composing(&agent.id) {
                    theme::ACCENT
                } else {
                    theme::SURFACE0
                }),
            );
            // A working agent can be stopped from here, as with Ctrl+C.
            if self.live
                && agent.state == AgentState::Working
                && !agent.unmanaged
                && self.composing(&agent.id)
            {
                let label = " working · ctrl+c twice stops it ";
                buf.set_stringn(
                    area.x + 2,
                    y,
                    label,
                    area.width.saturating_sub(4) as usize,
                    theme::dim(),
                );
                // Not clickable: it sits where a click focuses the box (Nathan, 2026-10-01).
            }
            // How fresh the conversation is, on the rule above the box: st pushes changes to
            // the conversations on screen, and one that is not followed says so.
            if self.live {
                let label = self.freshness(&agent.id);
                let width = text::width(&label.content) as u16;
                if width + 4 < area.width {
                    buf.set_line(
                        area.x + area.width - width - 2,
                        y,
                        &Line::from(vec![label]),
                        width,
                    );
                }
            }
            for (index, attachment) in thumbnails.iter().enumerate() {
                let x = area.x + 2 + index as u16 * (THUMBNAIL.width + 1);
                if x + THUMBNAIL.width > area.x + area.width {
                    break;
                }
                self.draw_thumbnail(
                    buf,
                    Rect {
                        x,
                        y: y + 1,
                        width: THUMBNAIL.width,
                        height: THUMBNAIL.height,
                    },
                    attachment,
                );
            }
            for (offset, line) in composer.iter().enumerate() {
                buf.set_line(area.x, y + 1 + strip + offset as u16, line, area.width);
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
            // Where this stui can listen, the box offers it: a click or Ctrl+R.
            if crate::voice::available()
                && self.composing(&agent.id)
                && self.voice_for(&agent.id).is_none()
            {
                let label = " ◉ ctrl+r speak ";
                let width = text::width(label) as u16;
                // Only beside the text, never over it.
                let typed = composer.first().map_or(0, |line| line.width()) as u16;
                if area.width > width + 24 && typed + width + 2 <= area.width {
                    let at = (area.x + area.width - width - 1, y + 1 + strip);
                    buf.set_stringn(
                        at.0,
                        at.1,
                        label,
                        width as usize,
                        theme::fg(theme::OVERLAY1),
                    );
                    self.hit(
                        Rect {
                            x: at.0,
                            y: at.1,
                            width,
                            height: 1,
                        },
                        Hit::Voice,
                    );
                }
            }
        }
    }

    /// Draw a document into a scrolling pane, registering its click targets.
    fn pane(&self, buf: &mut Buffer, key: &str, area: Rect, doc: Doc, follow_default: bool) {
        let height = area.height as usize;
        let total = doc.lines.len();
        let state = {
            let mut panes = self.conversation_state.panes.borrow_mut();
            let state = panes.entry(key.to_owned()).or_insert(PaneState {
                follow: follow_default,
                seen: total,
                ..PaneState::default()
            });
            // Someone reading back keeps their place by entry: lines added, removed or grown
            // above it (a window that slid, a late entry, a tool call that grew) never move
            // what they read. Unless they scrolled since, the entry at the top stays there.
            let mut anchors = self.anchors.borrow_mut();
            if !state.follow
                && let Some(anchor) = anchors.get(key)
                && anchor.top == state.top
                && let Some((_, start)) = doc.entries.iter().find(|(id, _)| *id == anchor.entry)
            {
                state.top = start + anchor.offset;
            }
            state.reconcile(total, height);
            match doc
                .entries
                .iter()
                .rev()
                .find(|(_, start)| *start <= state.top)
            {
                Some((entry, start)) if !state.follow => {
                    anchors.insert(
                        key.to_owned(),
                        Anchor {
                            entry: entry.clone(),
                            offset: state.top - start,
                            top: state.top,
                        },
                    );
                }
                _ => {
                    anchors.remove(key);
                }
            }
            *state
        };
        let top = state.top;
        if area.width > 1 && height > 0 {
            self.frame.borrow_mut().read_messages.extend(
                doc.messages
                    .iter()
                    .filter(|(_, range)| range.start < top + height && range.end > top)
                    .map(|(id, _)| id.clone()),
            );
        }
        let lines = Rc::new(doc.lines);
        for (offset, line) in lines.iter().skip(top).take(height).enumerate() {
            buf.set_line(
                area.x,
                area.y + offset as u16,
                line,
                area.width.saturating_sub(1),
            );
        }
        self.links(
            buf,
            Rect {
                height: (total.saturating_sub(top)).min(height) as u16,
                width: area.width.saturating_sub(1),
                ..area
            },
        );
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
        if let Some(selection) = &self.conversation_state.selection
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

    fn draw_terminal(&self, buf: &mut Buffer, area: Rect, agent: &str) {
        let Some(view) = self.terminal.as_ref().filter(|view| view.agent == agent) else {
            buf.set_stringn(
                area.x,
                area.y + 1,
                " Not attached. Ctrl+] attaches the terminal; Ctrl+\\ leaves it.",
                area.width as usize,
                theme::dim(),
            );
            return;
        };
        if let Some(native) = view.native.as_ref() {
            // One line for where this is and how to leave; the PTY gets the rest of the pane.
            let scrolled = native.scrolled();
            let status = match (native.ended(), native.attached(), scrolled) {
                (Some(reason), _, _) => format!("ended: {reason}"),
                (None, false, _) => "attaching…".into(),
                (None, true, 0) => {
                    "ctrl-c twice reaches it · wheel or shift+pgup scrolls back".into()
                }
                (None, true, lines) => format!("↑ {lines} lines back · type to return"),
            };
            let header = Line::from(vec![
                Span::styled(
                    format!(" ← Ctrl+\\  {}", view.title),
                    theme::strong(theme::ACCENT),
                ),
                Span::styled(format!("   {status}"), theme::dim()),
            ]);
            buf.set_line(area.x, area.y, &header, area.width);
            self.hit(Rect { height: 1, ..area }, Hit::Detach);
            let body = Rect {
                y: area.y + 1,
                height: area.height.saturating_sub(1),
                ..area
            };
            self.terminal_size
                .set((body.height.max(1), body.width.max(1)));
            self.terminal_body.set(Some(body));
            native.fit(body.height, body.width);
            native.draw(buf, body);
            return;
        }
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

    /// The attached terminal's direct connection, when it has one.
    pub(crate) fn native_terminal(&self) -> Option<&pty::NativeTerminal> {
        self.terminal.as_ref().and_then(|view| view.native.as_ref())
    }

    /// A key for the focused terminal: its bytes straight to the PTY when attached directly,
    /// else through st.
    fn terminal_key(&mut self, key: KeyEvent) {
        match self.native_terminal() {
            Some(native) => {
                if let Some(bytes) = pty::key_bytes(key, native.mode()) {
                    native.write(bytes);
                }
            }
            None => self.effects.push(Effect::TerminalKey(key)),
        }
    }

    /// The find bar: what is sought, where the current match is, and the keys.
    fn find_bar(&self, find: &Find) -> Line<'static> {
        let count = find.count.get();
        let place = match count {
            0 if find.query.is_empty() => String::new(),
            0 => "  no matches".to_owned(),
            count => format!("  {} of {count}", find.current.min(count - 1) + 1),
        };
        let at = self.cursor.at("find", &find.query);
        let query = edit::lines(&find.query, Some(at), theme::text())
            .into_iter()
            .flatten()
            .map(|run| Span::styled(run.text, run.style));
        Line::from(
            [Span::styled("/ ", theme::strong(theme::ACCENT))]
                .into_iter()
                .chain(query)
                .chain([
                    Span::styled(place, theme::fg(theme::YELLOW)),
                    Span::styled(
                        "  · enter older · shift+enter newer · esc close",
                        theme::dim(),
                    ),
                ])
                .collect::<Vec<_>>(),
        )
    }

    /// The message box under a conversation, wrapped, newest lines last.
    fn composer_lines(&self, agent: &Agent, width: usize) -> Vec<Line<'static>> {
        let draft = self
            .conversation_state
            .drafts
            .get(&agent.id)
            .cloned()
            .unwrap_or_default();
        // Only the focused pane's box takes keys; the others show their draft, and how to reach
        // them by click.
        let editing = self.editing && self.composing(&agent.id);
        // Attached images ride above the text as chips; Backspace in an empty box takes the
        // last one back.
        let chips = self
            .attachments
            .get(&agent.id)
            .map(|list| {
                list.iter()
                    .enumerate()
                    .map(|(index, attachment)| {
                        Line::from(vec![
                            Span::styled("  ▣ ", theme::fg(theme::LAVENDER)),
                            Span::styled(
                                format!("{} {}", index + 1, attachment.label()),
                                theme::fg(theme::LAVENDER),
                            ),
                            Span::styled(
                                if index + 1 == list.len() {
                                    "  · ⌫ in an empty box removes it"
                                } else {
                                    ""
                                },
                                theme::dim(),
                            ),
                        ])
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut lines = self.composer_text(agent, width, editing, &draft);
        if !chips.is_empty() {
            lines.splice(0..0, chips);
        }
        lines
    }

    fn composer_text(
        &self,
        agent: &Agent,
        width: usize,
        editing: bool,
        draft: &str,
    ) -> Vec<Line<'static>> {
        if let Some(state) = self.voice_for(&agent.id) {
            return self.voice_lines(state, width);
        }
        if draft.is_empty() && !editing {
            let hint = if self.composing(&agent.id) {
                format!("Message {} · c or click", agent.name)
            } else {
                format!("Message {} · click", agent.name)
            };
            return vec![Line::from(vec![
                Span::styled("› ", theme::dim()),
                Span::styled(hint, theme::dim()),
            ])];
        }
        let style = if editing {
            theme::text()
        } else {
            theme::soft()
        };
        let mut lines = Vec::new();
        let at = editing.then(|| self.cursor.at(&agent.id, draft));
        for (index, runs) in edit::lines(draft, at, style).into_iter().enumerate() {
            let first = if index == 0 {
                text::run(
                    "› ",
                    if editing {
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
        // Two columns where they fit, so the whole list shows at once.
        let width = 124.min(area.width.saturating_sub(4));
        let columns = if width >= 100 { 2 } else { 1 };
        let column = (width as usize - 4 - 3 * (columns - 1)) / columns;
        let keys = |doc: &mut Doc, title: &str, entries: &[(&str, &str)]| {
            doc.section(title, None, column);
            for (key, meaning) in entries {
                doc.lines(text::wrap(
                    &[text::run(*meaning, theme::soft())],
                    column,
                    &[text::run(
                        format!(" {key:<19} "),
                        theme::strong(theme::ACCENT),
                    )],
                    &[text::run(" ".repeat(21), theme::dim())],
                    None,
                ));
            }
            doc.blank();
        };
        // The marks also show in the footer's legend, so a single column puts the keys first.
        let mut marks = Doc::new();
        marks.section("what the marks mean", None, column);
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
            marks.lines(text::wrap(
                &[text::run(meaning, theme::soft())],
                column,
                &[text::run(format!(" {glyph}  "), theme::strong(color))],
                &[text::run("    ", theme::dim())],
                None,
            ));
        }
        marks.blank();
        let mut left = Doc::new();
        keys(
            &mut left,
            "everywhere",
            &[
                ("?", "this help; any key closes it"),
                (
                    "ctrl+k",
                    "open anything: an agent, a mission, a space, an action",
                ),
                (
                    "ctrl+h",
                    "Home, and again to close it (in a text box it is backspace)",
                ),
                ("q", "quit"),
            ],
        );
        if self.glasses.is_some() {
            keys(
                &mut left,
                "spaces",
                &[
                    ("ctrl+t", "open something in a new tab"),
                    ("ctrl+v  ctrl+x", "split right; split below"),
                    ("ctrl+w", "close the tab"),
                    ("[ ]  ctrl+pgup pgdn", "previous, next tab in the split"),
                    ("alt+1-9", "a tab by its number"),
                    (
                        "alt+arrows",
                        "move between splits; left of the first is the sidebar",
                    ),
                    ("ctrl+o", "zoom the split, and back"),
                    ("ctrl+s", "show or hide the sidebar"),
                    ("ctrl+g", "another space, or a new one"),
                    ("ctrl+n", "start a new agent"),
                    ("1-5", "open the palette at a section"),
                ],
            );
        } else {
            keys(
                &mut left,
                "classic",
                &[("1-5  tab", "switch tabs"), ("s", "hide or show the list")],
            );
        }
        let mut right = Doc::new();
        keys(
            &mut right,
            "lists and cards",
            &[
                ("↑↓ j k or click", "select"),
                ("t", "Agents, Missions: the path tree or the groups"),
                ("x", "Missions: show st's own missions"),
                ("n", "Agents: a new agent; Missions: a new mission"),
                ("y", "confirm what a card asks; Enter never does"),
                (
                    "b p",
                    "Usage: group by agent, mission, step...; change the period",
                ),
            ],
        );
        keys(
            &mut right,
            "a conversation",
            &[
                ("c or click", "write: a message, feedback, a reply"),
                ("wheel pgup pgdn", "scroll the pane under the pointer"),
                ("end", "jump to the newest message and follow it"),
                ("/", "find in this conversation"),
                ("o", "expand or collapse tool output"),
                (
                    "shift+o",
                    "simplified view: tool calls fold to a line (this device)",
                ),
                ("i", "the agent's details beside it"),
                ("drag", "select text in one pane; release copies it"),
                ("ctrl+]  ctrl+\\", "attach the agent's terminal; leave it"),
                ("r  x", "resend or clear a message that was not sent"),
            ],
        );
        keys(
            &mut right,
            "in a message box",
            &[
                ("enter", "send"),
                ("shift+enter ctrl+j", "a new line"),
                ("ctrl+w  ctrl+u", "delete a word; clear the box"),
                ("ctrl+v", "attach the clipboard's image"),
                (
                    "ctrl+r",
                    "speak instead of typing (a Mac with SmallTalk.app's speech helper)",
                ),
                ("esc", "stop writing; the keys above work again"),
            ],
        );
        let mut inner = Doc::new();
        if columns == 2 {
            marks.lines.extend(left.lines);
            let left = marks;
            for row in 0..left.lines.len().max(right.lines.len()) {
                let mut spans = left
                    .lines
                    .get(row)
                    .map(|line| line.spans.clone())
                    .unwrap_or_default();
                let used: usize = spans.iter().map(|span| text::width(&span.content)).sum();
                spans.push(Span::raw(" ".repeat(column.saturating_sub(used) + 3)));
                if let Some(line) = right.lines.get(row) {
                    spans.extend(line.spans.clone());
                }
                inner.line(Line::from(spans));
            }
        } else {
            inner.lines(left.lines);
            inner.lines(right.lines);
            inner.lines(marks.lines);
        }
        let mut doc = Doc::new();
        doc.card(
            "help · any key closes",
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
        self.conversation_state.selection = None;
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab.min(TABS.len() - 1);
        self.editing = false;
        self.chat = None;
        self.popover = None;
        self.kdl = false;
        self.confirm = None;
        self.conversation_state.selection = None;
    }

    fn open(&mut self, id: &str) {
        // Inside a glass, a subject opens as its own tab rather than moving a sidebar.
        if self.glasses.is_some()
            && let Some(pane) = glass::pane_for(id)
        {
            self.open_in_glass(pane, glass::Open::Tab);
            return;
        }
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
        let mut panes = self.conversation_state.panes.borrow_mut();
        let state = panes.entry(key.to_owned()).or_default();
        state.top = pane.top;
        state.scroll(
            delta,
            pane.total,
            pane.rect.height as usize,
            key.starts_with("chat:"),
        );
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
            let mut panes = self.conversation_state.panes.borrow_mut();
            let state = panes.entry(key).or_default();
            state.follow_latest();
        }
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Listening takes every key until the words are sent, kept or dropped.
        if self.voice_key(key) {
            return;
        }
        if self.glass_key(key) {
            return;
        }
        // A focused, attached terminal gets every key first, Ctrl-C included.
        if self.terminal_focused() {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // Terminals send Ctrl+\\ as 0x1c, which crossterm reports as Ctrl+4.
                KeyCode::Char('\\' | '4') if control => {
                    // In glasses an agent's tab turns back into its conversation; a shell's tab
                    // stays a shell, detached.
                    if let Some(Pane::Terminal(agent)) = self.focused_pane()
                        && agent.starts_with("agent/")
                    {
                        self.swap_focused_pane(Pane::Agent(Some(agent)));
                        self.terminal = None;
                    }
                    self.effects.push(Effect::CloseTerminal);
                }
                KeyCode::Char(letter @ ('c' | 'd')) if control => {
                    let code = KeyCode::Char(letter);
                    if self.terminal_confirm.is_some_and(|(pending, at)| {
                        pending == code && at.elapsed() < Duration::from_secs(2)
                    }) {
                        self.terminal_confirm = None;
                        self.terminal_key(key);
                    } else {
                        self.terminal_confirm = Some((code, Instant::now()));
                        self.flash(format!(
                            "Press Ctrl-{} again within 2s to send it to the agent",
                            letter.to_ascii_uppercase()
                        ));
                    }
                }
                // Shift+PgUp/PgDn page through what scrolled off the top.
                KeyCode::PageUp | KeyCode::PageDown
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        && self.native_terminal().is_some() =>
                {
                    if let Some(native) = self.native_terminal() {
                        native.page(key.code == KeyCode::PageUp);
                    }
                }
                _ => self.terminal_key(key),
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            if self.editing {
                self.editing = false;
            } else if self.confirm == Some('s') {
                // The second Ctrl+C stops the agent, as a harness's own TUI would.
                self.confirm = None;
                self.act('s');
            } else if let Some(name) = self.working_agent() {
                self.confirm = Some('s');
                self.flash(format!(
                    "Stop {name}? Ctrl+C again or y stops it · Esc keeps it working"
                ));
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
        if let Some(find) = self.find.as_mut() {
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            let count = find.count.get().max(1);
            match key.code {
                KeyCode::Esc => self.find = None,
                // Enter goes back in time, to the next older match; Shift+Enter forward.
                KeyCode::Enter | KeyCode::Up if !shift => {
                    find.current = (find.current + 1) % count;
                    find.jump.set(true);
                }
                KeyCode::Enter | KeyCode::Down => {
                    find.current = (find.current + count - 1) % count;
                    find.jump.set(true);
                }
                _ => {
                    if edit::edit(&mut find.query, &self.cursor, "find", key) {
                        find.current = 0;
                        find.jump.set(true);
                    }
                }
            }
            return;
        }
        if self.agent_form && self.tab == 1 && self.new_agent.is_some() {
            self.agent_form_key(key);
            return;
        }
        if self.mission_form_focused()
            && let Some((fields, focus)) = self.new_mission.as_mut()
        {
            let focus_now = *focus;
            match key.code {
                KeyCode::Esc => {
                    self.new_mission = None;
                    self.close_form_tab();
                }
                KeyCode::Tab => *focus = (focus_now + 1) % 4,
                KeyCode::BackTab => *focus = (focus_now + 3) % 4,
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                {
                    edit::insert(
                        &mut fields[focus_now],
                        &self.cursor,
                        &format!("mission:{focus_now}"),
                        "\n",
                    )
                }
                KeyCode::Enter if focus_now < 3 => *focus = focus_now + 1,
                KeyCode::Enter => self.create_launch(),
                _ => {
                    edit::edit(
                        &mut fields[focus_now],
                        &self.cursor,
                        &format!("mission:{focus_now}"),
                        key,
                    );
                }
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
                _ => {
                    let draft = self
                        .conversation_state
                        .drafts
                        .entry(key_id.clone())
                        .or_default();
                    edit::edit(draft, &self.cursor, &key_id, key);
                }
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
                let draft = self
                    .conversation_state
                    .drafts
                    .entry(key_id.clone())
                    .or_default();
                edit::insert(draft, &self.cursor, &key_id, "\n");
                return;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            let empty = self
                .conversation_state
                .drafts
                .get(&key_id)
                .is_none_or(String::is_empty);
            match key.code {
                KeyCode::Esc => self.editing = false,
                KeyCode::Enter => self.submit(),
                // Ctrl+R speaks into the input instead of typing.
                KeyCode::Char('r') if control => self.start_voice(),
                // Ctrl+V while writing to an agent attaches the clipboard's image.
                KeyCode::Char('v') if control && self.tab == 1 => self.attach_clipboard(),
                // Backspace in an empty box takes back the last image.
                KeyCode::Backspace
                    if empty
                        && self
                            .attachments
                            .get(&key_id)
                            .is_some_and(|list| !list.is_empty()) =>
                {
                    if let Some(list) = self.attachments.get_mut(&key_id) {
                        list.pop();
                    }
                }
                _ => {
                    let draft = self
                        .conversation_state
                        .drafts
                        .entry(key_id.clone())
                        .or_default();
                    edit::edit(draft, &self.cursor, &key_id, key);
                }
            }
            return;
        }
        if let Some(action) = self.confirm {
            match key.code {
                // Only y confirms. Enter is how a message is sent, so it never confirms anything
                // that acts for the person: stopping, approving, closing, cancelling, revoking.
                KeyCode::Char('y') => {
                    self.confirm = None;
                    self.act(action);
                }
                _ => self.confirm = None,
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            // Ctrl+H is Home; in a text box it stays backspace.
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.switch_tab(0)
            }
            KeyCode::Char('?') => self.help = true,
            // Without Ctrl: Ctrl+5 is how terminals send Ctrl+], which attaches.
            KeyCode::Char(digit @ '1'..='5') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.switch_tab(digit as usize - '1' as usize)
            }
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
            // Classic's list; in spaces only Ctrl+S has a sidebar.
            KeyCode::Char('s') if self.glasses.is_none() => self.sidebar = !self.sidebar,
            KeyCode::Char('x') if self.tab == 2 => self.system = !self.system,
            KeyCode::Char('o') if self.tab == 1 => self.toggle_all_tools(),
            KeyCode::Char('O') => self.toggle_simple(),
            KeyCode::Char('/') if self.tab == 1 => {
                if let Some(agent) = self.selected_id() {
                    self.find = Some(Find {
                        agent,
                        query: String::new(),
                        current: 0,
                        count: Cell::new(0),
                        jump: Cell::new(false),
                    });
                }
            }
            KeyCode::Char(letter @ ('r' | 'x')) if self.tab == 1 && self.live => {
                match self.undelivered() {
                    Some(entry) if letter == 'r' => self.effects.push(Effect::Resend { entry }),
                    Some(entry) => self.effects.push(Effect::Forget { entry }),
                    None => {}
                }
            }
            KeyCode::Char('i') if self.tab == 1 => self.toggle_details(),
            KeyCode::Char('b') if self.tab == 4 => self.usage_by_next(),
            KeyCode::Char('p') if self.tab == 4 => self.usage_period_next(),
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
            // Ctrl+] attaches the agent's terminal, beside Ctrl+\ that leaves it; terminals send
            // it as 0x1d, which crossterm reports as Ctrl+5. Enter was too easy to hit by mistake.
            KeyCode::Char(']' | '5')
                if self.tab == 1 && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.open_terminal()
            }
            KeyCode::Char('n') if self.tab == 1 => self.open_new_agent(None),
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
                    self.conversation_state.selection = None;
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
            .filter(|entry| st3_conversation_ui::conversation::folds(&entry.body))
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        if tools
            .iter()
            .all(|tool| self.conversation_state.expanded.contains(tool))
        {
            for tool in tools {
                self.conversation_state.expanded.remove(&tool);
            }
        } else {
            self.conversation_state.expanded.extend(tools);
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
                    (
                        "review" | "feedback" | "launch" | "message" | "revision" | "request",
                        'c',
                    ) => self.editing = true,
                    ("review" | "feedback" | "launch" | "revision", 'a') => {
                        self.confirm = Some('a')
                    }
                    ("launch", 'd') | ("revision", 'j') | ("fault" | "request", 'r') => {
                        self.confirm = Some(key)
                    }
                    ("request", 'y' | 'n') => self.confirm = Some(key),
                    ("message", 'm') => self.act('m'),
                    _ => {}
                }
            }
            1 if key == 'i' => self.toggle_details(),
            1 if key == 'S' => {
                if let Some(name) = self.working_agent() {
                    self.confirm = Some('s');
                    self.flash(format!(
                        "Stop {name}? Ctrl+C again or y stops it · Esc keeps it working"
                    ));
                }
            }
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
        // A plain shell's tab attaches itself; it has no agent to look up.
        if let Some(Pane::Terminal(id)) = self.focused_pane()
            && id.starts_with("terminal/")
        {
            self.attach_terminal(&id);
            return;
        }
        // In glasses it is the focused pane's own agent, never the list's selection, which can
        // be another agent's.
        let subject = if self.glasses.is_some() {
            match self.focused_pane() {
                Some(Pane::Agent(Some(id)) | Pane::Terminal(id)) => Some(id),
                _ => None,
            }
        } else {
            self.selected_id()
        };
        let Some(agent) = subject.and_then(|id| {
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
        // In glasses the agent's tab turns into its terminal, and Ctrl+\ turns it back.
        if self.glasses.is_some() {
            match self.focused_pane() {
                Some(Pane::Terminal(id)) if id == agent.id => {}
                Some(Pane::Agent(Some(id))) if id == agent.id => {
                    self.swap_focused_pane(Pane::Terminal(agent.id.clone()))
                }
                _ => self.open_in_glass(Pane::Terminal(agent.id.clone()), glass::Open::Tab),
            }
            self.attach_terminal(&agent.id);
            return;
        }
        self.attach_terminal(&agent.id);
    }

    /// Attach `agent`'s terminal: followed live, or the demo's in demo mode.
    pub(crate) fn attach_terminal(&mut self, agent: &str) {
        // A plain shell is a terminal of its own, not an agent's.
        if agent.starts_with("terminal/") {
            if self.live {
                self.effects.push(Effect::OpenTerminal {
                    agent: agent.to_owned(),
                });
                self.flash("Opening the terminal…");
            } else {
                self.terminal = Some(TerminalView {
                    agent: agent.to_owned(),
                    title: "shell · demo terminal".into(),
                    name: "shell".into(),
                    lines: demo::terminal("shell"),
                    cursor: None,
                    stale: None,
                    ended: None,
                    native: None,
                });
            }
            return;
        }
        let Some(agent) = self
            .world
            .agents
            .items()
            .iter()
            .find(|candidate| candidate.id == agent)
            .cloned()
        else {
            return;
        };
        if self.live {
            self.effects.push(Effect::OpenTerminal { agent: agent.id });
            self.flash("Opening the terminal…");
        } else {
            self.terminal = Some(TerminalView {
                agent: agent.id.clone(),
                title: format!("{} · demo terminal", agent.name),
                name: agent.name.clone(),
                lines: demo::terminal(&agent.name),
                cursor: None,
                stale: None,
                ended: None,
                native: None,
            });
        }
    }

    /// The fleet's machines other than this one, in list order: the new agent form's hosts.
    fn other_hosts(&self) -> Vec<String> {
        self.world
            .machines
            .items()
            .iter()
            .filter(|machine| machine.reach != Reach::Here)
            .map(|machine| machine.name.clone())
            .collect()
    }

    /// Open the new agent form: in a new tab of the focused split in a glass, on the Agents
    /// tab otherwise. `task` fills in what it should do.
    pub(crate) fn open_new_agent(&mut self, task: Option<String>) {
        match self.new_agent.as_mut() {
            Some(form) => {
                if let Some(task) = task {
                    form.task = task;
                }
            }
            None => self.new_agent = Some(screens::AgentForm::new(task.unwrap_or_default())),
        }
        if self.glasses.is_some() {
            self.open_in_glass(Pane::NewAgent, glass::Open::Tab);
        } else {
            self.tab = 1;
            self.agent_form = true;
            self.terminal = None;
        }
    }

    fn cancel_agent_form(&mut self) {
        self.new_agent = None;
        self.agent_form = false;
        if self.glasses.is_some() {
            self.close_form_tab();
        }
    }

    fn agent_form_key(&mut self, key: KeyEvent) {
        let hosts = self.other_hosts().len();
        let Some(form) = self.new_agent.as_mut() else {
            return;
        };
        let shifted = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => self.cancel_agent_form(),
            KeyCode::Tab | KeyCode::Down if form.focus >= 2 || key.code == KeyCode::Tab => {
                form.focus = (form.focus + 1) % screens::AgentForm::FIELDS;
            }
            KeyCode::BackTab | KeyCode::Up if form.focus >= 2 || key.code == KeyCode::BackTab => {
                form.focus =
                    (form.focus + screens::AgentForm::FIELDS - 1) % screens::AgentForm::FIELDS;
            }
            KeyCode::Left | KeyCode::Right if form.focus >= 2 => {
                form.cycle(key.code == KeyCode::Right, hosts);
            }
            KeyCode::Enter if shifted && form.focus == 0 => {
                edit::insert(&mut form.task, &self.cursor, "agent:task", "\n")
            }
            KeyCode::Enter => self.start_agent(),
            _ => match form.focus {
                0 => {
                    edit::edit(&mut form.task, &self.cursor, "agent:task", key);
                }
                1 => {
                    // A name is one word of letters, digits, dots and dashes.
                    if let KeyCode::Char(character) = key.code
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !(character.is_ascii_alphanumeric() || matches!(character, '-' | '.'))
                    {
                        return;
                    }
                    edit::edit(&mut form.name, &self.cursor, "agent:name", key);
                }
                _ => {}
            },
        }
    }

    fn start_agent(&mut self) {
        let hosts = self.other_hosts();
        let Some(form) = self.new_agent.clone() else {
            return;
        };
        if form.name.trim().is_empty() {
            if let Some(form) = self.new_agent.as_mut() {
                form.focus = 1;
            }
            self.flash("Give it a name");
            return;
        }
        let message = Some(form.task.trim().to_owned()).filter(|task| !task.is_empty());
        let host = form
            .host
            .checked_sub(1)
            .and_then(|index| hosts.get(index).cloned());
        if self.live {
            self.effects.push(Effect::CreateAgent {
                name: form.name.trim().to_owned(),
                harness: form.harness().to_owned(),
                model: form.model().map(str::to_owned),
                effort: form.effort().map(str::to_owned),
                host,
                message,
            });
            self.flash(format!("Starting {}…", form.name.trim()));
        } else {
            self.flash("Agent started · demo: nothing was sent");
        }
    }

    /// st started the agent asked for here: its conversation replaces the form.
    /// The new mission form: in a new tab of the focused split in a glass, on Missions otherwise.
    pub(crate) fn open_new_mission(&mut self) {
        if self.new_mission.is_none() {
            self.new_mission = Some((Default::default(), 0));
        }
        if self.glasses.is_some() {
            self.open_in_glass(Pane::NewMission, glass::Open::Tab);
        } else {
            self.switch_tab(2);
        }
    }

    /// Start a plain shell for the person, opened in a new tab once st has it.
    pub(crate) fn open_new_terminal(&mut self) {
        if self.live {
            self.effects.push(Effect::CreateTerminal {
                name: screens::random_name(),
            });
            self.flash("Starting a shell…");
        } else {
            self.terminal_started("terminal/pty/person/demo/shell".into());
        }
    }

    /// st started a shell asked for here: it opens in a new tab, attached.
    pub(crate) fn terminal_started(&mut self, id: String) {
        if self.glasses.is_some() {
            self.open_in_glass(Pane::Terminal(id.clone()), glass::Open::Tab);
        }
        self.attach_terminal(&id);
    }

    pub(crate) fn agent_started(&mut self, id: String) {
        self.new_agent = None;
        self.agent_form = false;
        if self.glasses.is_some() {
            // The agent's conversation takes the form's tab.
            self.close_form_tab();
            self.open_in_glass(Pane::Agent(Some(id.clone())), glass::Open::Tab);
        } else {
            self.tab = 1;
        }
        self.started = Some(id);
        self.select_started();
    }

    /// Select the agent just started once st lists it.
    fn select_started(&mut self) {
        let Some(id) = self.started.clone() else {
            return;
        };
        let tab = self.tab;
        self.tab = 1;
        if let Some(position) = self.ids().iter().position(|candidate| *candidate == id) {
            self.selected[1] = position;
            self.started = None;
        }
        self.tab = tab;
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
        self.close_form_tab();
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
        let text = self
            .conversation_state
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_default();
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
        self.conversation_state.drafts.remove(&key);
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
                    delivered: false,
                    dictated: false,
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
                    delivered: false,
                    dictated: false,
                },
            });
        }
        self.flash("Sent · demo: nothing left this machine");
    }

    fn submit(&mut self) {
        let Some(id) = self.draft_key() else { return };
        let images = if self.tab == 1 {
            self.attachments.get(&id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let draft = match self.conversation_state.send(&id) {
            Some(PaneIntent::Send(draft)) => Some(draft),
            _ if !images.is_empty() => Some(String::new()),
            _ => None,
        };
        let Some(mut draft) = draft else {
            self.flash("Write something first");
            return;
        };
        // Until st carries images, the message names each file; an agent on this machine
        // reads it there.
        if !images.is_empty() {
            if !draft.is_empty() {
                draft.push_str("\n\n");
            }
            draft.push_str(&attach::mention(&images));
            self.attachments.remove(&id);
        }
        // The input stays focused after a send; Esc leaves it.
        if self.live {
            let effect = match self.tab {
                1 => Some(Effect::Send {
                    agent: id.clone(),
                    tags: if self.dictated.remove(&id) {
                        vec!["dictated".into()]
                    } else {
                        Vec::new()
                    },
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
                    Some(AttentionKind::Request { .. }) => Some(Effect::Attention {
                        id: id.clone(),
                        action: "work.done".into(),
                        reason: Some(draft),
                    }),
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
                    self.conversation_state.drafts.remove(&id);
                    self.follow_latest();
                    // An answer on Home is done: the next item opens closed, so it does not
                    // take the keys (Nathan, 2026-10-02). A conversation keeps its input.
                    if self.tab != 1 {
                        self.editing = false;
                    }
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
                            delivered: false,
                            dictated: false,
                        },
                    });
                }
                self.conversation_state.drafts.remove(&id);
                self.follow_latest();
                self.flash("Sent · demo: nothing left this machine");
            }
            _ => {
                self.conversation_state.drafts.remove(&id);
                self.resolve(&id, "Sent to the agent");
            }
        }
    }

    /// The agent whose conversation is shown, by name, while it is working.
    fn working_agent(&self) -> Option<String> {
        if self.tab != 1 || self.agent_form || self.terminal_focused() {
            return None;
        }
        let id = self.selected_id()?;
        self.world
            .agents
            .items()
            .iter()
            .find(|agent| agent.id == id && agent.state == AgentState::Working && !agent.unmanaged)
            .map(|agent| agent.name.clone())
    }

    fn act(&mut self, action: char) {
        if action == 's' {
            // Stop the agent's turn: its harness gets Esc, as in its own TUI.
            let Some(agent) = self.selected_id().filter(|_| self.tab == 1) else {
                return;
            };
            if self.live {
                self.effects.push(Effect::StopAgent { agent });
                self.flash("Stopping…");
            } else {
                self.flash("Stopped · demo: nothing was sent");
            }
            return;
        }
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
        if matches!(action, 'y' | 'n' | 'r') && self.current_kind() == Some("request") {
            // r closes a request that needs nothing from the person; the step continues.
            let answer = match action {
                'y' => "Yes",
                'n' => "No",
                _ => "Nothing for me to do here.",
            };
            if self.live {
                self.effects.push(Effect::Attention {
                    id,
                    action: "work.done".into(),
                    reason: Some(answer.into()),
                });
                self.flash(format!("Answering “{answer}”…"));
            } else {
                self.flash(format!("Answered “{answer}” · demo: nothing was sent"));
            }
            return;
        }
        if self.live {
            let kind = self.current_kind().unwrap_or("");
            let name = match (kind, action) {
                ("review", 'a') => "review.approve",
                ("launch", 'a') => "launch.approve",
                ("launch", 'd') => "launch.cancel",
                ("revision", 'a') => "mission.approve-revision",
                ("revision", 'j') => "mission.cancel-revision",

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
                // Notes written on a review go with an approval too.
                let reason = (name == "review.approve")
                    .then(|| self.conversation_state.drafts.remove(&id))
                    .flatten()
                    .filter(|notes| !notes.trim().is_empty());
                self.effects.push(Effect::Attention {
                    id,
                    action: name.into(),
                    reason,
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
        // A program in the focused terminal that asked for the mouse gets its clicks there.
        if self.terminal_focused()
            && let Some(native) = self.native_terminal()
            && let Some(body) = self.terminal_body.get()
            && contains(body, mouse.column, mouse.row)
            && let Some(bytes) = pty::mouse_bytes(
                mouse,
                mouse.column - body.x,
                mouse.row - body.y,
                native.mode(),
            )
        {
            native.write(bytes);
            return;
        }
        // A tab dragged to another place or a split's edge.
        if self.drag_mouse(mouse) {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.glass_click(mouse.column, mouse.row) {
                    return;
                }
                let hit = {
                    let info = self.frame.borrow();
                    info.hits
                        .iter()
                        .rev()
                        .find(|(rect, _)| contains(*rect, mouse.column, mouse.row))
                        .map(|(_, hit)| hit.clone())
                };
                self.conversation_state.selection = None;
                if let Some(hit) = hit {
                    self.click(hit);
                    return;
                }
                if let Some((key, line, column)) = self.pane_point(mouse.column, mouse.row) {
                    self.conversation_state.selection = Some(Selection {
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
                    self.conversation_state
                        .selection
                        .as_ref()
                        .and_then(|selection| {
                            info.panes
                                .iter()
                                .find(|pane| pane.key == selection.pane)
                                .map(|pane| (pane.rect, pane.top))
                        })
                };
                if let (Some(selection), Some((rect, top))) =
                    (self.conversation_state.selection.clone(), edge)
                {
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
                    if let Some(selection) = &mut self.conversation_state.selection {
                        selection.head = (top + (row - rect.y) as usize, column);
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                if let Some(selection) = &self.conversation_state.selection {
                    if selection.anchor == selection.head {
                        self.conversation_state.selection = None;
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
            // The palette, while open, takes the wheel: nothing behind it moves.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.palette_open() => {
                self.scroll_palette(matches!(mouse.kind, MouseEventKind::ScrollUp));
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
                    let (lines, height, tab) = {
                        let info = self.frame.borrow();
                        (info.sidebar_lines, info.sidebar_height, info.sidebar_tab)
                    };
                    let mut tops = self.list_top.borrow_mut();
                    tops[tab] = (tops[tab] as isize + delta)
                        .clamp(0, lines.saturating_sub(height) as isize)
                        as usize;
                } else if let Some(key) = pane {
                    // An attached terminal scrolls its own history (or tells its program).
                    match self.native_terminal() {
                        Some(native)
                            if self.terminal.as_ref().is_some_and(|view| {
                                key == Pane::Terminal(view.agent.clone()).key()
                            }) =>
                        {
                            native.wheel(-(delta as i32));
                        }
                        _ => self.scroll_pane(&key, delta),
                    }
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, hit: Hit) {
        match hit {
            Hit::GlassMenu => self.open_palette(Some(4), glass::Open::Here),
            Hit::PaletteSection(section) => self.open_palette(Some(section), glass::Open::Here),
            Hit::NewAgent => self.open_new_agent(None),
            Hit::Home if self.home_open() => self.close_home(),
            Hit::Home => self.open_home(),
            // The terminal may be on another machine than stui (over SSH or fabric): the
            // clipboard is the person's, so the link lands where their browser is.
            Hit::Link(url) => {
                copy(&url);
                self.flash(format!(
                    "Copied {} · paste it in a browser",
                    text::truncate(&url, 60)
                ));
            }
            Hit::Split(right) => self.split(right),
            Hit::GlassTab(group, tab) => self.show_in(group, tab),
            Hit::GlassAdd(group) => {
                self.focus_group(group);
                self.open_palette(None, glass::Open::Tab);
            }
            Hit::PaletteChoice(index) => self.open_choice(Some(index), glass::Open::Here),
            Hit::Tab(tab) => self.switch_tab(tab),
            Hit::Row(index) => self.select(index),
            Hit::NewTerminal => self.open_new_terminal(),
            Hit::SidebarRow(index) => {
                if let Some(glasses) = self.glasses.as_mut() {
                    let sidebar = &mut glasses.sidebar;
                    sidebar.selected[sidebar.section] = index;
                }
                self.open_from_sidebar();
            }
            Hit::SidebarSection(section) => {
                if let Some(glasses) = self.glasses.as_mut() {
                    glasses.sidebar.section = section;
                    glasses.sidebar.focused = true;
                }
            }
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
            Hit::Key('\t') if self.agent_form => {
                if let Some(form) = self.new_agent.as_mut() {
                    form.focus = (form.focus + 1) % screens::AgentForm::FIELDS;
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
            Hit::Enter if self.agent_form && self.tab == 1 => self.start_agent(),
            Hit::Escape if self.agent_form && self.tab == 1 => self.cancel_agent_form(),
            Hit::Enter => {
                if self.chat.is_some() {
                    self.submit_chat()
                } else {
                    self.submit()
                }
            }
            Hit::Escape if self.new_mission.is_some() => {
                self.new_mission = None;
                self.close_form_tab();
            }
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
            Hit::ToggleTool(id) | Hit::Pane(PaneIntent::Expand(id)) => {
                self.conversation_state.expand(id);
            }
            Hit::Pane(PaneIntent::Open(id)) => self.open(&id),
            Hit::Pane(PaneIntent::Send(text)) => {
                if let Some(key) = self.draft_key() {
                    self.conversation_state.drafts.insert(key, text);
                    self.submit();
                }
            }
            Hit::Pane(PaneIntent::LoadOlder) => {
                self.flash("Earlier history is not available through stui yet");
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
            Hit::Voice => {
                self.editing = true;
                self.start_voice();
            }
            Hit::Open(id) => self.open(&id),
            Hit::Field(index) if self.agent_form => {
                if let Some(form) = self.new_agent.as_mut() {
                    form.focus = index;
                }
            }
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
        let selection = self.conversation_state.selection.as_ref()?;
        let info = self.frame.borrow();
        let pane = info.panes.iter().find(|pane| pane.key == selection.pane)?;
        Some(selection.text(&pane.lines))
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
                self.world.usage = full.usage.clone();
                self.world.usage_limits = full.usage_limits.clone();
                self.world.worktrees = full.worktrees.clone();
                self.world.conversations = full.conversations;
                if let Some(Load::Ready(entries)) = self
                    .world
                    .conversations
                    .get_mut("agent/example/harbor/reviewer")
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
            Some("agent/example/cos") if demo.cos_seen.is_none() => demo.cos_seen = Some(now),
            Some("agent/example/harbor/reviewer") if demo.harbor_seen.is_none() => {
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
                    self.world.conversations.get_mut("agent/example/cos")
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
                .get_mut("agent/example/harbor/reviewer")
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
            if let Some(Load::Ready(entries)) =
                self.world.conversations.get_mut("agent/example/cos")
            {
                entries.push(demo::late_mail());
            }
        }
    }
}

/// Editing at the text's end, for an input without a cursor of its own (the palette's query).
fn edit_text(text: &mut String, key: KeyEvent) -> bool {
    edit::edit(text, &edit::Cursor::default(), "", key)
}

/// Where `query` shows in a drawn document, case aside: (line, display column, display width).
fn find_matches(doc: &Doc, query: &str) -> Vec<(usize, usize, usize)> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (index, line) in doc.lines.iter().enumerate() {
        let plain = st3_conversation_ui::text::plain(line);
        let lower = plain.to_lowercase();
        // Lowercasing keeps byte offsets only for ASCII; elsewhere match the line as written.
        let haystack = if lower.len() == plain.len() {
            &lower
        } else {
            &plain
        };
        let mut from = 0;
        while let Some(offset) = haystack[from..].find(&query) {
            let start = from + offset;
            let end = start + query.len();
            found.push((
                index,
                text::width(&plain[..start]),
                text::width(&plain[start..end]),
            ));
            from = end;
        }
    }
    found
}

/// The size of an attachment's thumbnail, in cells.
const THUMBNAIL: ratatui::layout::Size = ratatui::layout::Size {
    width: 10,
    height: 4,
};


fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
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

struct Guard {
    enhanced: bool,
}

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
    /// Take over the terminal, and start the watch that ends stui once its terminal is gone so
    /// a stuck read can never outlive it. `keys`: ask the terminal to report modifiers it
    /// usually keeps, such as Cmd on macOS, where it can (kitty's keyboard protocol). Only
    /// glasses ask, for Cmd+K.
    fn enter(keys: bool) -> Result<Self> {
        crate::watch_terminal_hangup();
        enable_raw_mode()?;
        // A paste arrives whole, so its newlines never press Enter.
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            crossterm::event::EnableBracketedPaste
        )?;
        let enhanced = keys && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        if enhanced {
            execute!(
                io::stdout(),
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            )?;
        }
        Ok(Self { enhanced })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.enhanced {
            let _ = execute!(io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            io::stdout(),
            crossterm::event::DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

/// Which space to open: `stui --space NAME` names one; otherwise the last one used on this
/// device (`Some(None)`). Spaces are how stui works; `--classic` keeps the old layout for a
/// while (`None`). The older `--glass` and `--glasses` still work.
pub fn glass_request(args: &[String]) -> Option<Option<String>> {
    if args.iter().any(|arg| arg == "--classic") {
        return None;
    }
    Some(arg(args, "--space").or_else(|| arg(args, "--glass")))
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
    let glass = glass_request(args);
    let _guard = Guard::enter(glass.is_some())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let mut ui = Ui::new(demo::loading());
    ui.load_prefs();
    // The demo keeps its glasses in memory only, and shows the sidebar as a new device does.
    ui.glasses = glass.map(|name| {
        let mut glasses = glass::Glasses::open(name, None);
        glasses.sidebar.shown = true;
        glasses
    });
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
        ui.step_voice();
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
            // Drain everything queued so a fast wheel does not lag behind. crossterm's read never
            // returns on a closed terminal, so check for one before each read.
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) && !crate::stdin_hung_up() {
                match event::read()? {
                    Event::Key(key) => ui.key(key),
                    Event::Paste(text) => ui.paste(text),
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
    ui.glasses = glass_request(args).map(|name| glass::Glasses::open(name, None));
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    // Keys: each character is a key; "\n" is Enter, "<esc>", "<end>", "<pgdn>", "<pgup>", "<down>";
    // "<c-k>" is Ctrl+K and "<a-1>" Alt+1.
    if let Some(keys) = arg(args, "--keys") {
        let mut rest = keys.as_str();
        while !rest.is_empty() {
            terminal.draw(|frame| ui.render(frame))?;
            let mut modifiers = KeyModifiers::NONE;
            let (code, len) =
                if let Some(end) = rest.strip_prefix('<').and_then(|tail| tail.find('>')) {
                    let mut name = &rest[1..=end];
                    for (prefix, modifier) in
                        [("c-", KeyModifiers::CONTROL), ("a-", KeyModifiers::ALT)]
                    {
                        if let Some(key) = name.strip_prefix(prefix) {
                            modifiers |= modifier;
                            name = key;
                        }
                    }
                    let code = match name {
                        "esc" => KeyCode::Esc,
                        "end" => KeyCode::End,
                        "pgdn" => KeyCode::PageDown,
                        "pgup" => KeyCode::PageUp,
                        "down" => KeyCode::Down,
                        "up" => KeyCode::Up,
                        "enter" => KeyCode::Enter,
                        "tab" => KeyCode::Tab,
                        "bs" => KeyCode::Backspace,
                        _ if name.chars().count() == 1 => KeyCode::Char(name.chars().next().unwrap()),
                        _ => KeyCode::Null,
                    };
                    (code, end + 2)
                } else {
                    let character = rest.chars().next().unwrap_or(' ');
                    (KeyCode::Char(character), character.len_utf8())
                };
            ui.key(KeyEvent::new(code, modifiers));
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
    // `--pane KEY` draws that one pane alone, at the given size.
    let pane = match arg(args, "--pane") {
        Some(key) => {
            Some(Pane::parse(&key).ok_or_else(|| anyhow::anyhow!("no pane is called {key}"))?)
        }
        None => None,
    };
    // Draw twice so panes that follow the end settle.
    for _ in 0..2 {
        terminal.draw(|frame| match &pane {
            Some(pane) => ui.render_pane(frame, pane),
            None => ui.render(frame),
        })?;
    }
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

    #[test]
    fn drafts_take_the_terminal_editing_keys() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let mut text = "look at the  ".to_owned();
        assert!(edit_text(
            &mut text,
            key(KeyCode::Char('w'), KeyModifiers::CONTROL)
        ));
        assert_eq!(text, "look at ");
        edit_text(&mut text, key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(text, "look ");
        edit_text(&mut text, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(text, "look ", "a control key never types its letter");
        edit_text(&mut text, key(KeyCode::Char('X'), KeyModifiers::SHIFT));
        assert_eq!(text, "look X");
        let mut lines = "first\nsecond line".to_owned();
        edit_text(&mut lines, key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(lines, "first\n");
        edit_text(&mut lines, key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(lines, "first");
        assert!(!edit_text(&mut lines, key(KeyCode::Up, KeyModifiers::NONE)));
    }

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
    fn answering_a_request_leaves_the_next_one_closed() {
        let mut world = demo::world();
        let asks = |id: &str| Attention {
            id: id.into(),
            tier: Tier::Stopped,
            title: "Merge the three PRs?".into(),
            waiting: Some("Planner on lark".into()),
            age: "2m".into(),
            mission: None,
            agent: Some("agent/example/planner".into()),
            kind: AttentionKind::Request {
                from: "Planner".into(),
                from_id: "agent/example/planner".into(),
                question: "Answer yes and I land them.".into(),
            },
            actions: vec!["work.done".into()],
            related: Vec::new(),
            raised_by: None,
        };
        if let Load::Ready(items) = &mut world.attention {
            items.insert(0, asks("attention/request-one"));
            items.insert(1, asks("attention/request-two"));
        }
        let mut ui = Ui::new(world);
        ui.live = true;
        ui.tab = 0;
        let request = ui
            .listing(60)
            .ids
            .iter()
            .position(|id| id == "attention/request-one")
            .expect("the request is listed");
        ui.selected[0] = request;
        ui.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(ui.editing, "c opens the answer box");
        for letter in "fine".chars() {
            ui.key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE));
        }
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!ui.editing, "the answer is sent and the box closes");
    }

    #[test]
    fn shift_o_simplifies_every_conversation_and_back() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        let agent = ui.selected_id().unwrap();
        let tools = |ui: &Ui| {
            let screen = frame(ui, 120, 40).join("\n");
            (screen.contains("tool calls ·"), screen)
        };
        assert!(!tools(&ui).0, "full by default");
        ui.key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT));
        assert!(ui.simple);
        let (bundled, screen) = tools(&ui);
        assert!(
            bundled || !screen.contains("$ "),
            "a run of calls folds to one line in {agent}: {screen}"
        );
        ui.key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT));
        assert!(!ui.simple);
    }

    #[test]
    fn an_agents_question_gets_room_to_read() {
        let question = "Recommend: yes, in three parts.\nWhy: the release run failed.\nMy proposal:\n1. Land #1049.\n2. Build on main.\n```\ncargo build\nnext\n```\nAnswer yes and I queue it.";
        assert_eq!(
            screens::spaced(question),
            "**Recommend:** yes, in three parts.\n\n**Why:** the release run failed.\n\nMy proposal:\n1. Land #1049.\n2. Build on main.\n```\ncargo build\nnext\n```\nAnswer yes and I queue it."
        );
        // A long lead before a colon is a sentence, not a label.
        assert_eq!(
            screens::spaced("The release run on the Linux runner failed: mold is missing"),
            "The release run on the Linux runner failed: mold is missing"
        );
    }

    #[test]
    fn spoken_words_are_sent_tagged_dictated() {
        let mut ui = Ui::new(demo::world());
        ui.live = true;
        ui.tab = 1;
        let agent = ui.selected_id().unwrap();
        ui.editing = true;
        ui.conversation_state
            .drafts
            .insert(agent.clone(), "Also:".into());
        ui.voice = Some(voice::VoiceState::stand_in(&agent));
        ui.voice_event(crate::voice::Event::Ready {
            device: "Desk Microphone".into(),
        });
        ui.voice_event(crate::voice::Event::Level(0.6));
        ui.voice_event(crate::voice::Event::Text {
            text: "ship the".into(),
            settled: false,
        });
        let screen = frame(&ui, 120, 30).join("\n");
        assert!(screen.contains("listening · Desk Microphone"), "{screen}");
        assert!(screen.contains("ship the"), "{screen}");
        // Typing waits while listening; Enter sends once the words are all in.
        ui.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(ui.effects.is_empty(), "nothing goes before the last words");
        ui.voice_event(crate::voice::Event::Done {
            text: "ship the harbor fix".into(),
        });
        assert!(ui.voice.is_none());
        let sent: Vec<_> = ui.effects.iter().filter(|e| voice::dictated(e)).collect();
        assert!(
            matches!(&sent[..], [Effect::Send { text, .. }] if text == "Also: ship the harbor fix"),
            "{:?}",
            ui.effects
        );
    }

    #[test]
    fn spoken_words_kept_to_edit_are_tagged_when_sent_and_esc_drops_them() {
        let mut ui = Ui::new(demo::world());
        ui.live = true;
        ui.tab = 1;
        let agent = ui.selected_id().unwrap();
        ui.editing = true;
        ui.voice = Some(voice::VoiceState::stand_in(&agent));
        ui.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        ui.voice_event(crate::voice::Event::Done {
            text: "check the logs".into(),
        });
        assert!(ui.effects.is_empty(), "Tab keeps the words to edit");
        assert_eq!(
            ui.conversation_state.drafts.get(&agent).map(String::as_str),
            Some("check the logs")
        );
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(ui.effects.iter().any(voice::dictated), "{:?}", ui.effects);

        ui.effects.clear();
        ui.voice = Some(voice::VoiceState::stand_in(&agent));
        ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(ui.voice.is_none());
        assert!(ui.effects.is_empty());
        // A later typed message is not tagged.
        ui.conversation_state
            .drafts
            .insert(agent.clone(), "typed".into());
        ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!ui.effects.iter().any(voice::dictated), "{:?}", ui.effects);
    }

    #[test]
    fn a_first_page_st_keeps_refusing_says_why_while_stui_tries_again() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        let agent = ui.selected_id().unwrap();
        ui.world.conversations.insert(agent.clone(), Load::Loading);
        ui.conversation_failed(&agent, "the list changed while it was being read");
        let screen = frame(&ui, 120, 30).join("\n");
        assert!(
            screen.contains("Not loaded yet: the list changed"),
            "{screen}"
        );
        assert!(!screen.contains("Loading the conversation"), "{screen}");
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
    fn every_pane_draws_alone_from_its_key_at_any_size() {
        let mut ui = Ui::new(demo::world());
        let first = |tab: usize| {
            ui.listing_for(tab, 40)
                .ids
                .first()
                .cloned()
                .expect("the demo has one")
        };
        let (home, agent, mission, machine) = (first(0), first(1), first(2), first(3));
        let name = |id: &str| {
            ui.world
                .agents
                .items()
                .iter()
                .find(|candidate| candidate.id == id)
                .map(|candidate| candidate.name.clone())
                .unwrap()
        };
        let agent_name = name(&agent);
        let mission_title = ui
            .world
            .missions
            .items()
            .iter()
            .find(|candidate| candidate.id == mission)
            .map(|candidate| candidate.title.clone())
            .unwrap();
        ui.terminal = Some(TerminalView {
            agent: agent.clone(),
            name: agent_name.clone(),
            title: agent_name.clone(),
            lines: vec![Line::from("$ ready")],
            cursor: None,
            stale: None,
            ended: None,
            native: None,
        });
        for (key, shows) in [
            ("list:missions".to_owned(), "needs you".to_owned()),
            (format!("home:{home}"), String::new()),
            (format!("agent:{agent}"), agent_name.clone()),
            (format!("terminal:{agent}"), "$ ready".to_owned()),
            (format!("mission:{mission}"), mission_title.clone()),
            (format!("declaration:{mission}"), String::new()),
            (format!("machine:machine/{machine}"), machine.clone()),
            ("new-mission:".to_owned(), String::new()),
        ] {
            let pane = Pane::parse(&key).unwrap();
            for (width, height) in [(44, 14), (120, 40)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.render_pane(frame, &pane)).unwrap();
                let screen = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    !screen.trim().is_empty(),
                    "{key} drew nothing at {width}x{height}"
                );
                assert!(
                    screen.contains(&shows),
                    "{key} at {width}x{height} does not show {shows:?}"
                );
            }
        }
        // A terminal pane for an agent that is not attached says so.
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal
            .draw(|frame| ui.render_pane(frame, &Pane::Terminal("agent/example/nobody".into())))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Not attached"));
    }

    #[test]
    fn a_conversation_read_back_keeps_its_place_while_entries_come_and_go() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        let id = ui.selected_id().unwrap();
        frame(&ui, 120, 30);
        let pane = ui
            .frame
            .borrow()
            .panes
            .iter()
            .find(|pane| pane.key.starts_with("chat:"))
            .map(|pane| pane.rect)
            .unwrap();
        for _ in 0..4 {
            ui.mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: pane.x + 2,
                row: pane.y + 2,
                modifiers: KeyModifiers::NONE,
            });
            frame(&ui, 120, 30);
        }
        // The rows read, without the scrollbar, whose thumb moves as the length changes.
        let shown = |ui: &Ui| {
            frame(ui, 120, 30)[usize::from(pane.y) + 1..usize::from(pane.y) + 8]
                .iter()
                .map(|line| {
                    line.chars()
                        .take(usize::from(pane.x + pane.width) - 2)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let before = shown(&ui);
        // The window slides (the oldest entry leaves), a reply arrives, and an entry above
        // grows: what is being read stays where it is.
        let Some(Load::Ready(entries)) = ui.world.conversations.get_mut(&id) else {
            panic!("the demo agent has a conversation")
        };
        entries.remove(0);
        entries.push(Entry {
            id: "late".into(),
            at: "09:59".into(),
            body: Body::Assistant("a reply arrives below".into()),
        });
        assert_eq!(shown(&ui), before);
        // At the bottom it follows as before.
        press(&mut ui, KeyCode::End);
        frame(&ui, 120, 30);
        assert!(
            frame(&ui, 120, 30)
                .join("\n")
                .contains("a reply arrives below")
        );
    }

    #[test]
    fn a_click_on_a_subagent_line_only_selects_its_agent() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        let ids = ui.ids();
        let builder = ids
            .iter()
            .position(|id| id == "agent/example/atlas/builder")
            .unwrap();
        let other = ids.iter().position(|id| id == "agent/example/cos").unwrap();
        ui.select(other);
        let screen = frame(&ui, 140, 40);
        let line = screen
            .iter()
            .position(|line| line.contains("run the slow tests"))
            .expect("the builder's subagent is drawn beneath it");
        let (rect, hit) = ui
            .frame
            .borrow()
            .hits
            .iter()
            .find(|(rect, _)| rect.y == line as u16 && rect.x == 0)
            .map(|(rect, hit)| (*rect, hit.clone()))
            .unwrap();
        assert_eq!(
            hit,
            Hit::Row(builder),
            "the line belongs to its agent's row"
        );
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 2,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            ui.selected_id().as_deref(),
            Some("agent/example/atlas/builder")
        );
        assert_eq!(ui.tab, 1);
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
            .position(|id| id == "agent/example/cos")
            .unwrap();
        ui.select(cos);
        frame(&ui, 140, 30);
        let key = "chat:agent/example/cos".to_owned();
        assert!(ui.conversation_state.panes.borrow()[&key].follow);
        ui.scroll_pane(&key, -5);
        assert!(!ui.conversation_state.panes.borrow()[&key].follow);
        if let Some(Load::Ready(entries)) = ui.world.conversations.get_mut("agent/example/cos") {
            entries.push(demo::late_mail());
        }
        let screen = frame(&ui, 140, 30).join("\n");
        assert!(screen.contains("new lines"), "{screen}");
        ui.follow_latest();
        frame(&ui, 140, 30);
        assert!(ui.conversation_state.panes.borrow()[&key].follow);
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
            ui.conversation_state.selection.is_none(),
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
        let selection = ui.conversation_state.selection.clone().unwrap();
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
            ("attention/1", "c or click to write"),
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
        assert_eq!(ui.chat.as_ref().unwrap().to, "agent/example/atlas/builder");
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
        assert_eq!(ui.chat_target(&item).unwrap().0, "agent/example/cos");
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
                agent.id = format!("agent/example/extra/{index}");
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
            agent: "agent/example/harbor/keeper".into(),
            name: "Keeper".into(),
            title: "Keeper · vim".into(),
            lines: (0..40)
                .map(|row| Line::from(format!("row {row}")))
                .collect(),
            cursor: Some((5, 2)),
            stale: Some("st closed the connection".into()),
            ended: None,
            native: None,
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
    fn ctrl_bracket_opens_an_agents_terminal_and_ctrl_backslash_returns() {
        let mut ui = Ui::new(demo::world());
        ui.tab = 1;
        press(&mut ui, KeyCode::Enter);
        assert!(ui.terminal.is_none(), "Enter alone never attaches");
        ui.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
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
