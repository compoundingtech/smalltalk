//! Live terminal screens for client viewers.
//!
//! One read-only PTY connection per watched terminal incarnation feeds a terminal emulator.
//! Viewers share it and read whole screens: each screen replaces every earlier one, so a slow
//! viewer skips to the latest and nothing is replayed. An idle terminal produces no screens.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use libghostty_vt::render::{CursorVisualStyle, RenderState};
use libghostty_vt::screen::{Cell, CellContentTag, CellWide, GridRef};
use libghostty_vt::style::{Style as GhosttyStyle, StyleColor, Underline};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate};
use pty_terminal::{TerminalActor, TerminalEvent};
use pty_core::protocol::{MessageType, PacketReader, decode_geometry, encode_peek};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

pub(super) const TERMINAL_MAX_LINES: usize = 200;
pub(super) const TERMINAL_MAX_LINE_BYTES: usize = 4_096;
/// The fastest a changing terminal publishes screens after the first change.
pub(super) const SCREEN_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// A first change waits this long so one redraw written in several pieces arrives whole.
const SCREEN_SETTLE: Duration = Duration::from_millis(4);
/// How long a watcher keeps its PTY connection after its last viewer leaves.
const WATCHER_LINGER: Duration = Duration::from_secs(10);
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLUMNS: u16 = 80;

/// What a watched terminal incarnation currently shows.
#[derive(Clone, Debug)]
pub(super) enum ViewState {
    Connecting,
    Screen(Arc<Screen>),
    Ended(ViewEnd),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ViewEnd {
    /// The PTY session closed or its process exited.
    Exited,
    /// The PTY session could not be read.
    Unavailable(String),
    /// No viewer remained; nobody observes this state.
    Idle,
}

/// One complete screen. `body` holds every field that depends only on the terminal, and
/// `revision` digests it so equal screens are never published twice.
#[derive(Debug)]
pub(super) struct Screen {
    body: serde_json::Map<String, Value>,
    revision: String,
}

impl Screen {
    pub(super) fn revision(&self) -> &str {
        &self.revision
    }

    /// An opaque screen fence that fits exactly in a JavaScript number. It is stable
    /// across watcher/daemon restarts and depends only on the observed screen; incarnation
    /// is fenced separately. Clients compare it for equality, never order it.
    pub(super) fn sequence(&self) -> u64 {
        u64::from_str_radix(&self.revision[..13], 16).expect("screen revisions are SHA-256 hex")
    }

    /// The client `TerminalScreen` value for one viewer.
    pub(super) fn value(&self, terminal_id: &str, incarnation: &str) -> Value {
        let mut value = serde_json::Map::with_capacity(self.body.len() + 5);
        value.insert("kind".into(), "terminal-screen".into());
        value.insert("terminal_id".into(), terminal_id.into());
        value.insert("runtime_incarnation".into(), incarnation.into());
        value.extend(self.body.clone());
        value.insert("revision".into(), self.revision.clone().into());
        value.insert("next_sequence".into(), self.sequence().into());
        Value::Object(value)
    }
}

type WatchKey = (PathBuf, String);

static WATCHERS: LazyLock<Mutex<HashMap<WatchKey, Arc<watch::Sender<ViewState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Subscribe to the screens of one terminal incarnation, starting its watcher when none runs.
/// The receiver's current value is the latest screen, so a new viewer starts from it.
pub(super) fn subscribe(
    pty_root: &Path,
    runtime_id: &str,
    incarnation: &str,
) -> watch::Receiver<ViewState> {
    let key = (
        pty_root.join(format!("{runtime_id}.sock")),
        incarnation.to_owned(),
    );
    let mut watchers = WATCHERS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(sender) = watchers.get(&key) {
        return sender.subscribe();
    }
    let (sender, receiver) = watch::channel(ViewState::Connecting);
    let sender = Arc::new(sender);
    let job = WatchJob {
        key: key.clone(),
        sender: sender.clone(),
        fallback_title: runtime_id.to_owned(),
    };
    if EMULATION.send(job).is_err() {
        sender.send_replace(ViewState::Ended(ViewEnd::Unavailable(
            "the terminal emulation thread stopped".to_owned(),
        )));
        return receiver;
    }
    watchers.insert(key, sender);
    receiver
}

/// One watcher handed to the emulation thread.
struct WatchJob {
    key: WatchKey,
    sender: Arc<watch::Sender<ViewState>>,
    fallback_title: String,
}

/// libghostty's terminal is `!Send`, so no watcher may live on the shared multi-threaded
/// runtime. Every watcher runs on this one thread, which owns a current-thread runtime and a
/// `LocalSet`; `subscribe` only hands the job over.
static EMULATION: LazyLock<tokio::sync::mpsc::UnboundedSender<WatchJob>> = LazyLock::new(|| {
    let (jobs, mut incoming) = tokio::sync::mpsc::unbounded_channel::<WatchJob>();
    let started = std::thread::Builder::new()
        .name("st3-terminal-view".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                while let Some(job) = incoming.recv().await {
                    tokio::task::spawn_local(run_watcher(job));
                }
            });
        });
    if let Err(error) = started {
        tracing::error!(%error, "start the terminal emulation thread");
    }
    jobs
});

async fn run_watcher(job: WatchJob) {
    let registration = Registration {
        key: job.key,
        sender: job.sender,
    };
    let end = watch_terminal(&registration.key, &job.fallback_title, &registration.sender).await;
    let sender = registration.sender.clone();
    drop(registration);
    sender.send_replace(ViewState::Ended(end));
}

/// Removes a watcher from the registry when its task ends, panics, or is cancelled, so a later
/// viewer never subscribes to a watcher that no longer runs.
struct Registration {
    key: WatchKey,
    sender: Arc<watch::Sender<ViewState>>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut watchers = WATCHERS.lock().unwrap_or_else(|error| error.into_inner());
        if watchers
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.sender))
        {
            watchers.remove(&self.key);
        }
    }
}

/// Wait for a screen whose revision differs from `after`, or for any screen when `after` is
/// `None`. Returns the latest screen when `deadline` passes first, if there is one.
pub(super) async fn next_screen(
    receiver: &mut watch::Receiver<ViewState>,
    after: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<Option<Arc<Screen>>, ViewEnd> {
    loop {
        let latest = match &*receiver.borrow_and_update() {
            ViewState::Connecting => None,
            ViewState::Screen(screen) => Some(screen.clone()),
            ViewState::Ended(end) => return Err(end.clone()),
        };
        if let Some(screen) = &latest
            && after != Some(screen.revision())
        {
            return Ok(latest);
        }
        match tokio::time::timeout_at(deadline, receiver.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(ViewEnd::Exited),
            Err(_) => return Ok(latest),
        }
    }
}

enum Follow {
    Resync,
    End(ViewEnd),
}

async fn watch_terminal(
    key: &WatchKey,
    fallback_title: &str,
    sender: &Arc<watch::Sender<ViewState>>,
) -> ViewEnd {
    // A program sets its title once; the replay does not carry it, so keep it across resyncs.
    let title = Title::default();
    loop {
        match follow(key, fallback_title, sender, &title).await {
            Follow::Resync => continue,
            Follow::End(end) => return end,
        }
    }
}

/// Read one PEEK connection: the atomic replay, then live output. A geometry change ends the
/// connection with `Resync`, so the next replay carries the owner's reflowed screen.
async fn follow(
    key: &WatchKey,
    fallback_title: &str,
    sender: &Arc<watch::Sender<ViewState>>,
    title: &Title,
) -> Follow {
    let (socket, _) = key;
    let mut stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(error) => {
            return Follow::End(ViewEnd::Unavailable(format!(
                "connect to the terminal session: {error}"
            )));
        }
    };
    if let Err(error) = stream.write_all(&encode_peek(false, false)).await {
        return Follow::End(ViewEnd::Unavailable(format!(
            "request the terminal screen: {error}"
        )));
    }
    let mut reader = PacketReader::new();
    let mut bytes = vec![0_u8; 65_536];
    let mut size = None;
    let mut emulator: Option<Emulator> = None;
    let mut publish_at: Option<tokio::time::Instant> = None;
    let mut last_publish: Option<tokio::time::Instant> = None;
    let mut linger_until: Option<tokio::time::Instant> = None;
    loop {
        let sync_deadline = emulator
            .as_ref()
            .and_then(Emulator::sync_deadline)
            .map(tokio::time::Instant::from_std);
        tokio::select! {
            read = stream.read(&mut bytes) => {
                let count = match read {
                    Ok(0) => return Follow::End(ViewEnd::Exited),
                    Ok(count) => count,
                    Err(error) => return Follow::End(ViewEnd::Unavailable(format!(
                        "read the terminal session: {error}"
                    ))),
                };
                let packets = match reader.feed(&bytes[..count]) {
                    Ok(packets) => packets,
                    Err(error) => return Follow::End(ViewEnd::Unavailable(format!(
                        "decode the terminal session: {error}"
                    ))),
                };
                let mut changed = false;
                for packet in packets {
                    match packet.type_ {
                        MessageType::Geometry => {
                            let geometry = decode_geometry(&packet.payload);
                            if emulator.is_some() && size != Some(geometry) {
                                return Follow::Resync;
                            }
                            size = Some(geometry);
                        }
                        MessageType::Screen => {
                            let (rows, columns) = size.unwrap_or((DEFAULT_ROWS, DEFAULT_COLUMNS));
                            let mut replayed = Emulator::new(rows, columns, title.clone());
                            replayed.feed(&packet.payload);
                            emulator = Some(replayed);
                            changed = true;
                        }
                        MessageType::Data => {
                            if let Some(emulator) = emulator.as_mut() {
                                emulator.feed(&packet.payload);
                                changed = true;
                            }
                        }
                        MessageType::Exit => return Follow::End(ViewEnd::Exited),
                        _ => {}
                    }
                }
                if changed && publish_at.is_none() {
                    let now = tokio::time::Instant::now();
                    let settled = now + SCREEN_SETTLE;
                    publish_at = Some(match last_publish {
                        Some(last) => settled.max(last + SCREEN_MIN_INTERVAL),
                        None => settled,
                    });
                }
            }
            _ = tokio::time::sleep_until(publish_at.unwrap_or_else(tokio::time::Instant::now)),
                if publish_at.is_some() && emulator.as_ref().is_some_and(|e| e.sync_deadline().is_none()) =>
            {
                publish_at = None;
                let Some(emulator) = emulator.as_ref() else { continue };
                let screen = emulator.screen(fallback_title);
                let unchanged = matches!(
                    &*sender.borrow(),
                    ViewState::Screen(current) if current.revision == screen.revision
                );
                if !unchanged {
                    sender.send_replace(ViewState::Screen(Arc::new(screen)));
                    last_publish = Some(tokio::time::Instant::now());
                }
            }
            _ = tokio::time::sleep_until(sync_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if sync_deadline.is_some() =>
            {
                // A synchronized update that never ends is shown when its timeout passes.
                if let Some(emulator) = emulator.as_mut() {
                    emulator.stop_sync();
                }
                publish_at.get_or_insert_with(tokio::time::Instant::now);
            }
            _ = sender.closed(), if linger_until.is_none() => {
                linger_until = Some(tokio::time::Instant::now() + WATCHER_LINGER);
            }
            _ = tokio::time::sleep_until(linger_until.unwrap_or_else(tokio::time::Instant::now)),
                if linger_until.is_some() =>
            {
                linger_until = None;
                let mut watchers = WATCHERS.lock().unwrap_or_else(|error| error.into_inner());
                if sender.receiver_count() == 0 {
                    if watchers.get(key).is_some_and(|current| Arc::ptr_eq(current, sender)) {
                        watchers.remove(key);
                    }
                    return Follow::End(ViewEnd::Idle);
                }
            }
        }
    }
}

/// The title a program set. A program sets its title once and the replay does not carry it, so
/// it outlives each emulator across resyncs. Every watcher runs on the emulation thread, so a
/// shared `Rc` is enough.
#[derive(Clone, Default)]
struct Title(Rc<RefCell<Option<String>>>);

impl Title {
    fn get(&self) -> Option<String> {
        self.0.borrow().clone()
    }
}

/// How long a synchronized update (`?2026h`) may hold changes back before the screen is shown.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// End of a synchronized update.
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// libghostty, through pty-terminal: the emulator the pty daemon and Fractal run, so every client
/// sees the same cells, widths, and modes. Its terminal is `!Send`; see [`EMULATION`].
pub(super) struct Emulator {
    actor: TerminalActor,
    render: Option<RefCell<RenderState<'static>>>,
    title: Title,
    sync_since: Option<Instant>,
    sync_shown: bool,
}

impl Emulator {
    fn new(rows: u16, columns: u16, title: Title) -> Self {
        let actor = TerminalActor::new(rows.max(1), columns.max(1), 0);
        Self {
            actor,
            render: RenderState::new().ok().map(RefCell::new),
            title,
            sync_since: None,
            sync_shown: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        // A viewer never answers the program's queries; the owner's attached client does.
        let _ = self.actor.write(bytes);
        let _ = self.actor.take_pty_replies();
        for event in self.actor.take_events() {
            if let TerminalEvent::TitleChange(title) = event {
                *self.title.0.borrow_mut() = (!title.is_empty()).then_some(title);
            }
        }
        // libghostty applies a synchronized update as it arrives, so only the mode after the
        // write is visible; an update that ended inside this chunk still restarts the hold.
        if bytes.windows(SYNC_END.len()).any(|window| window == SYNC_END) {
            self.sync_since = None;
            self.sync_shown = false;
        }
        if self.actor.terminal().mode(Mode::SYNC_OUTPUT).unwrap_or(false) {
            self.sync_since.get_or_insert_with(Instant::now);
        } else {
            self.sync_since = None;
            self.sync_shown = false;
        }
    }

    fn sync_deadline(&self) -> Option<Instant> {
        if self.sync_shown {
            return None;
        }
        self.sync_since.map(|since| since + SYNC_TIMEOUT)
    }

    fn stop_sync(&mut self) {
        self.sync_shown = true;
    }

    fn cursor_style(&self) -> (&'static str, bool) {
        let Some(render) = &self.render else {
            return ("block", false);
        };
        let mut render = render.borrow_mut();
        let Ok(snapshot) = render.update(self.actor.terminal()) else {
            return ("block", false);
        };
        let style = match snapshot.cursor_visual_style() {
            Ok(CursorVisualStyle::Bar) => "bar",
            Ok(CursorVisualStyle::Underline) => "underline",
            _ => "block",
        };
        (style, snapshot.cursor_blinking().unwrap_or(false))
    }

    fn screen(&self, fallback_title: &str) -> Screen {
        let term = self.actor.terminal();
        let rows = usize::from(term.rows().unwrap_or(1));
        let columns = usize::from(term.cols().unwrap_or(1));
        let mut truncated = false;
        let mut lines = Vec::with_capacity(rows.min(TERMINAL_MAX_LINES));
        let mut graphemes = vec![char::default(); 16];
        let mut uri = vec![0_u8; 256];
        let mut text = String::new();
        for row in 0..rows {
            if row == TERMINAL_MAX_LINES {
                truncated = true;
                break;
            }
            let point = |column: usize| {
                Point::Active(PointCoordinate {
                    x: column as u16,
                    y: row as u32,
                })
            };
            let mut line = ScreenLine::default();
            for column in 0..columns {
                let Ok(cell_ref) = term.grid_ref(point(column)) else {
                    continue;
                };
                let cell = cell_ref.cell().ok();
                let cells = match cell.and_then(|cell| cell.wide().ok()) {
                    Some(CellWide::Wide) => 2,
                    Some(CellWide::SpacerTail | CellWide::SpacerHead) => continue,
                    _ => 1,
                };
                let pen = cell_ref.style().unwrap_or_default();
                let link = cell
                    .and_then(|cell| cell.has_hyperlink().ok())
                    .unwrap_or(false)
                    .then(|| read_link(&cell_ref, &mut uri))
                    .flatten();
                let style = Style::of(&pen, cell, link);
                if pen.invisible {
                    line.push(style, if cells == 2 { "  " } else { " " }, cells);
                } else {
                    read_text(&cell_ref, &mut graphemes, &mut text);
                    line.push(style, &text, cells);
                }
            }
            let wrapped = term
                .grid_ref(point(0))
                .ok()
                .and_then(|cell| cell.row().ok())
                .and_then(|row| row.is_wrap_continuation().ok())
                .unwrap_or(false);
            lines.push(line.finish().value(row, wrapped));
        }
        let (style, blinking) = self.cursor_style();
        let mode = |mode| term.mode(mode).unwrap_or(false);
        let mouse_tracking = if mode(Mode::ANY_MOUSE) {
            "motion"
        } else if mode(Mode::BUTTON_MOUSE) {
            "drag"
        } else if mode(Mode::NORMAL_MOUSE) || mode(Mode::X10_MOUSE) {
            "click"
        } else {
            "none"
        };
        let mouse_encoding = if mode(Mode::SGR_MOUSE) || mode(Mode::SGR_PIXELS_MOUSE) {
            "sgr"
        } else if mode(Mode::UTF8_MOUSE) {
            "utf8"
        } else {
            "default"
        };
        let body = json!({
            "rows": rows,
            "columns": columns,
            "cursor": {
                "row": term.cursor_y().unwrap_or(0),
                "column": usize::from(term.cursor_x().unwrap_or(0)).min(columns.saturating_sub(1)),
                "visible": term.is_cursor_visible().unwrap_or(true),
                "style": style,
                "blinking": blinking,
            },
            "title": self.title.get().unwrap_or_else(|| fallback_title.to_owned()),
            "modes": {
                "alternate_screen": self.actor.alt_screen_active(),
                "application_cursor": mode(Mode::DECCKM),
                "application_keypad": mode(Mode::KEYPAD_KEYS),
                "bracketed_paste": mode(Mode::BRACKETED_PASTE),
                "focus_events": mode(Mode::FOCUS_EVENT),
                "mouse_tracking": mouse_tracking,
                "mouse_encoding": mouse_encoding,
                "kitty_keyboard": self.actor.kitty_flags(),
            },
            "lines": lines,
            "truncated": truncated,
        });
        let Value::Object(body) = body else {
            unreachable!("the screen body is an object")
        };
        let revision = hex::encode(
            &Sha256::digest(serde_json::to_vec(&body).unwrap_or_default())[..12],
        );
        Screen { body, revision }
    }
}

fn read_text(cell: &GridRef<'_>, buffer: &mut Vec<char>, text: &mut String) {
    let count = match cell.graphemes(buffer) {
        Ok(count) => count,
        Err(libghostty_vt::error::Error::OutOfSpace { required }) => {
            buffer.resize(required, char::default());
            cell.graphemes(buffer).unwrap_or(0)
        }
        Err(_) => 0,
    };
    text.clear();
    text.extend(buffer[..count].iter());
    if text.is_empty() || text.starts_with('\0') {
        text.clear();
        text.push(' ');
    }
}

/// The cell's OSC 8 target, admitted by Fractal's rules: absolute http(s) URLs with a host, and
/// `file:` URLs naming an absolute local path. Anything else keeps its text and loses the link.
fn read_link(cell: &GridRef<'_>, buffer: &mut Vec<u8>) -> Option<String> {
    let count = match cell.hyperlink_uri(buffer) {
        Ok(count) => count,
        Err(libghostty_vt::error::Error::OutOfSpace { required }) => {
            buffer.resize(required, 0);
            cell.hyperlink_uri(buffer).ok()?
        }
        Err(_) => return None,
    };
    let raw = std::str::from_utf8(&buffer[..count]).ok()?;
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    let admitted = match url.scheme() {
        "http" | "https" => url.host().is_some(),
        "file" => url.to_file_path().is_ok(),
        _ => false,
    };
    admitted.then(|| url.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paint {
    Palette(u8),
    Rgb(u8, u8, u8),
}

impl Paint {
    fn of(color: StyleColor) -> Option<Self> {
        match color {
            StyleColor::None => None,
            StyleColor::Palette(index) => Some(Self::Palette(index.0)),
            StyleColor::Rgb(rgb) => Some(Self::Rgb(rgb.r, rgb.g, rgb.b)),
        }
    }

    fn value(self) -> Value {
        match self {
            Self::Palette(index) => index.into(),
            Self::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}").into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Style {
    fg: Option<Paint>,
    bg: Option<Paint>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    strikethrough: bool,
    link: Option<String>,
}

impl Style {
    fn of(pen: &GhosttyStyle, cell: Option<Cell>, link: Option<String>) -> Self {
        // A cell erased with a background keeps the colour in its content, not in a style.
        let bg = match cell.and_then(|cell| cell.content_tag().ok()) {
            Some(CellContentTag::BgColorPalette) => cell
                .and_then(|cell| cell.bg_color_palette().ok())
                .map(|index| Paint::Palette(index.0)),
            Some(CellContentTag::BgColorRgb) => cell
                .and_then(|cell| cell.bg_color_rgb().ok())
                .map(|rgb| Paint::Rgb(rgb.r, rgb.g, rgb.b)),
            _ => Paint::of(pen.bg_color),
        };
        Self {
            fg: Paint::of(pen.fg_color),
            bg,
            bold: pen.bold,
            dim: pen.faint,
            italic: pen.italic,
            underline: pen.underline != Underline::None,
            inverse: pen.inverse,
            strikethrough: pen.strikethrough,
            link,
        }
    }

    /// A blank cell in this style looks like no cell at all.
    fn blank_is_invisible(&self) -> bool {
        self.bg.is_none() && !self.inverse && !self.underline && !self.strikethrough
    }
}

struct Run {
    text: String,
    cells: usize,
    style: Style,
}

impl Run {
    fn value(&self) -> Value {
        let mut value = serde_json::Map::new();
        value.insert("text".into(), self.text.clone().into());
        value.insert("cells".into(), self.cells.into());
        if let Some(fg) = self.style.fg {
            value.insert("fg".into(), fg.value());
        }
        if let Some(bg) = self.style.bg {
            value.insert("bg".into(), bg.value());
        }
        for (name, set) in [
            ("bold", self.style.bold),
            ("dim", self.style.dim),
            ("italic", self.style.italic),
            ("underline", self.style.underline),
            ("inverse", self.style.inverse),
            ("strikethrough", self.style.strikethrough),
        ] {
            if set {
                value.insert(name.into(), true.into());
            }
        }
        if let Some(uri) = &self.style.link {
            value.insert("link".into(), json!({ "uri": uri }));
        }
        Value::Object(value)
    }
}

/// One row being built: cells join the last run while their style matches, and a row stops
/// growing at [`TERMINAL_MAX_LINE_BYTES`] on a grapheme boundary, so `cells` stays exact.
#[derive(Default)]
struct ScreenLine {
    text: String,
    runs: Vec<Run>,
    bytes: usize,
    truncated: bool,
}

impl ScreenLine {
    fn push(&mut self, style: Style, text: &str, cells: usize) {
        if self.truncated {
            return;
        }
        if self.bytes + text.len() > TERMINAL_MAX_LINE_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += text.len();
        match self.runs.last_mut() {
            Some(run) if run.style == style => {
                run.text.push_str(text);
                run.cells += cells;
            }
            _ => self.runs.push(Run {
                text: text.to_owned(),
                cells,
                style,
            }),
        }
    }

    /// Drop trailing blanks nobody can see. `text` is the plain text with trailing spaces
    /// removed; the runs spell the same text plus any visible trailing blanks.
    fn finish(mut self) -> Self {
        while let Some(run) = self.runs.last_mut() {
            if run.style.blank_is_invisible() {
                let kept = run.text.trim_end_matches(' ').len();
                run.cells -= run.text.len() - kept;
                run.text.truncate(kept);
            }
            if run.text.is_empty() {
                self.runs.pop();
            } else {
                break;
            }
        }
        for run in &self.runs {
            self.text.push_str(&run.text);
        }
        self.text.truncate(self.text.trim_end_matches(' ').len());
        self
    }

    fn value(&self, row: usize, wrapped: bool) -> Value {
        json!({
            "row": row,
            "text": self.text,
            "runs": self.runs.iter().map(Run::value).collect::<Vec<_>>(),
            "wrapped": wrapped,
            "redacted": false,
            "truncated": self.truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_of(rows: u16, columns: u16, bytes: &[u8]) -> Value {
        let mut emulator = Emulator::new(rows, columns, Title::default());
        emulator.feed(bytes);
        emulator.screen("fallback").value("terminal/demo", "1:now")
    }

    #[test]
    fn styled_cells_become_runs_that_spell_the_plain_text() {
        let screen = screen_of(
            3,
            20,
            b"plain \x1b[1;31mbold red\x1b[0m \x1b[2;3;4mdim\x1b[0m\r\n\x1b[7;38;2;1;2;3;48;5;200minv\x1b[0m",
        );
        let first = &screen["lines"][0];
        assert_eq!(first["text"], "plain bold red dim");
        assert_eq!(
            first["runs"],
            json!([
                { "text": "plain ", "cells": 6 },
                { "text": "bold red", "cells": 8, "fg": 1, "bold": true },
                { "text": " ", "cells": 1 },
                { "text": "dim", "cells": 3, "dim": true, "italic": true, "underline": true },
            ])
        );
        assert_eq!(
            screen["lines"][1]["runs"],
            json!([{ "text": "inv", "cells": 3, "fg": "#010203", "bg": 200, "inverse": true }])
        );
        assert_eq!(
            screen["lines"][2],
            json!({ "row": 2, "text": "", "runs": [], "wrapped": false, "redacted": false, "truncated": false })
        );
        assert_eq!(screen["rows"], 3);
        assert_eq!(screen["columns"], 20);
        assert_eq!(screen["title"], "fallback");
    }

    #[test]
    fn a_visible_trailing_blank_stays_in_the_runs_but_not_the_text() {
        let screen = screen_of(1, 10, b"ok\x1b[44m   \x1b[0m");
        let line = &screen["lines"][0];
        assert_eq!(line["text"], "ok");
        assert_eq!(
            line["runs"],
            json!([{ "text": "ok", "cells": 2 }, { "text": "   ", "cells": 3, "bg": 4 }])
        );
    }

    #[test]
    fn cursor_modes_and_title_follow_the_output() {
        let screen = screen_of(
            4,
            12,
            b"\x1b]2;build log\x07\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[>5u\x1b[6 q\x1b[3;5Hx",
        );
        assert_eq!(screen["title"], "build log");
        assert_eq!(
            screen["cursor"],
            json!({ "row": 2, "column": 5, "visible": true, "style": "bar", "blinking": false })
        );
        assert_eq!(
            screen["modes"],
            json!({
                "alternate_screen": true, "application_cursor": true, "application_keypad": false,
                "bracketed_paste": true, "focus_events": false, "mouse_tracking": "drag",
                "mouse_encoding": "sgr", "kitty_keyboard": 5
            })
        );
        let hidden = screen_of(2, 4, b"\x1b[?25l");
        assert_eq!(hidden["cursor"]["visible"], false);
    }

    #[test]
    fn vim_mouse_a_turns_tracking_on() {
        // vim -u NONE -c 'set mouse=a ttymouse=sgr' (9.1), as written to its terminal: SGR and
        // click tracking in one DECSET, then drag; set, reset and set again (issue #1013).
        let vim = b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[?12h\x1b[?12l\
            \x1b[?1006;1000h\x1b[?1002h\x1b[?1006;1000l\x1b[?1002l\
            \x1b[?1006;1000h\x1b[?1002h\x1b[?1006;1000l\x1b[?1002l\
            \x1b[?1006;1000h\x1b[?1002h\x1b[?25l\x1b[?25h";
        let screen = screen_of(4, 20, vim);
        assert_eq!(screen["modes"]["mouse_tracking"], "drag");
        assert_eq!(screen["modes"]["mouse_encoding"], "sgr");
    }

    #[test]
    fn wide_characters_hidden_text_and_long_lines_are_bounded() {
        let screen = screen_of(2, 8, "漢字ok\r\n\x1b[8msecret\x1b[0m".as_bytes());
        assert_eq!(screen["lines"][0]["text"], "漢字ok");
        assert_eq!(screen["lines"][0]["runs"][0]["cells"], 6);
        assert_eq!(screen["lines"][1]["text"], "");
        let long = "é".repeat(TERMINAL_MAX_LINE_BYTES);
        let screen = screen_of(1, 5_000, long.as_bytes());
        let line = &screen["lines"][0];
        assert_eq!(line["truncated"], true);
        let text = line["text"].as_str().unwrap();
        assert!(text.len() <= TERMINAL_MAX_LINE_BYTES);
        assert_eq!(line["runs"][0]["cells"], text.chars().count());
        let many = screen_of(250, 4, b"x");
        assert_eq!(many["lines"].as_array().unwrap().len(), TERMINAL_MAX_LINES);
        assert_eq!(many["truncated"], true);
    }

    #[test]
    fn soft_wraps_strikethrough_and_admitted_links_reach_the_runs() {
        let wrapped = screen_of(3, 6, b"abcdefgh\r\nnext");
        assert_eq!(wrapped["lines"][0]["wrapped"], false);
        assert_eq!(wrapped["lines"][1]["text"], "gh");
        assert_eq!(wrapped["lines"][1]["wrapped"], true);
        assert_eq!(wrapped["lines"][2]["wrapped"], false);

        let screen = screen_of(
            1,
            40,
            "漢\x1b[9mz\x1b[0m \x1b]8;;https://example.com\x1b\\web\x1b]8;;\x1b\\ \x1b]8;;javascript:alert(1)\x1b\\js\x1b]8;;\x1b\\ \x1b]8;id=f;file:///tmp/a.txt\x1b\\f\x1b]8;;\x1b\\"
                .as_bytes(),
        );
        assert_eq!(
            screen["lines"][0]["runs"],
            json!([
                { "text": "漢", "cells": 2 },
                { "text": "z", "cells": 1, "strikethrough": true },
                { "text": " ", "cells": 1 },
                { "text": "web", "cells": 3, "link": { "uri": "https://example.com/" } },
                { "text": " js ", "cells": 4 },
                { "text": "f", "cells": 1, "link": { "uri": "file:///tmp/a.txt" } },
            ])
        );
    }

    #[test]
    fn a_synchronized_update_holds_the_screen_until_it_ends_or_times_out() {
        let mut emulator = Emulator::new(2, 10, Title::default());
        emulator.feed(b"\x1b[?2026hhalf");
        let deadline = emulator.sync_deadline().expect("a sync update is pending");
        assert!(deadline > Instant::now());
        emulator.stop_sync();
        assert_eq!(emulator.sync_deadline(), None);
        emulator.feed(b" more");
        assert_eq!(emulator.sync_deadline(), None, "a shown update stays shown until it ends");
        emulator.feed(b"\x1b[?2026l\x1b[?2026h");
        assert!(emulator.sync_deadline().is_some(), "the next update holds again");
    }

    #[test]
    fn the_title_outlives_a_resync() {
        let title = Title::default();
        let mut first = Emulator::new(2, 10, title.clone());
        first.feed(b"\x1b]2;deploy\x07");
        let resynced = Emulator::new(2, 10, title);
        assert_eq!(
            resynced.screen("fallback").value("t", "i", 0)["title"],
            "deploy"
        );
    }

    #[test]
    fn equal_screens_share_a_revision() {
        let mut first = Emulator::new(2, 10, Title::default());
        first.feed(b"same");
        let mut second = Emulator::new(2, 10, Title::default());
        second.feed(b"sa\x1b[Kme");
        assert_eq!(
            first.screen("t").revision(),
            second.screen("t").revision()
        );
        second.feed(b"!");
        assert_ne!(
            first.screen("t").revision(),
            second.screen("t").revision()
        );
    }

    /// The emulator is `!Send`; a viewer on any runtime thread still gets screens, because the
    /// watcher runs on the emulation thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_viewer_on_any_thread_follows_a_pty_session() {
        use pty_core::protocol::{encode_data, encode_geometry, encode_packet};
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(root.path().join("rt.sock")).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 6];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(request.to_vec(), encode_peek(false, false));
            socket.write_all(&encode_geometry(2, 20)).await.unwrap();
            socket
                .write_all(&encode_packet(MessageType::Screen, b"\x1b]2;agent\x07hi"))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            socket.write_all(&encode_data(" there \x1b[9mok".as_bytes())).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let mut screens = subscribe(root.path(), "rt", "inc-1");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut after: Option<String> = None;
        loop {
            let screen = next_screen(&mut screens, after.as_deref(), deadline)
                .await
                .expect("the watcher stays up")
                .expect("a screen before the deadline");
            let value = screen.value("terminal/rt", "inc-1", 0);
            if value["lines"][0]["text"] == "hi there ok" {
                assert_eq!(value["title"], "agent");
                assert_eq!(value["lines"][0]["runs"][1], json!({ "text": "ok", "cells": 2, "strikethrough": true }));
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no live screen: {value}");
            after = Some(screen.revision().to_owned());
        }
        server.abort();
    }
}
