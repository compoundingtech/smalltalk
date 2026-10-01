//! An agent's terminal attached the way `st terminals attach` attaches it: the bytes of its PTY
//! session, through the gateway's raw stream to whichever host owns it, drawn by a terminal
//! emulator here. The PTY takes the pane's size, keys go to it as bytes, and the history that
//! scrolls off the top stays here to scroll back through.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pty_client::connection::{SessionConnection, SessionEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Lines kept above the screen to scroll back through.
const HISTORY: usize = 10_000;
/// How long the first screen may take to arrive.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// The PTY daemon answers terminal queries itself, so the emulator's answers go nowhere.
#[derive(Clone, Default)]
struct Quiet;

impl EventListener for Quiet {
    fn send_event(&self, _event: Event) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Size {
    rows: u16,
    columns: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        usize::from(self.rows)
    }
    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }
    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

struct Screen {
    term: Term<Quiet>,
    parser: Processor,
    /// Why the session is over, once it is.
    ended: Option<String>,
    /// When the stream dropped while the program still ran, so stui can attach again.
    dropped_at: Option<Instant>,
    /// Whether the first screen has arrived.
    attached: bool,
    /// When output last arrived, so stui can draw more often while it flows.
    output_at: Option<Instant>,
}

impl Screen {
    fn new(size: Size) -> Self {
        let config = Config {
            scrolling_history: HISTORY,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, Quiet),
            parser: Processor::new(),
            ended: None,
            dropped_at: None,
            attached: false,
            output_at: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.output_at = Some(Instant::now());
    }
}

enum Input {
    Bytes(Vec<u8>),
    Resize(Size),
}

/// A terminal attached through its PTY session. Dropping it detaches.
pub(crate) struct NativeTerminal {
    screen: Arc<Mutex<Screen>>,
    /// The current connection's; a reconnect replaces it.
    input: Mutex<mpsc::Sender<Input>>,
    /// The size last asked of the PTY.
    size: Mutex<Size>,
    /// The incarnation attached to; attaching again after a drop is only ever to this one.
    pub(crate) incarnation: String,
}

impl NativeTerminal {
    /// Attach over `stream`, a connection that speaks the PTY session protocol (the gateway's
    /// raw stream, or the session socket itself), asking for `rows` by `columns`. The attach
    /// runs on its own thread; until the first screen arrives the terminal says so.
    pub(crate) fn spawn(
        stream: UnixStream,
        name: &str,
        incarnation: String,
        rows: u16,
        columns: u16,
    ) -> Self {
        let size = Size {
            rows: rows.max(1),
            columns: columns.max(1),
        };
        let screen = Arc::new(Mutex::new(Screen::new(size)));
        let input = start(stream, name, size, &screen);
        Self {
            screen,
            input: Mutex::new(input),
            size: Mutex::new(size),
            incarnation,
        }
    }

    /// Attach again over a new stream after the last one dropped, into the same screen, so
    /// what scrolled back stays.
    pub(crate) fn reconnect(&self, stream: UnixStream, name: &str) {
        let size = *self.size.lock().unwrap_or_else(|error| error.into_inner());
        {
            let mut screen = self.lock();
            screen.ended = None;
            screen.dropped_at = None;
            screen.attached = false;
        }
        let input = start(stream, name, size, &self.screen);
        *self.input.lock().unwrap_or_else(|error| error.into_inner()) = input;
    }

    /// When the stream dropped while the program still ran.
    pub(crate) fn dropped(&self) -> Option<Instant> {
        self.lock().dropped_at
    }

    /// The reconnect failed for good: say why, and stop trying.
    pub(crate) fn give_up(&self, reason: String) {
        let mut screen = self.lock();
        screen.ended = Some(reason);
        screen.dropped_at = None;
    }

    fn send(&self, input: Input) {
        let _ = self
            .input
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send(input);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Screen> {
        self.screen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Send what was typed, at the bottom of the history.
    pub(crate) fn write(&self, bytes: Vec<u8>) {
        self.lock().term.scroll_display(Scroll::Bottom);
        self.send(Input::Bytes(bytes));
    }

    /// A paste, bracketed when the program asked for that.
    pub(crate) fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        let mut bytes = Vec::new();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.write(bytes);
    }

    /// Ask the PTY for the pane's size when it changed.
    pub(crate) fn fit(&self, rows: u16, columns: u16) {
        let size = Size {
            rows: rows.max(1),
            columns: columns.max(1),
        };
        let mut current = self.size.lock().unwrap_or_else(|error| error.into_inner());
        if *current != size {
            *current = size;
            self.send(Input::Resize(size));
        }
    }

    pub(crate) fn mode(&self) -> TermMode {
        *self.lock().term.mode()
    }

    /// Scroll back (positive) or forward through the history, or hand a program that reads the
    /// mouse its wheel instead.
    pub(crate) fn wheel(&self, lines: i32) {
        let mode = self.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && mode.contains(TermMode::ALT_SCREEN) {
            let button = if lines > 0 { 64 } else { 65 };
            let report = if mode.contains(TermMode::SGR_MOUSE) {
                format!("\x1b[<{button};1;1M")
            } else {
                format!("\x1b[M{}!!", char::from(32 + button as u8))
            };
            self.send(Input::Bytes(
                report.repeat(lines.unsigned_abs() as usize).into_bytes(),
            ));
        } else if mode.contains(TermMode::ALT_SCREEN) {
            // A full-screen program without the mouse scrolls with its arrows, as terminals do.
            let arrow = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => "\x1bOA",
                (true, false) => "\x1b[A",
                (false, true) => "\x1bOB",
                (false, false) => "\x1b[B",
            };
            self.send(Input::Bytes(
                arrow.repeat(lines.unsigned_abs() as usize).into_bytes(),
            ));
        } else {
            self.lock().term.scroll_display(Scroll::Delta(lines));
        }
    }

    /// Scroll the history by a page: up when `up`.
    pub(crate) fn page(&self, up: bool) {
        let mut screen = self.lock();
        let page = screen.term.screen_lines() as i32 - 1;
        screen
            .term
            .scroll_display(Scroll::Delta(if up { page } else { -page }));
    }

    /// Lines scrolled back from the bottom.
    pub(crate) fn scrolled(&self) -> usize {
        self.lock().term.grid().display_offset()
    }

    pub(crate) fn ended(&self) -> Option<String> {
        self.lock().ended.clone()
    }

    pub(crate) fn attached(&self) -> bool {
        self.lock().attached
    }

    /// Whether output arrived in the last moment, so stui draws more often.
    pub(crate) fn flowing(&self) -> bool {
        self.lock()
            .output_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(250))
    }

    /// Draw the screen, or the history scrolled to, into `area`.
    pub(crate) fn draw(&self, buf: &mut Buffer, area: Rect) {
        let screen = self.lock();
        let grid = screen.term.grid();
        let offset = grid.display_offset() as i32;
        let rows = grid.screen_lines().min(usize::from(area.height));
        let columns = grid.columns().min(usize::from(area.width));
        for row in 0..rows {
            let cells = &grid[GridLine(row as i32 - offset)];
            let mut spans: Vec<Span<'static>> = Vec::new();
            for column in 0..columns {
                let cell = &cells[Column(column)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let style = cell_style(cell.fg, cell.bg, cell.flags);
                let mut text = if cell.flags.contains(Flags::HIDDEN) || cell.c == '\t' {
                    ' '.to_string()
                } else {
                    cell.c.to_string()
                };
                if let Some(marks) = cell.zerowidth() {
                    text.extend(marks);
                }
                match spans.last_mut() {
                    Some(span) if span.style == style => span.content.to_mut().push_str(&text),
                    _ => spans.push(Span::styled(text, style)),
                }
            }
            buf.set_line(area.x, area.y + row as u16, &Line::from(spans), area.width);
        }
        // The cursor, when the bottom is shown and the program shows it.
        let point = grid.cursor.point;
        let shape = screen.term.cursor_style().shape;
        if offset == 0
            && screen.ended.is_none()
            && screen.term.mode().contains(TermMode::SHOW_CURSOR)
            && shape != CursorShape::Hidden
            && let (Ok(row), column) = (u16::try_from(point.line.0), point.column.0)
            && row < area.height
            && column < usize::from(area.width)
            && let Some(cell) = buf.cell_mut((area.x + column as u16, area.y + row))
        {
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
        }
    }
}

impl Drop for NativeTerminal {
    fn drop(&mut self) {
        // The thread sees the channel close and detaches.
    }
}

/// Start the attach thread for `stream`; the sender takes what is typed and resizes.
fn start(
    stream: UnixStream,
    name: &str,
    size: Size,
    screen: &Arc<Mutex<Screen>>,
) -> mpsc::Sender<Input> {
    let (input, inputs) = mpsc::channel();
    let shared = screen.clone();
    let name = name.to_owned();
    std::thread::Builder::new()
        .name(format!("pty {name}"))
        .spawn(move || run(stream, &name, size, &shared, &inputs))
        .ok();
    input
}

/// The stream is gone but the program may still run: say so, and let stui attach again.
fn dropped(screen: &mut Screen, reason: String) {
    screen.ended = Some(format!("{reason}; attaching again…"));
    screen.dropped_at = Some(Instant::now());
}

/// The attach thread: ATTACH at `size`, then feed what the session sends into the screen and
/// send it what is typed, until stui lets go or the session ends.
fn run(
    stream: UnixStream,
    name: &str,
    size: Size,
    screen: &Mutex<Screen>,
    inputs: &mpsc::Receiver<Input>,
) {
    let lock = || screen.lock().unwrap_or_else(|error| error.into_inner());
    if let Err(error) = stream.set_nonblocking(false) {
        lock().ended = Some(error.to_string());
        return;
    }
    let mut connection = match SessionConnection::attach_over(
        stream,
        name,
        size.rows,
        size.columns,
        Some(ATTACH_TIMEOUT),
    ) {
        Ok(connection) => connection,
        Err(error) => {
            dropped(&mut lock(), error.to_string());
            return;
        }
    };
    {
        let mut screen = lock();
        let replay = connection.screen().to_vec();
        screen.feed(&replay);
        screen.attached = true;
    }
    loop {
        loop {
            match inputs.try_recv() {
                Ok(Input::Bytes(bytes)) => connection.write(&bytes),
                Ok(Input::Resize(size)) => {
                    connection.resize(size.rows, size.columns);
                    lock().term.resize(size);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    connection.disconnect();
                    return;
                }
            }
        }
        match connection.next_event(Some(Duration::from_millis(10))) {
            Ok(Some(SessionEvent::Data(bytes))) => lock().feed(&bytes),
            // A replay after a reconnect: the screen as it is now.
            Ok(Some(SessionEvent::Screen(bytes))) => {
                let mut screen = lock();
                let size = Size {
                    rows: screen.term.screen_lines() as u16,
                    columns: screen.term.columns() as u16,
                };
                *screen = Screen::new(size);
                screen.attached = true;
                screen.feed(&bytes);
            }
            // The size every attached client shares, before the output drawn at it.
            Ok(Some(SessionEvent::Geometry { rows, cols })) => {
                lock().term.resize(Size {
                    rows: rows.max(1),
                    columns: cols.max(1),
                });
            }
            Ok(Some(SessionEvent::Exit(code))) => {
                lock().ended = Some(format!("the program exited ({code})"));
                return;
            }
            Ok(Some(SessionEvent::Closed)) => {
                dropped(&mut lock(), "the connection closed".into());
                return;
            }
            Ok(None) => {}
            Err(error) => {
                dropped(&mut lock(), error.to_string());
                return;
            }
        }
        let mut screen = lock();
        if screen
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            let Screen { parser, term, .. } = &mut *screen;
            parser.stop_sync(term);
        }
    }
}

fn paint(color: AnsiColor) -> Option<Color> {
    match color {
        AnsiColor::Indexed(index) => Some(Color::Indexed(index)),
        AnsiColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        AnsiColor::Named(named) => {
            let index = named as usize;
            if index < 16 {
                Some(Color::Indexed(index as u8))
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize)
                .contains(&index)
            {
                Some(Color::Indexed(
                    (index - NamedColor::DimBlack as usize) as u8,
                ))
            } else {
                None
            }
        }
    }
}

fn cell_style(fg: AnsiColor, bg: AnsiColor, flags: Flags) -> Style {
    let mut style = Style::default();
    if let Some(color) = paint(fg) {
        style = style.fg(color);
    }
    if let Some(color) = paint(bg) {
        style = style.bg(color);
    }
    for (flag, modifier) in [
        (Flags::BOLD, Modifier::BOLD),
        (Flags::DIM, Modifier::DIM),
        (Flags::ITALIC, Modifier::ITALIC),
        (Flags::INVERSE, Modifier::REVERSED),
        (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
    ] {
        if flags.contains(flag) {
            style = style.add_modifier(modifier);
        }
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// What a terminal reports to a program that asked for the mouse, for `mouse` at a cell of its
/// screen (zero-based). `None` when the program did not ask, or for what it did not ask for.
pub(crate) fn mouse_bytes(
    mouse: crossterm::event::MouseEvent,
    column: u16,
    row: u16,
    mode: TermMode,
) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind};
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let button = |button: MouseButton| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match mouse.kind {
        MouseEventKind::Down(which) => (button(which), false),
        MouseEventKind::Up(which) => (button(which), true),
        MouseEventKind::Drag(which)
            if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) =>
        {
            (button(which) + 32, false)
        }
        _ => return None,
    };
    let modifiers = u8::from(mouse.modifiers.contains(KeyModifiers::SHIFT)) * 4
        + u8::from(mouse.modifiers.contains(KeyModifiers::ALT)) * 8
        + u8::from(mouse.modifiers.contains(KeyModifiers::CONTROL)) * 16;
    let code = code + modifiers;
    Some(if mode.contains(TermMode::SGR_MOUSE) {
        format!(
            "\x1b[<{code};{};{}{}",
            column + 1,
            row + 1,
            if release { 'm' } else { 'M' }
        )
        .into_bytes()
    } else {
        // The old encoding: a release is button 3, and a cell past 223 cannot be said.
        let code = if release { 3 + modifiers } else { code };
        let cell = |value: u16| u8::try_from(value + 33).unwrap_or(255);
        vec![0x1b, b'[', b'M', 32 + code, cell(column), cell(row)]
    })
}

/// The bytes a terminal sends for `key`, following the program's cursor-key mode.
pub(crate) fn key_bytes(key: KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 + shift + 2·alt + 4·control.
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    let application = mode.contains(TermMode::APP_CURSOR);
    let cursor = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if application {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |number: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{number};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        }
    };
    let bytes = match key.code {
        KeyCode::Char(character) => {
            let mut bytes = Vec::new();
            if alt {
                bytes.push(0x1b);
            }
            if control {
                let upper = character.to_ascii_uppercase();
                match upper {
                    '@'..='_' => bytes.push(upper as u8 & 0x1f),
                    ' ' | '2' => bytes.push(0),
                    '3'..='7' => bytes.push(0x1b + (upper as u8 - b'3')),
                    '8' | '?' => bytes.push(0x7f),
                    _ => bytes.extend_from_slice(character.to_string().as_bytes()),
                }
            } else {
                bytes.extend_from_slice(character.to_string().as_bytes());
            }
            bytes
        }
        KeyCode::Enter => {
            if alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if alt {
                b"\x1b\x7f".to_vec()
            } else if control {
                b"\x08".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(number @ 1..=4) => {
            let letter = (b'P' + number - 1) as char;
            if modifier > 1 {
                format!("\x1b[1;{modifier}{letter}").into_bytes()
            } else {
                format!("\x1bO{letter}").into_bytes()
            }
        }
        KeyCode::F(number @ 5..=12) => {
            tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)])
        }
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn clicks_reach_a_program_that_asked_for_the_mouse_and_no_other() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let event = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let down = event(MouseEventKind::Down(MouseButton::Left));
        let up = event(MouseEventKind::Up(MouseButton::Left));
        assert_eq!(mouse_bytes(down, 4, 2, TermMode::empty()), None);
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(mouse_bytes(down, 4, 2, sgr).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(mouse_bytes(up, 4, 2, sgr).unwrap(), b"\x1b[<0;5;3m");
        let drag = event(MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(mouse_bytes(drag, 4, 2, sgr), None, "clicks only");
        assert_eq!(
            mouse_bytes(drag, 4, 2, sgr | TermMode::MOUSE_DRAG).unwrap(),
            b"\x1b[<32;5;3M"
        );
        assert_eq!(
            mouse_bytes(down, 4, 2, TermMode::MOUSE_REPORT_CLICK).unwrap(),
            [0x1b, b'[', b'M', 32, 37, 35]
        );
    }

    #[test]
    fn keys_are_the_bytes_a_terminal_sends() {
        let normal = TermMode::empty();
        let app = TermMode::APP_CURSOR;
        let bytes = |code, modifiers, mode| key_bytes(key(code, modifiers), mode).unwrap();
        assert_eq!(bytes(KeyCode::Char('x'), KeyModifiers::NONE, normal), b"x");
        assert_eq!(
            bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, normal),
            b"\x03"
        );
        assert_eq!(
            bytes(KeyCode::Char('['), KeyModifiers::CONTROL, normal),
            b"\x1b"
        );
        assert_eq!(
            bytes(KeyCode::Char('b'), KeyModifiers::ALT, normal),
            b"\x1bb"
        );
        assert_eq!(
            bytes(KeyCode::Char('é'), KeyModifiers::NONE, normal),
            "é".as_bytes()
        );
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE, normal), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE, app), b"\x1bOA");
        assert_eq!(
            bytes(KeyCode::Left, KeyModifiers::CONTROL, app),
            b"\x1b[1;5D"
        );
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::NONE, normal), b"\r");
        assert_eq!(
            bytes(KeyCode::Backspace, KeyModifiers::NONE, normal),
            b"\x7f"
        );
        assert_eq!(
            bytes(KeyCode::PageUp, KeyModifiers::NONE, normal),
            b"\x1b[5~"
        );
        assert_eq!(bytes(KeyCode::F(1), KeyModifiers::NONE, normal), b"\x1bOP");
        assert_eq!(
            bytes(KeyCode::F(5), KeyModifiers::NONE, normal),
            b"\x1b[15~"
        );
        assert_eq!(
            bytes(KeyCode::BackTab, KeyModifiers::SHIFT, normal),
            b"\x1b[Z"
        );
    }

    #[test]
    fn a_dropped_stream_attaches_again_into_the_same_screen() {
        use pty_core::protocol::{MessageType, encode_packet};
        use std::io::{Read as _, Write as _};
        let wait = |done: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !done() {
                assert!(start.elapsed() < Duration::from_secs(5), "timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let shown = |terminal: &NativeTerminal| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 30, 6));
            let area = buf.area;
            terminal.draw(&mut buf, area);
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let (stui, mut daemon) = UnixStream::pair().unwrap();
        let terminal = NativeTerminal::spawn(stui, "test", "one".into(), 6, 30);
        let mut bytes = [0_u8; 64];
        let _ = daemon.read(&mut bytes).unwrap();
        daemon
            .write_all(&encode_packet(MessageType::Screen, b""))
            .unwrap();
        for line in 0..10 {
            daemon
                .write_all(&encode_packet(
                    MessageType::Data,
                    format!("old {line}\r\n").as_bytes(),
                ))
                .unwrap();
        }
        wait(&|| shown(&terminal).contains("old 9"));
        // The stream drops; the program did not exit.
        drop(daemon);
        wait(&|| terminal.dropped().is_some());
        assert!(terminal.ended().unwrap().contains("attaching again"));
        let (stui, mut daemon) = UnixStream::pair().unwrap();
        terminal.reconnect(stui, "test");
        let _ = daemon.read(&mut bytes).unwrap();
        daemon
            .write_all(&encode_packet(MessageType::Screen, b"new\r\n"))
            .unwrap();
        wait(&|| shown(&terminal).contains("new") && terminal.ended().is_none());
        assert!(terminal.dropped().is_none());
        // What scrolled off before the drop is still there to scroll back to.
        terminal.wheel(20);
        assert!(shown(&terminal).contains("old 0"), "{}", shown(&terminal));
    }

    /// A PTY session daemon, played by the test: answer ATTACH with a screen, then data.
    #[test]
    fn a_session_draws_into_the_pane_takes_its_size_and_keys_and_ends() {
        use pty_core::protocol::{MessageType, PacketReader, encode_packet};
        use std::io::{Read as _, Write as _};
        let (stui, mut daemon) = UnixStream::pair().unwrap();
        let terminal = NativeTerminal::spawn(stui, "test", "incarnation".into(), 4, 20);
        let mut reader = PacketReader::new();
        let mut packets = Vec::new();
        let mut next = |daemon: &mut UnixStream, packets: &mut Vec<_>| {
            while packets.is_empty() {
                let mut bytes = [0_u8; 256];
                let count = daemon.read(&mut bytes).unwrap();
                packets.extend(reader.feed(&bytes[..count]).unwrap());
            }
            packets.remove(0)
        };
        let attach = next(&mut daemon, &mut packets);
        assert_eq!(attach.type_, MessageType::Attach);
        daemon
            .write_all(&encode_packet(MessageType::Screen, b"hello\r\n"))
            .unwrap();
        daemon
            .write_all(&encode_packet(MessageType::Data, b"\x1b[1mworld"))
            .unwrap();
        let wait = |done: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !done() {
                assert!(start.elapsed() < Duration::from_secs(5), "timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&|| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 4));
            {
                let area = buf.area;
                terminal.draw(&mut buf, area);
            }
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("world")
        });
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 4));
        {
            let area = buf.area;
            terminal.draw(&mut buf, area);
        }
        assert_eq!(buf[(0, 1)].symbol(), "w");
        assert!(buf[(0, 1)].modifier.contains(Modifier::BOLD));
        // Keys go as bytes; a new pane size goes as a resize.
        terminal.write(b"ls\r".to_vec());
        let data = next(&mut daemon, &mut packets);
        assert_eq!(
            (data.type_, data.payload.as_slice()),
            (MessageType::Data, &b"ls\r"[..])
        );
        terminal.fit(10, 40);
        assert_eq!(next(&mut daemon, &mut packets).type_, MessageType::Resize);
        terminal.fit(10, 40);
        // Output past the screen stays to scroll back through.
        for line in 0..30 {
            daemon
                .write_all(&encode_packet(
                    MessageType::Data,
                    format!("line {line}\r\n").as_bytes(),
                ))
                .unwrap();
        }
        wait(&|| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
            {
                let area = buf.area;
                terminal.draw(&mut buf, area);
            }
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("line 29")
        });
        terminal.wheel(5);
        assert_eq!(terminal.scrolled(), 5);
        terminal.write(b"x".to_vec());
        assert_eq!(terminal.scrolled(), 0, "typing returns to the bottom");
        daemon
            .write_all(&encode_packet(MessageType::Exit, &0_i32.to_be_bytes()))
            .unwrap();
        wait(&|| terminal.ended().is_some());
        // Dropping it detaches.
        drop(terminal);
    }
}
