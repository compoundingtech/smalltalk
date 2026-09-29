//! Live terminal screens for client viewers.
//!
//! One read-only PTY connection per watched terminal incarnation feeds a terminal emulator.
//! Viewers share it and read whole screens: each screen replaces every earlier one, so a slow
//! viewer skips to the latest and nothing is replayed. An idle terminal produces no screens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
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

    /// The client `TerminalScreen` value for one viewer.
    pub(super) fn value(&self, terminal_id: &str, incarnation: &str, next_sequence: u64) -> Value {
        let mut value = serde_json::Map::with_capacity(self.body.len() + 5);
        value.insert("kind".into(), "terminal-screen".into());
        value.insert("terminal_id".into(), terminal_id.into());
        value.insert("runtime_incarnation".into(), incarnation.into());
        value.extend(self.body.clone());
        value.insert("revision".into(), self.revision.clone().into());
        value.insert("next_sequence".into(), next_sequence.into());
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
    watchers.insert(key.clone(), sender.clone());
    let fallback_title = runtime_id.to_owned();
    tokio::spawn(async move {
        let registration = Registration { key, sender };
        let end = watch_terminal(&registration.key, &fallback_title, &registration.sender).await;
        let sender = registration.sender.clone();
        drop(registration);
        sender.send_replace(ViewState::Ended(end));
    });
    receiver
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
    let title = TitleListener::default();
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
    title: &TitleListener,
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

#[derive(Clone, Default)]
struct TitleListener(Arc<Mutex<Option<String>>>);

impl TitleListener {
    fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        let title = match event {
            Event::Title(title) => Some(title),
            Event::ResetTitle => None,
            _ => return,
        };
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = title;
    }
}

struct Size {
    rows: usize,
    columns: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

pub(super) struct Emulator {
    term: Term<TitleListener>,
    parser: Processor,
    title: TitleListener,
}

impl Emulator {
    fn new(rows: u16, columns: u16, title: TitleListener) -> Self {
        let size = Size {
            rows: usize::from(rows.max(1)),
            columns: usize::from(columns.max(1)),
        };
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, title.clone()),
            parser: Processor::new(),
            title,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn stop_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }

    fn screen(&self, fallback_title: &str) -> Screen {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let columns = grid.columns();
        let mut truncated = false;
        let mut lines = Vec::with_capacity(rows.min(TERMINAL_MAX_LINES));
        for row in 0..rows {
            if row == TERMINAL_MAX_LINES {
                truncated = true;
                break;
            }
            let cells = &grid[Line(row as i32)];
            let mut runs = Vec::<Run>::new();
            for column in 0..columns {
                let cell = &cells[Column(column)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let style = Style::of(cell.fg, cell.bg, cell.flags);
                let hidden = cell.flags.contains(Flags::HIDDEN);
                let character = if hidden || cell.c == '\t' {
                    ' '
                } else {
                    cell.c
                };
                let run = match runs.last_mut() {
                    Some(run) if run.style == style => run,
                    _ => {
                        runs.push(Run {
                            text: String::new(),
                            style,
                        });
                        runs.last_mut().expect("a run was pushed")
                    }
                };
                run.text.push(character);
                if !hidden && let Some(marks) = cell.zerowidth() {
                    run.text.extend(marks);
                }
            }
            let line = ScreenLine::of(runs);
            lines.push(line.value(row));
        }
        let cursor_style = self.term.cursor_style();
        let mode = *self.term.mode();
        let point = grid.cursor.point;
        let (style, visible) = match cursor_style.shape {
            CursorShape::Block | CursorShape::HollowBlock => ("block", true),
            CursorShape::Underline => ("underline", true),
            CursorShape::Beam => ("bar", true),
            CursorShape::Hidden => ("block", false),
        };
        let mouse_tracking = if mode.contains(TermMode::MOUSE_MOTION) {
            "motion"
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            "drag"
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            "click"
        } else {
            "none"
        };
        let mouse_encoding = if mode.contains(TermMode::SGR_MOUSE) {
            "sgr"
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            "utf8"
        } else {
            "default"
        };
        let body = json!({
            "rows": rows,
            "columns": columns,
            "cursor": {
                "row": point.line.0.max(0),
                "column": point.column.0.min(columns.saturating_sub(1)),
                "visible": visible && mode.contains(TermMode::SHOW_CURSOR),
                "style": style,
                "blinking": cursor_style.blinking,
            },
            "title": self.title.get().unwrap_or_else(|| fallback_title.to_owned()),
            "modes": {
                "alternate_screen": mode.contains(TermMode::ALT_SCREEN),
                "application_cursor": mode.contains(TermMode::APP_CURSOR),
                "application_keypad": mode.contains(TermMode::APP_KEYPAD),
                "bracketed_paste": mode.contains(TermMode::BRACKETED_PASTE),
                "focus_events": mode.contains(TermMode::FOCUS_IN_OUT),
                "mouse_tracking": mouse_tracking,
                "mouse_encoding": mouse_encoding,
            },
            "lines": lines,
            "truncated": truncated,
        });
        let Value::Object(body) = body else {
            unreachable!("the screen body is an object")
        };
        let revision =
            hex::encode(&Sha256::digest(serde_json::to_vec(&body).unwrap_or_default())[..12]);
        Screen { body, revision }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paint {
    Palette(u8),
    Rgb(u8, u8, u8),
}

impl Paint {
    fn of(color: Color) -> Option<Self> {
        match color {
            Color::Indexed(index) => Some(Self::Palette(index)),
            Color::Spec(rgb) => Some(Self::Rgb(rgb.r, rgb.g, rgb.b)),
            Color::Named(named) => {
                let index = named as usize;
                if index < 16 {
                    Some(Self::Palette(index as u8))
                } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize)
                    .contains(&index)
                {
                    Some(Self::Palette((index - NamedColor::DimBlack as usize) as u8))
                } else {
                    None
                }
            }
        }
    }

    fn value(self) -> Value {
        match self {
            Self::Palette(index) => index.into(),
            Self::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}").into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Style {
    fg: Option<Paint>,
    bg: Option<Paint>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Style {
    fn of(fg: Color, bg: Color, flags: Flags) -> Self {
        Self {
            fg: Paint::of(fg),
            bg: Paint::of(bg),
            bold: flags.contains(Flags::BOLD),
            dim: flags.contains(Flags::DIM),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            inverse: flags.contains(Flags::INVERSE),
        }
    }

    /// A blank cell in this style looks like no cell at all.
    fn blank_is_invisible(&self) -> bool {
        self.bg.is_none() && !self.inverse && !self.underline
    }
}

struct Run {
    text: String,
    style: Style,
}

impl Run {
    fn value(&self) -> Value {
        let mut value = serde_json::Map::new();
        value.insert("text".into(), self.text.clone().into());
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
        ] {
            if set {
                value.insert(name.into(), true.into());
            }
        }
        Value::Object(value)
    }
}

struct ScreenLine {
    text: String,
    runs: Vec<Run>,
    truncated: bool,
}

impl ScreenLine {
    /// Drop trailing blanks nobody can see, then bound the line. `text` is the plain text with
    /// trailing spaces removed; the runs spell the same text plus any visible trailing blanks.
    fn of(mut runs: Vec<Run>) -> Self {
        while let Some(run) = runs.last_mut() {
            if run.style.blank_is_invisible() {
                let kept = run.text.trim_end_matches(' ').len();
                run.text.truncate(kept);
            }
            if run.text.is_empty() {
                runs.pop();
            } else {
                break;
            }
        }
        let mut truncated = false;
        let mut budget = TERMINAL_MAX_LINE_BYTES;
        let mut kept = 0;
        for run in &mut runs {
            if run.text.len() > budget {
                let mut end = budget;
                while !run.text.is_char_boundary(end) {
                    end -= 1;
                }
                run.text.truncate(end);
                truncated = true;
            }
            budget -= run.text.len();
            kept += 1;
            if truncated {
                break;
            }
        }
        runs.truncate(kept);
        runs.retain(|run| !run.text.is_empty());
        let text = runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
            .trim_end_matches(' ')
            .to_owned();
        Self {
            text,
            runs,
            truncated,
        }
    }

    fn value(&self, row: usize) -> Value {
        json!({
            "row": row,
            "text": self.text,
            "runs": self.runs.iter().map(Run::value).collect::<Vec<_>>(),
            "redacted": false,
            "truncated": self.truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_of(rows: u16, columns: u16, bytes: &[u8]) -> Value {
        let mut emulator = Emulator::new(rows, columns, TitleListener::default());
        emulator.feed(bytes);
        emulator
            .screen("fallback")
            .value("terminal/demo", "1:now", 7)
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
                { "text": "plain " },
                { "text": "bold red", "fg": 1, "bold": true },
                { "text": " " },
                { "text": "dim", "dim": true, "italic": true, "underline": true },
            ])
        );
        assert_eq!(
            screen["lines"][1]["runs"],
            json!([{ "text": "inv", "fg": "#010203", "bg": 200, "inverse": true }])
        );
        assert_eq!(
            screen["lines"][2],
            json!({ "row": 2, "text": "", "runs": [], "redacted": false, "truncated": false })
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
            json!([{ "text": "ok" }, { "text": "   ", "bg": 4 }])
        );
    }

    #[test]
    fn cursor_modes_and_title_follow_the_output() {
        let screen = screen_of(
            4,
            12,
            b"\x1b]2;build log\x07\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[6 q\x1b[3;5Hx",
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
                "mouse_encoding": "sgr"
            })
        );
        let hidden = screen_of(2, 4, b"\x1b[?25l");
        assert_eq!(hidden["cursor"]["visible"], false);
    }

    #[test]
    fn wide_characters_hidden_text_and_long_lines_are_bounded() {
        let screen = screen_of(2, 8, "漢字ok\r\n\x1b[8msecret\x1b[0m".as_bytes());
        assert_eq!(screen["lines"][0]["text"], "漢字ok");
        assert_eq!(screen["lines"][1]["text"], "");
        let long = "é".repeat(TERMINAL_MAX_LINE_BYTES);
        let screen = screen_of(1, 5_000, long.as_bytes());
        let line = &screen["lines"][0];
        assert_eq!(line["truncated"], true);
        assert!(line["text"].as_str().unwrap().len() <= TERMINAL_MAX_LINE_BYTES);
        let many = screen_of(250, 4, b"x");
        assert_eq!(many["lines"].as_array().unwrap().len(), TERMINAL_MAX_LINES);
        assert_eq!(many["truncated"], true);
    }

    #[test]
    fn equal_screens_share_a_revision() {
        let mut first = Emulator::new(2, 10, TitleListener::default());
        first.feed(b"same");
        let mut second = Emulator::new(2, 10, TitleListener::default());
        second.feed(b"sa\x1b[Kme");
        assert_eq!(first.screen("t").revision(), second.screen("t").revision());
        second.feed(b"!");
        assert_ne!(first.screen("t").revision(), second.screen("t").revision());
    }
}
