//! An agent's terminal attached the way `st terminals attach` attaches it: the bytes of its PTY
//! session, through the gateway's raw stream to whichever host owns it, drawn by a terminal
//! emulator here. The PTY takes the pane's size, keys go to it as bytes, and the history that
//! scrolls off the top stays here to scroll back through.

use super::theme;
#[path = "pty_graphics.rs"]
mod graphics;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line as GridLine, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, CursorStyle, NamedColor, Processor,
};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
};
use pty_client::connection::{SessionConnection, SessionEvent};
use pty_terminal::{
    Key as ProtocolKey, KeyAction, Mods, MouseAction, MouseButton as ProtocolButton, TerminalActor,
};
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

/// What the program asked of its terminal that stui acts on: a copy for the person's clipboard
/// (OSC 52), the window title, the bell. The PTY daemon answers terminal queries itself, so
/// the emulator's own answers go nowhere.
#[derive(Clone, Default)]
struct Requests(Arc<Mutex<Asked>>);

/// What the program asked since stui last looked.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Asked {
    /// Text the program copied, for the person's clipboard.
    pub(crate) copied: Option<String>,
    /// The title the program gave its window; `None` when it never did or reset it.
    pub(crate) title: Option<String>,
    pub(crate) bell: bool,
    clipboard_reads: Vec<Vec<u8>>,
}

impl EventListener for Requests {
    fn send_event(&self, event: Event) {
        let mut asked = self.0.lock().unwrap_or_else(|error| error.into_inner());
        match event {
            // Only a copy: a program reading the person's clipboard is refused, as by default
            // in most terminals.
            Event::ClipboardStore(_, text) => asked.copied = Some(text),
            Event::ClipboardLoad(_, format) => asked.clipboard_reads.push(format("").into_bytes()),
            Event::Title(title) => asked.title = Some(title),
            Event::ResetTitle => asked.title = None,
            Event::Bell => asked.bell = true,
            _ => {}
        }
    }
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
    term: Term<Requests>,
    parser: Processor,
    /// Why the session is over, once it is.
    ended: Option<String>,
    /// When the stream dropped while the program still ran, so stui can attach again.
    dropped_at: Option<Instant>,
    /// Whether the first screen has arrived.
    attached: bool,
    /// When output last arrived, so stui can draw more often while it flows.
    output_at: Option<Instant>,
    /// What the program asks of the terminal, shared with the emulator that hears it.
    requests: Requests,
    /// Where the selection being dragged began.
    anchor: Option<Point>,
    graphics: graphics::Frame,
}

impl Screen {
    fn new(size: Size, requests: Requests) -> Self {
        let config = Config {
            scrolling_history: HISTORY,
            kitty_keyboard: true,
            // Reads are handled with an empty response, never clipboard access.
            osc52: alacritty_terminal::term::Osc52::CopyPaste,
            // A shape no program can ask for, so stui can tell when none did.
            default_cursor_style: UNASKED,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, requests.clone()),
            parser: Processor::new(),
            ended: None,
            dropped_at: None,
            attached: false,
            output_at: None,
            requests,
            anchor: None,
            graphics: graphics::Frame::default(),
        }
    }

    /// The grid point at a cell of the pane, counting the history scrolled to; `None` past the
    /// screen.
    fn point(&self, column: u16, row: u16) -> Option<Point> {
        let (rows, columns) = (self.term.screen_lines(), self.term.columns());
        (usize::from(row) < rows && usize::from(column) < columns).then(|| {
            viewport_to_point(
                self.term.grid().display_offset(),
                Point::new(usize::from(row), Column(usize::from(column))),
            )
        })
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.output_at = Some(Instant::now());
    }
}

enum Input {
    Bytes(Vec<u8>),
    Resize(Size),
    Key(pty_terminal::KeyEvent, bool),
    Mouse(pty_terminal::MouseEvent, usize),
    Focus(bool),
    Reset,
    Cell(pty_terminal::CellSize),
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
    cell: Mutex<pty_terminal::CellSize>,
    painter: Mutex<graphics::Painter>,
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
        let screen = Arc::new(Mutex::new(Screen::new(size, Requests::default())));
        let input = start(stream, name, size, &screen);
        Self {
            screen,
            input: Mutex::new(input),
            size: Mutex::new(size),
            incarnation,
            cell: Mutex::new(pty_terminal::CellSize::default()),
            painter: Mutex::new(graphics::Painter::default()),
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
        {
            let mut screen = self.lock();
            screen.term.scroll_display(Scroll::Bottom);
            // Typing lets go of a selection, as in a terminal.
            screen.term.selection = None;
        }
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

    pub(crate) fn draw_graphics(
        &self,
        buf: &mut Buffer,
        area: Rect,
        picker: &ratatui_image::picker::Picker,
    ) {
        use ratatui_image::picker::ProtocolType;
        let font = picker.font_size();
        let cell = pty_terminal::CellSize {
            width: u32::from(font.width),
            height: u32::from(font.height),
        };
        let mut current = self.cell.lock().unwrap_or_else(|e| e.into_inner());
        if picker.protocol_type() == ProtocolType::Kitty && *current != cell {
            *current = cell;
            self.send(Input::Cell(cell));
        }
        drop(current);
        let frame = self.lock().graphics.clone();
        self.painter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .draw(&frame, buf, area, picker);
    }

    pub(crate) fn hide_graphics(&self) {
        *self.painter.lock().unwrap_or_else(|e| e.into_inner()) = graphics::Painter::default();
    }

    pub(crate) fn mode(&self) -> TermMode {
        *self.lock().term.mode()
    }

    /// Send a semantic key; the session's terminal state chooses its protocol encoding.
    pub(crate) fn key(&self, key: KeyEvent) {
        if let Some(key) = protocol_key(key) {
            self.lock().term.scroll_display(Scroll::Bottom);
            self.lock().term.selection = None;
            self.send(Input::Key(key, self.mode().contains(TermMode::APP_KEYPAD)));
        }
    }

    pub(crate) fn mouse(&self, mouse: MouseEvent, column: u16, row: u16) {
        if let Some(mouse) = protocol_mouse(mouse, column, row) {
            self.send(Input::Mouse(mouse, 1));
        }
    }

    pub(crate) fn focus(&self, gained: bool) {
        self.send(Input::Focus(gained));
    }

    /// Reset the tab's input modes without sending reset bytes as shell input.
    pub(crate) fn reset_modes(&self) {
        let mut screen = self.lock();
        if screen.term.mode().contains(TermMode::ALT_SCREEN) {
            screen.feed(b"\x1b[<99u\x1b[?1049l");
        }
        screen.feed(pty_terminal::actor::RESET_INPUT_MODES);
        screen.graphics = graphics::Frame::default();
        drop(screen);
        self.hide_graphics();
        self.send(Input::Reset);
    }

    /// Scroll history, or send a wheel report at the actual pane-relative cell.
    pub(crate) fn wheel(&self, lines: i32, mouse: MouseEvent, column: u16, row: u16, local: bool) {
        let mode = self.mode();
        if !local && mode.intersects(TermMode::MOUSE_MODE) {
            if let Some(mouse) = protocol_mouse(mouse, column, row) {
                self.send(Input::Mouse(mouse, lines.unsigned_abs() as usize));
            }
        } else if matches!(
            mouse.kind,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight
        ) {
            // There is no horizontal history axis or DEC alternate-scroll equivalent.
        } else if !local && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
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

    /// Begin a selection at a cell of the pane (zero-based), as a press does: a second press in
    /// a row selects the word there, a third the line.
    pub(crate) fn select_from(&self, column: u16, row: u16, clicks: u8) {
        let mut screen = self.lock();
        let Some(point) = screen.point(column, row) else {
            return;
        };
        let kind = match clicks {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        screen.anchor = Some(point);
        screen.term.selection = Some(Selection::new(kind, point, Side::Left));
    }

    /// Carry the selection to where the mouse is now, at a cell of the pane that may be past its
    /// edges: above or below scrolls the history a line, as terminals do.
    pub(crate) fn select_to(&self, column: i32, row: i32) {
        let mut screen = self.lock();
        let rows = screen.term.screen_lines() as i32;
        let columns = screen.term.columns() as i32;
        if row < 0 {
            screen.term.scroll_display(Scroll::Delta(1));
        } else if row >= rows {
            screen.term.scroll_display(Scroll::Delta(-1));
        }
        let Some(point) = screen.point(
            column.clamp(0, columns - 1) as u16,
            row.clamp(0, rows - 1) as u16,
        ) else {
            return;
        };
        // The cell where it began and the cell under the mouse are both in, whichever way the
        // drag went.
        let (Some(anchor), Some(kind)) = (
            screen.anchor,
            screen.term.selection.as_ref().map(|selection| selection.ty),
        ) else {
            return;
        };
        let (from, to) = if point < anchor {
            (Side::Right, Side::Left)
        } else {
            (Side::Left, Side::Right)
        };
        let mut selection = Selection::new(kind, anchor, from);
        selection.update(point, to);
        screen.term.selection = Some(selection);
    }

    /// Copy a hyperlink destination without asking the outer terminal to own pane coordinates.
    pub(crate) fn hyperlink(&self, column: u16, row: u16) -> Option<String> {
        let screen = self.lock();
        let point = screen.point(column, row)?;
        screen.term.grid()[point]
            .hyperlink()
            .map(|link| link.uri().to_owned())
    }

    /// The selected text as it would paste: a line the program's output wrapped stays one line.
    pub(crate) fn selected(&self) -> Option<String> {
        self.lock()
            .term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// What the program asked of its terminal since the last look. A copy and a bell are taken;
    /// the title stays until the program changes it.
    pub(crate) fn asked(&self) -> Asked {
        let requests = self.lock().requests.clone();
        let mut asked = requests.0.lock().unwrap_or_else(|error| error.into_inner());
        Asked {
            copied: asked.copied.take(),
            title: asked.title.clone(),
            bell: std::mem::take(&mut asked.bell),
            clipboard_reads: Vec::new(),
        }
    }

    /// The title the program gave its window, when it gave one.
    pub(crate) fn title(&self) -> Option<String> {
        let requests = self.lock().requests.clone();
        let asked = requests.0.lock().unwrap_or_else(|error| error.into_inner());
        asked.title.clone().filter(|title| !title.trim().is_empty())
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

    #[cfg(test)]
    pub(super) fn grid_size(&self) -> (usize, usize) {
        let screen = self.lock();
        (screen.term.screen_lines(), screen.term.columns())
    }

    /// Whether output arrived in the last moment, so stui draws more often.
    pub(crate) fn flowing(&self) -> bool {
        self.lock()
            .output_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(250))
    }

    /// Draw the screen, or the history scrolled to, into `area`. With `real_cursor` the cursor
    /// is not drawn but returned, for the person's own terminal cursor to show it.
    pub(crate) fn draw(&self, buf: &mut Buffer, area: Rect, real_cursor: bool) -> Option<Cursor> {
        let screen = self.lock();
        let grid = screen.term.grid();
        let offset = grid.display_offset() as i32;
        let rows = grid.screen_lines().min(usize::from(area.height));
        let columns = grid.columns().min(usize::from(area.width));
        let selection = screen
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&screen.term));
        for row in 0..rows {
            let line = GridLine(row as i32 - offset);
            let cells = &grid[line];
            let mut spans: Vec<Span<'static>> = Vec::new();
            for column in 0..columns {
                let cell = &cells[Column(column)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let mut style = cell_style(cell.fg, cell.bg, cell.flags);
                if let Some(color) = cell.underline_color().and_then(paint) {
                    style = style.underline_color(color);
                }
                if selection.is_some_and(|range| range.contains(Point::new(line, Column(column)))) {
                    style = style.bg(theme::SELECTION_BG);
                }
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
        // The cursor, when the bottom is shown and the program shows it: the person's own
        // terminal cursor where it has the keys, so it keeps the shape the program asked for
        // (vim's bar while inserting), blinks, and places an input method's text; otherwise
        // an inverted cell.
        let point = grid.cursor.point;
        let style = screen.term.cursor_style();
        if offset == 0
            && screen.ended.is_none()
            && screen.term.mode().contains(TermMode::SHOW_CURSOR)
            && style.shape != CursorShape::Hidden
            && let (Ok(row), column) = (u16::try_from(point.line.0), point.column.0)
            && row < area.height
            && column < usize::from(area.width)
        {
            let (x, y) = (area.x + column as u16, area.y + row);
            if real_cursor {
                return Some(Cursor {
                    x,
                    y,
                    style: cursor_style(style.shape, style.blinking),
                });
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            }
        }
        None
    }
}

/// The cursor style until a program asks for one: the person's own shape is kept then.
const UNASKED: CursorStyle = CursorStyle {
    shape: CursorShape::HollowBlock,
    blinking: false,
};

/// Where the terminal's own cursor goes on the screen, and its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) style: SetCursorStyle,
}

/// The person's terminal cursor for the shape a program asked of its own.
fn cursor_style(shape: CursorShape, blinking: bool) -> SetCursorStyle {
    match (shape, blinking) {
        (CursorShape::HollowBlock, _) => SetCursorStyle::DefaultUserShape,
        (CursorShape::Beam, true) => SetCursorStyle::BlinkingBar,
        (CursorShape::Beam, false) => SetCursorStyle::SteadyBar,
        (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
        (_, true) => SetCursorStyle::BlinkingBlock,
        (_, false) => SetCursorStyle::SteadyBlock,
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
    let mut control = stream.try_clone();
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
    let mut protocol = TerminalActor::new(size.rows, size.columns, HISTORY);
    protocol.enable_graphics(pty_terminal::GraphicsOptions::default());
    {
        let mut screen = lock();
        let replay = connection.screen().to_vec();
        protocol.write(&replay);
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
                    protocol.resize(size.columns, size.rows);
                }
                Ok(Input::Key(key, keypad)) => {
                    connection.write(&encode_key(&protocol, &key, keypad))
                }
                Ok(Input::Mouse(mouse, count)) => {
                    if let Some(bytes) = protocol.encode_mouse(&mouse) {
                        connection.write(&bytes.repeat(count));
                    }
                }
                Ok(Input::Focus(gained)) => {
                    if let Some(bytes) = protocol.encode_focus(gained) {
                        connection.write(&bytes);
                    }
                }
                Ok(Input::Reset) => {
                    protocol.reset_input_modes();
                    connection.reset_input_modes();
                }
                Ok(Input::Cell(cell)) => {
                    use std::io::Write;
                    protocol.set_cell_size(cell);
                    if let Ok(control) = &mut control {
                        let _ = control.write_all(&pty_core::protocol::encode_resize_with_cell(
                            protocol.rows(),
                            protocol.cols(),
                            cell.width as u16,
                            cell.height as u16,
                        ));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    connection.disconnect();
                    return;
                }
            }
        }
        match connection.next_event(Some(Duration::from_millis(10))) {
            Ok(Some(SessionEvent::Data(bytes))) => {
                protocol.write(&bytes);
                lock().feed(&bytes);
            }
            // A replay after a reconnect: the screen as it is now.
            Ok(Some(SessionEvent::Screen(bytes))) => {
                let mut screen = lock();
                let size = Size {
                    rows: screen.term.screen_lines() as u16,
                    columns: screen.term.columns() as u16,
                };
                let requests = screen.requests.clone();
                let cell = protocol.cell_size();
                protocol = TerminalActor::new(size.rows, size.columns, HISTORY);
                protocol.enable_graphics(pty_terminal::GraphicsOptions {
                    cell,
                    ..Default::default()
                });
                protocol.write(&bytes);
                *screen = Screen::new(size, requests);
                screen.attached = true;
                screen.feed(&bytes);
            }
            // The size every attached client shares, before the output drawn at it.
            Ok(Some(SessionEvent::Geometry { rows, cols })) => {
                protocol.resize(cols.max(1), rows.max(1));
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
        // The daemon owns replies; alacritty owns titles, bells and clipboard requests.
        protocol.take_pty_replies();
        protocol.take_events();
        let requests = lock().requests.clone();
        let clipboard_reads = std::mem::take(
            &mut requests
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clipboard_reads,
        );
        for reply in clipboard_reads {
            connection.write(&reply);
        }
        let mut screen = lock();
        let offset = screen.term.grid().display_offset();
        screen.graphics.update(&protocol, offset);
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

/// Translate the outer decoder's semantics; protocol decisions stay in pty-terminal.
fn protocol_mods(modifiers: KeyModifiers, state: KeyEventState) -> Mods {
    let mut mods = Mods::empty();
    for (flag, mode) in [
        (KeyModifiers::SHIFT, Mods::SHIFT),
        (KeyModifiers::ALT, Mods::ALT),
        (KeyModifiers::CONTROL, Mods::CTRL),
        (KeyModifiers::SUPER, Mods::SUPER),
    ] {
        mods.set(mode, modifiers.contains(flag));
    }
    mods.set(Mods::CAPS_LOCK, state.contains(KeyEventState::CAPS_LOCK));
    mods.set(Mods::NUM_LOCK, state.contains(KeyEventState::NUM_LOCK));
    mods
}

fn protocol_key(key: KeyEvent) -> Option<pty_terminal::KeyEvent> {
    use ProtocolKey as K;
    let keypad = key.state.contains(KeyEventState::KEYPAD);
    let logical = match key.code {
        KeyCode::Char(c) if keypad => match c {
            '0'..='9' => [
                K::Numpad0,
                K::Numpad1,
                K::Numpad2,
                K::Numpad3,
                K::Numpad4,
                K::Numpad5,
                K::Numpad6,
                K::Numpad7,
                K::Numpad8,
                K::Numpad9,
            ][c as usize - '0' as usize],
            '+' => K::NumpadAdd,
            '-' => K::NumpadSubtract,
            '*' => K::NumpadMultiply,
            '/' => K::NumpadDivide,
            '.' => K::NumpadDecimal,
            ',' => K::NumpadComma,
            '=' => K::NumpadEqual,
            _ => K::Unidentified,
        },
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'a'..='z' => [
                K::A,
                K::B,
                K::C,
                K::D,
                K::E,
                K::F,
                K::G,
                K::H,
                K::I,
                K::J,
                K::K,
                K::L,
                K::M,
                K::N,
                K::O,
                K::P,
                K::Q,
                K::R,
                K::S,
                K::T,
                K::U,
                K::V,
                K::W,
                K::X,
                K::Y,
                K::Z,
            ][c.to_ascii_lowercase() as usize - 'a' as usize],
            '0'..='9' => [
                K::Digit0,
                K::Digit1,
                K::Digit2,
                K::Digit3,
                K::Digit4,
                K::Digit5,
                K::Digit6,
                K::Digit7,
                K::Digit8,
                K::Digit9,
            ][c as usize - '0' as usize],
            ' ' => K::Space,
            '[' => K::BracketLeft,
            ']' => K::BracketRight,
            '\\' => K::Backslash,
            '`' => K::Backquote,
            ',' => K::Comma,
            '.' => K::Period,
            '/' => K::Slash,
            ';' => K::Semicolon,
            '\'' => K::Quote,
            '=' => K::Equal,
            '-' => K::Minus,
            _ => K::Unidentified,
        },
        KeyCode::Enter if keypad => K::NumpadEnter,
        KeyCode::Enter => K::Enter,
        KeyCode::Tab | KeyCode::BackTab => K::Tab,
        KeyCode::Backspace => K::Backspace,
        KeyCode::Esc => K::Escape,
        KeyCode::Up if keypad => K::NumpadUp,
        KeyCode::Up => K::ArrowUp,
        KeyCode::Down if keypad => K::NumpadDown,
        KeyCode::Down => K::ArrowDown,
        KeyCode::Left if keypad => K::NumpadLeft,
        KeyCode::Left => K::ArrowLeft,
        KeyCode::Right if keypad => K::NumpadRight,
        KeyCode::Right => K::ArrowRight,
        KeyCode::Home if keypad => K::NumpadHome,
        KeyCode::Home => K::Home,
        KeyCode::End if keypad => K::NumpadEnd,
        KeyCode::End => K::End,
        KeyCode::PageUp if keypad => K::NumpadPageUp,
        KeyCode::PageUp => K::PageUp,
        KeyCode::PageDown if keypad => K::NumpadPageDown,
        KeyCode::PageDown => K::PageDown,
        KeyCode::Insert if keypad => K::NumpadInsert,
        KeyCode::Insert => K::Insert,
        KeyCode::Delete if keypad => K::NumpadDelete,
        KeyCode::Delete => K::Delete,
        KeyCode::F(n @ 1..=25) => [
            K::F1,
            K::F2,
            K::F3,
            K::F4,
            K::F5,
            K::F6,
            K::F7,
            K::F8,
            K::F9,
            K::F10,
            K::F11,
            K::F12,
            K::F13,
            K::F14,
            K::F15,
            K::F16,
            K::F17,
            K::F18,
            K::F19,
            K::F20,
            K::F21,
            K::F22,
            K::F23,
            K::F24,
            K::F25,
        ][usize::from(n - 1)],
        KeyCode::KeypadBegin => K::NumpadBegin,
        KeyCode::Null => K::Space,
        _ => return None,
    };
    let mut event = pty_terminal::KeyEvent::press(logical);
    event.mods = protocol_mods(key.modifiers, key.state);
    if key.code == KeyCode::Null {
        event.mods.insert(Mods::CTRL);
    }
    if key.code == KeyCode::BackTab {
        event.mods.insert(Mods::SHIFT);
    }
    event.action = match key.kind {
        KeyEventKind::Press => KeyAction::Press,
        KeyEventKind::Repeat => KeyAction::Repeat,
        KeyEventKind::Release => KeyAction::Release,
    };
    if let KeyCode::Char(c) = key.code {
        event.text = Some(c.to_string());
        event.unshifted = Some(c.to_ascii_lowercase());
    }
    Some(event)
}

/// The VT keypad application mode needs SS3 forms even when the outer terminal
/// reports keypad keys through kitty. libghostty's text-first fallback omits these.
fn encode_key(actor: &TerminalActor, key: &pty_terminal::KeyEvent, keypad: bool) -> Vec<u8> {
    use ProtocolKey as K;
    if keypad && actor.kitty_flags() == 0 && key.action != KeyAction::Release {
        let code = match key.key {
            K::Numpad0 | K::NumpadInsert => Some('p'),
            K::Numpad1 | K::NumpadEnd => Some('q'),
            K::Numpad2 | K::NumpadDown => Some('r'),
            K::Numpad3 | K::NumpadPageDown => Some('s'),
            K::Numpad4 | K::NumpadLeft => Some('t'),
            K::Numpad5 | K::NumpadBegin => Some('u'),
            K::Numpad6 | K::NumpadRight => Some('v'),
            K::Numpad7 | K::NumpadHome => Some('w'),
            K::Numpad8 | K::NumpadUp => Some('x'),
            K::Numpad9 | K::NumpadPageUp => Some('y'),
            K::NumpadDecimal | K::NumpadDelete => Some('n'),
            K::NumpadEnter => Some('M'),
            K::NumpadAdd => Some('k'),
            K::NumpadSubtract => Some('m'),
            K::NumpadMultiply => Some('j'),
            K::NumpadDivide => Some('o'),
            K::NumpadComma => Some('l'),
            K::NumpadEqual => Some('X'),
            _ => None,
        };
        if let Some(code) = code {
            let modifier = 1
                + u8::from(key.mods.contains(Mods::SHIFT))
                + 2 * u8::from(key.mods.contains(Mods::ALT))
                + 4 * u8::from(key.mods.contains(Mods::CTRL));
            return if modifier == 1 {
                format!("\x1bO{code}").into_bytes()
            } else {
                format!("\x1b[1;{modifier}{code}").into_bytes()
            };
        }
    }
    actor.encode_key(key)
}

fn protocol_mouse(mouse: MouseEvent, column: u16, row: u16) -> Option<pty_terminal::MouseEvent> {
    use crossterm::event::MouseButton;
    let button = |b| match b {
        MouseButton::Left => ProtocolButton::Left,
        MouseButton::Middle => ProtocolButton::Middle,
        MouseButton::Right => ProtocolButton::Right,
    };
    let (action, which, pressed) = match mouse.kind {
        MouseEventKind::Down(b) => (MouseAction::Press, Some(button(b)), true),
        MouseEventKind::Up(b) => (MouseAction::Release, Some(button(b)), false),
        MouseEventKind::Drag(b) => (MouseAction::Motion, Some(button(b)), true),
        MouseEventKind::Moved => (MouseAction::Motion, None, false),
        MouseEventKind::ScrollUp => (MouseAction::Press, Some(ProtocolButton::Four), false),
        MouseEventKind::ScrollDown => (MouseAction::Press, Some(ProtocolButton::Five), false),
        MouseEventKind::ScrollLeft => (MouseAction::Press, Some(ProtocolButton::Six), false),
        MouseEventKind::ScrollRight => (MouseAction::Press, Some(ProtocolButton::Seven), false),
    };
    Some(pty_terminal::MouseEvent {
        action,
        button: which,
        mods: protocol_mods(mouse.modifiers, KeyEventState::empty()),
        col: column,
        row,
        any_button_pressed: pressed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn protocol_modes_control_key_encoding() {
        let mut term = TerminalActor::new(24, 80, 0);
        let encode = |term: &TerminalActor, code, modifiers| {
            term.encode_key(&protocol_key(key(code, modifiers)).unwrap())
        };
        assert_eq!(encode(&term, KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        term.write(b"\x1b[?1h");
        assert_eq!(encode(&term, KeyCode::Up, KeyModifiers::NONE), b"\x1bOA");
        term.write(b"\x1b[>27u");
        assert_eq!(
            encode(&term, KeyCode::Enter, KeyModifiers::SHIFT),
            b"\x1b[13;2u"
        );
        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            term.encode_key(&protocol_key(release).unwrap()),
            b"\x1b[97;1:3u"
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
            terminal.draw(&mut buf, area, false);
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
        terminal.wheel(
            20,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            0,
            0,
            true,
        );
        assert!(shown(&terminal).contains("old 0"), "{}", shown(&terminal));
    }

    #[test]
    fn a_drag_selects_as_a_terminal_copies_and_a_program_copies_and_names_itself() {
        use pty_core::protocol::{MessageType, encode_packet};
        use std::io::{Read as _, Write as _};
        let wait = |done: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !done() {
                assert!(start.elapsed() < Duration::from_secs(5), "timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let drawn = |terminal: &NativeTerminal| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 10, 4));
            let area = buf.area;
            terminal.draw(&mut buf, area, false);
            buf
        };
        let (stui, mut daemon) = UnixStream::pair().unwrap();
        let terminal = NativeTerminal::spawn(stui, "test", "one".into(), 4, 10);
        let mut bytes = [0_u8; 64];
        let _ = daemon.read(&mut bytes).unwrap();
        daemon
            .write_all(&encode_packet(MessageType::Screen, b""))
            .unwrap();
        // Ten columns: the first line wraps.
        daemon
            .write_all(&encode_packet(
                MessageType::Data,
                b"hello world wraps\r\nnext line",
            ))
            .unwrap();
        wait(&|| {
            drawn(&terminal)
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("next")
        });
        // A drag across the wrapped line copies it as the one line the program wrote.
        terminal.select_from(0, 0, 1);
        terminal.select_to(6, 1);
        assert_eq!(terminal.selected().as_deref(), Some("hello world wraps"));
        assert_eq!(drawn(&terminal)[(0, 0)].bg, theme::SELECTION_BG);
        assert_ne!(drawn(&terminal)[(0, 2)].bg, theme::SELECTION_BG);
        // Dragging back past where it began takes the cell under the mouse too.
        terminal.select_from(4, 0, 1);
        terminal.select_to(0, 0);
        assert_eq!(terminal.selected().as_deref(), Some("hello"));
        // A second press selects the word, a third the line.
        terminal.select_from(6, 2, 2);
        assert_eq!(terminal.selected().as_deref(), Some("line"));
        terminal.select_from(6, 2, 3);
        assert!(terminal.selected().unwrap().starts_with("next line"));
        // A press without a drag selects nothing.
        terminal.select_from(2, 2, 1);
        assert_eq!(terminal.selected(), None);
        // Typing lets go of the selection.
        terminal.select_from(0, 0, 1);
        terminal.select_to(3, 0);
        terminal.write(b"x".to_vec());
        assert_eq!(terminal.selected(), None);
        // A program copies through OSC 52 and names its window; stui hears both once.
        daemon
            .write_all(&encode_packet(
                MessageType::Data,
                b"\x1b]52;c;aGk=\x07\x1b]2;build log\x07\x07",
            ))
            .unwrap();
        wait(&|| terminal.title().is_some());
        assert_eq!(terminal.title().as_deref(), Some("build log"));
        let asked = terminal.asked();
        assert_eq!(asked.copied.as_deref(), Some("hi"));
        assert!(asked.bell);
        assert_eq!(
            terminal.asked(),
            Asked {
                copied: None,
                title: Some("build log".into()),
                bell: false,
                clipboard_reads: Vec::new(),
            }
        );
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
                terminal.draw(&mut buf, area, false);
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
            terminal.draw(&mut buf, area, false);
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
                terminal.draw(&mut buf, area, false);
            }
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("line 29")
        });
        terminal.wheel(
            5,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            0,
            0,
            true,
        );
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
