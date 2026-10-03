//! Attach this terminal to a terminal that another fleet host owns.
//!
//! A local `st terminals attach` proxies the PTY session's own bytes. Another host's terminal is
//! reachable only through the client gateway, the path paired clients use: `terminal.attach`
//! issues a stream capability, the gateway relays the owner's screens, and `terminal.input` and
//! `terminal.resize` go back to the owner under its fences. [`serve_session`] speaks the PTY
//! session protocol to the ordinary attach client over a socket pair, so a remote attach keeps
//! the local one's raw mode, Ctrl+\ detach, resize handling, and trailer: each screen becomes a
//! repaint, and keystrokes become input actions.

use std::fmt::Write as _;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use pty_client::ClientIo;
use pty_core::protocol::{MessageType, PacketReader, decode_size, encode_data, encode_screen};
use st3_client::{
    Client, ClientError, ErrorCode, Fence, TargetParameters, TerminalColor, TerminalCursorStyle,
    TerminalInputMode, TerminalInputParameters, TerminalLine, TerminalModes,
    TerminalResizeParameters, TerminalRun, TerminalScreen, TerminalStream,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

/// Larger input goes as several actions, because a relayed control request is bounded.
const INPUT_CHUNK_BYTES: usize = 4_096;
/// A busy owner or gateway can move a fence between the screen read and the action. Each
/// attempt reads fresh fences; only a fence that keeps moving fails.
const CONTROL_ATTEMPTS: u32 = 8;

/// Turns successive screens into the bytes that repaint a local terminal. After the first
/// screen it rewrites only the rows that changed.
#[derive(Default)]
pub struct Painter {
    rows: Vec<String>,
    first_row: usize,
    modes: Option<TerminalModes>,
    local_rows: u16,
}

impl Painter {
    /// Record a new local size; the next paint redraws every row.
    pub fn resize(&mut self, rows: u16) {
        self.local_rows = rows;
        self.rows.clear();
    }

    /// The bytes that make the local terminal show `screen`.
    pub fn paint(&mut self, screen: &TerminalScreen) -> Vec<u8> {
        let mut out = String::new();
        // One synchronized update, without autowrap, and with the cursor hidden while rows change.
        out.push_str("\x1b[?2026h\x1b[?7l\x1b[?25l");
        if self.modes.is_none() {
            // The attach client's reset leaves the alternate screen, restoring the local one.
            out.push_str("\x1b[?1049h");
        }
        write_modes(self.modes.as_ref(), &screen.modes, &mut out);
        let local_rows = match usize::from(self.local_rows) {
            0 => screen.rows,
            rows => rows,
        };
        let shown = screen.rows.min(local_rows);
        // A taller remote screen shows its top rows while they hold the cursor, and otherwise its
        // bottom rows, moving up only as far as the cursor.
        let first_row = if screen.cursor.row < shown {
            0
        } else {
            (screen.rows - shown).min(screen.cursor.row)
        };
        let mut rows = vec![String::new(); shown];
        for line in &screen.lines {
            if let Some(row) = line.row.checked_sub(first_row).filter(|row| *row < shown) {
                rows[row] = render_line(line);
            }
        }
        let full = self.rows.len() != shown || self.first_row != first_row;
        for (row, text) in rows.iter().enumerate() {
            if !full && self.rows[row] == *text {
                continue;
            }
            let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[2K{text}\x1b[0m", row + 1);
        }
        if full && shown < local_rows {
            let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[J", shown + 1);
        }
        let cursor = &screen.cursor;
        if cursor.visible {
            let style = match (cursor.style, cursor.blinking) {
                (TerminalCursorStyle::Block, true) => 1,
                (TerminalCursorStyle::Block, false) => 2,
                (TerminalCursorStyle::Underline, true) => 3,
                (TerminalCursorStyle::Underline, false) => 4,
                (TerminalCursorStyle::Bar, true) => 5,
                (TerminalCursorStyle::Bar, false) => 6,
                (TerminalCursorStyle::Unknown, _) => 0,
            };
            let _ = write!(
                out,
                "\x1b[{};{}H\x1b[{style} q\x1b[?25h",
                cursor.row - first_row + 1,
                cursor.column + 1
            );
        }
        out.push_str("\x1b[?7h\x1b[?2026l");
        self.rows = rows;
        self.first_row = first_row;
        self.modes = Some(screen.modes.clone());
        out.into_bytes()
    }
}

/// Switch the local terminal's input modes to the remote terminal's, so keys, paste, focus, and
/// mouse reports reach the remote program in the encoding it asked for.
fn write_modes(previous: Option<&TerminalModes>, modes: &TerminalModes, out: &mut String) {
    let mut private = |code: u16, on: bool, was: Option<bool>| {
        if was != Some(on) {
            let _ = write!(out, "\x1b[?{code}{}", if on { 'h' } else { 'l' });
        }
    };
    private(
        1,
        modes.application_cursor,
        previous.map(|modes| modes.application_cursor),
    );
    private(
        2004,
        modes.bracketed_paste,
        previous.map(|modes| modes.bracketed_paste),
    );
    private(
        1004,
        modes.focus_events,
        previous.map(|modes| modes.focus_events),
    );
    if previous.map(|modes| modes.application_keypad) != Some(modes.application_keypad) {
        out.push_str(if modes.application_keypad {
            "\x1b="
        } else {
            "\x1b>"
        });
    }
    if previous.map(|modes| modes.mouse_tracking.as_str()) != Some(modes.mouse_tracking.as_str()) {
        out.push_str("\x1b[?1000l\x1b[?1002l\x1b[?1003l");
        out.push_str(match modes.mouse_tracking.as_str() {
            "click" => "\x1b[?1000h",
            "drag" => "\x1b[?1002h",
            "motion" => "\x1b[?1003h",
            _ => "",
        });
    }
    if previous.map(|modes| modes.mouse_encoding.as_str()) != Some(modes.mouse_encoding.as_str()) {
        out.push_str("\x1b[?1005l\x1b[?1006l");
        out.push_str(match modes.mouse_encoding.as_str() {
            "sgr" => "\x1b[?1006h",
            "utf8" => "\x1b[?1005h",
            _ => "",
        });
    }
}

fn render_line(line: &TerminalLine) -> String {
    let mut out = String::new();
    if line.runs.is_empty() {
        push_text(&mut out, &line.text);
    }
    for run in &line.runs {
        out.push_str(&graphic_rendition(run));
        push_text(&mut out, &run.text);
    }
    out
}

/// Screen text is printable, but a control character must never reach the local terminal.
fn push_text(out: &mut String, text: &str) {
    out.extend(text.chars().map(|character| {
        if character.is_control() {
            ' '
        } else {
            character
        }
    }));
}

fn graphic_rendition(run: &TerminalRun) -> String {
    let mut codes = String::from("\x1b[0");
    for (on, code) in [
        (run.bold, "1"),
        (run.dim, "2"),
        (run.italic, "3"),
        (run.underline, "4"),
        (run.inverse, "7"),
    ] {
        if on {
            codes.push(';');
            codes.push_str(code);
        }
    }
    for (color, normal, bright, extended) in [(&run.fg, 30, 90, 38), (&run.bg, 40, 100, 48)] {
        let code = match color {
            None => None,
            Some(TerminalColor::Palette(index @ 0..=7)) => Some((normal + index).to_string()),
            Some(TerminalColor::Palette(index @ 8..=15)) => Some((bright + index - 8).to_string()),
            Some(TerminalColor::Palette(index)) => Some(format!("{extended};5;{index}")),
            Some(TerminalColor::Rgb(hex)) => {
                rgb(hex).map(|(red, green, blue)| format!("{extended};2;{red};{green};{blue}"))
            }
        };
        if let Some(code) = code {
            codes.push(';');
            codes.push_str(&code);
        }
    }
    codes.push('m');
    codes
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let digits = hex.strip_prefix('#').filter(|digits| digits.len() == 6)?;
    let channel = |at: usize| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

/// One control the attached person sends to the remote terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    Input(Vec<u8>),
    Resize { rows: u16, columns: u16 },
}

/// Why [`serve_session`] stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// The person detached, or the attach client went away.
    Detached,
    /// The remote terminal ended or could not be reached, for this reason.
    Ended(String),
}

/// Serve one PTY attach client over `socket`. It paints `first` when the client attaches and
/// every later screen from `screens`, and it forwards the client's input and size to `controls`.
/// A reason sent on `failures` ends the session.
pub async fn serve_session(
    socket: tokio::net::UnixStream,
    first: TerminalScreen,
    mut screens: mpsc::Receiver<Result<TerminalScreen, String>>,
    controls: mpsc::UnboundedSender<Control>,
    mut failures: mpsc::UnboundedReceiver<String>,
) -> SessionEnd {
    let (mut reader, mut writer) = socket.into_split();
    let mut packets = PacketReader::new();
    let mut painter = Painter::default();
    let mut latest = first;
    let mut attached = false;
    let mut buffer = vec![0_u8; 65_536];
    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => {
                let Ok(count @ 1..) = read else {
                    return SessionEnd::Detached;
                };
                let Ok(batch) = packets.feed(&buffer[..count]) else {
                    return SessionEnd::Detached;
                };
                for packet in batch {
                    match packet.type_ {
                        MessageType::Attach | MessageType::Resize => {
                            let (rows, columns) = decode_size(&packet.payload);
                            if rows == 0 || columns == 0 {
                                continue;
                            }
                            let size = (usize::from(rows), usize::from(columns));
                            if size != (latest.rows, latest.columns) {
                                let _ = controls.send(Control::Resize { rows, columns });
                            }
                            painter.resize(rows);
                            let paint = painter.paint(&latest);
                            let frame = if packet.type_ == MessageType::Attach {
                                attached = true;
                                encode_screen(&paint)
                            } else {
                                encode_data(&paint)
                            };
                            if writer.write_all(&frame).await.is_err() {
                                return SessionEnd::Detached;
                            }
                        }
                        MessageType::Data => {
                            let _ = controls.send(Control::Input(packet.payload));
                        }
                        MessageType::Detach => return SessionEnd::Detached,
                        _ => {}
                    }
                }
            }
            screen = screens.recv() => match screen {
                Some(Ok(screen)) => {
                    latest = screen;
                    if attached
                        && writer.write_all(&encode_data(&painter.paint(&latest))).await.is_err()
                    {
                        return SessionEnd::Detached;
                    }
                }
                Some(Err(reason)) => return SessionEnd::Ended(reason),
                None => return SessionEnd::Ended("the terminal stream closed".into()),
            },
            Some(reason) = failures.recv() => return SessionEnd::Ended(reason),
        }
    }
}

/// Attach this terminal to `terminal_id` through the client gateway until the person detaches or
/// the terminal ends. `name` is what the attach client's trailer calls the session. Returns the
/// process exit code.
pub async fn attach(client: &Client, terminal_id: &str, name: &str) -> Result<i32> {
    attach_with_io(client, terminal_id, name, ClientIo::default()).await
}

/// [`attach`] with the attach client's terminal descriptors named.
pub async fn attach_with_io(
    client: &Client,
    terminal_id: &str,
    name: &str,
    io: ClientIo,
) -> Result<i32> {
    let terminal_id = format!("terminal/{}", terminal_id.trim_start_matches("terminal/"));
    let (attachment, first, stream) = open(client, &terminal_id).await?;
    let incarnation = first.runtime_incarnation.clone();
    let (screens_sender, screens) = mpsc::channel(4);
    let follower = tokio::spawn(follow(stream, screens_sender));
    let (controls, pending) = mpsc::unbounded_channel();
    let (failed, failures) = mpsc::unbounded_channel();
    let sender = tokio::spawn(send_controls(
        client.clone(),
        terminal_id.clone(),
        incarnation.clone(),
        pending,
        failed,
    ));
    let (local, remote) = StdUnixStream::pair().context("open the terminal attach socket pair")?;
    remote.set_nonblocking(true)?;
    let remote = tokio::net::UnixStream::from_std(remote)?;
    let session = tokio::spawn(serve_session(remote, first, screens, controls, failures));
    let code = crate::client::attach_socket_with_io(name, local, io).await;
    let end = session.await.unwrap_or(SessionEnd::Detached);
    follower.abort();
    // Input typed just before a detach still arrives, but it never holds the prompt for long.
    let _ = tokio::time::timeout(Duration::from_secs(5), sender).await;
    detach(client, &terminal_id, &attachment, &incarnation).await;
    let code = code?;
    match end {
        SessionEnd::Detached => Ok(code),
        SessionEnd::Ended(reason) => {
            eprintln!("st terminals attach: {reason}");
            Ok(if code == 0 { 1 } else { code })
        }
    }
}

/// Take a terminal attachment and read the first screen of its stream.
async fn open(
    client: &Client,
    terminal_id: &str,
) -> Result<(String, TerminalScreen, TerminalStream)> {
    let mut attempt = 0;
    let attachment = loop {
        attempt += 1;
        let screen = client.terminal_screen(terminal_id).await?;
        let capabilities = client.capabilities().await?;
        let fence = Fence {
            snapshot_id: capabilities.snapshot.id,
            runtime_incarnation: Some(screen.value.runtime_incarnation),
            terminal_sequence: Some(capabilities.snapshot.store_index),
            ..Fence::default()
        };
        let nonce = uuid::Uuid::now_v7().simple().to_string();
        match client
            .terminal_attach(
                format!("action/{nonce}"),
                format!("terminal-attach:{nonce}"),
                fence,
                TargetParameters {
                    target_id: terminal_id.to_owned(),
                    ..TargetParameters::default()
                },
            )
            .await
        {
            Ok(response) => {
                break response
                    .value
                    .terminal_attachment
                    .context("the gateway returned no terminal attachment")?;
            }
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < CONTROL_ATTEMPTS => {}
            Err(error) => return Err(error.into()),
        }
    };
    let capability = attachment
        .stream_capability
        .as_deref()
        .context("the terminal attachment has no stream capability")?;
    let mut stream = client
        .terminal_stream(
            terminal_id,
            Some(&attachment.runtime_incarnation),
            capability,
        )
        .await?;
    let first = stream
        .next()
        .await?
        .context("the terminal stream closed before its first screen")?
        .value;
    Ok((attachment.attachment_id, first, stream))
}

async fn follow(mut stream: TerminalStream, screens: mpsc::Sender<Result<TerminalScreen, String>>) {
    loop {
        let next = match stream.next().await {
            Ok(Some(screen)) => Ok(screen.value),
            Ok(None) => Err("the terminal stream closed".to_owned()),
            Err(error) => Err(format!("the terminal stream ended: {error}")),
        };
        let ended = next.is_err();
        if screens.send(next).await.is_err() || ended {
            return;
        }
    }
}

/// Deliver controls in order until the session ends. Waiting input becomes one action, and
/// waiting resizes collapse to the latest.
async fn send_controls(
    client: Client,
    terminal_id: String,
    incarnation: String,
    mut pending: mpsc::UnboundedReceiver<Control>,
    failed: mpsc::UnboundedSender<String>,
) {
    let mut held = None;
    loop {
        let control = match held.take() {
            Some(control) => control,
            None => match pending.recv().await {
                Some(control) => control,
                None => return,
            },
        };
        let control = coalesce(control, &mut pending, &mut held);
        let delivered = match &control {
            Control::Input(bytes) => {
                let mut delivered = Ok(());
                for chunk in bytes.chunks(INPUT_CHUNK_BYTES) {
                    delivered =
                        deliver(&client, &terminal_id, &incarnation, Delivery::Input(chunk)).await;
                    if delivered.is_err() {
                        break;
                    }
                }
                delivered
            }
            Control::Resize { rows, columns } => {
                deliver(
                    &client,
                    &terminal_id,
                    &incarnation,
                    Delivery::Resize(*rows, *columns),
                )
                .await
            }
        };
        if let Err(error) = delivered {
            let _ = failed.send(format!("the terminal did not take input: {error:#}"));
            return;
        }
    }
}

/// Merge the controls already waiting behind `control`. The first waiting control of another
/// kind goes to `held`, so order is kept.
fn coalesce(
    mut control: Control,
    pending: &mut mpsc::UnboundedReceiver<Control>,
    held: &mut Option<Control>,
) -> Control {
    while let Ok(next) = pending.try_recv() {
        match (&mut control, next) {
            (Control::Input(bytes), Control::Input(more)) => bytes.extend(more),
            (Control::Resize { .. }, next @ Control::Resize { .. }) => control = next,
            (_, next) => {
                *held = Some(next);
                break;
            }
        }
    }
    control
}

#[derive(Clone, Copy)]
enum Delivery<'a> {
    Input(&'a [u8]),
    Resize(u16, u16),
}

async fn deliver(
    client: &Client,
    terminal_id: &str,
    incarnation: &str,
    delivery: Delivery<'_>,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let screen = client.terminal_screen(terminal_id).await?;
        anyhow::ensure!(
            screen.value.runtime_incarnation == incarnation,
            "the terminal restarted; attach again"
        );
        let fence = Fence {
            snapshot_id: screen.snapshot.id,
            runtime_incarnation: Some(incarnation.to_owned()),
            terminal_sequence: Some(screen.value.next_sequence),
            ..Fence::default()
        };
        let nonce = uuid::Uuid::now_v7().simple().to_string();
        let result = match delivery {
            Delivery::Input(bytes) => client
                .terminal_input(
                    format!("action/{nonce}"),
                    format!("terminal-input:{nonce}"),
                    fence,
                    TerminalInputParameters {
                        terminal_id: terminal_id.to_owned(),
                        mode: TerminalInputMode::Raw,
                        value: base64::engine::general_purpose::STANDARD.encode(bytes),
                    },
                )
                .await
                .map(drop),
            Delivery::Resize(rows, columns) => client
                .terminal_resize(
                    format!("action/{nonce}"),
                    format!("terminal-resize:{nonce}"),
                    fence,
                    TerminalResizeParameters {
                        terminal_id: terminal_id.to_owned(),
                        rows,
                        columns,
                    },
                )
                .await
                .map(drop),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < CONTROL_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(20 * u64::from(attempt))).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// End the attachment. The terminal may already be gone, and an attachment also expires, so a
/// failure here changes nothing for the person.
async fn detach(client: &Client, terminal_id: &str, attachment: &str, incarnation: &str) {
    for _ in 0..3 {
        let Ok(screen) = client.terminal_screen(terminal_id).await else {
            return;
        };
        let fence = Fence {
            snapshot_id: screen.snapshot.id,
            runtime_incarnation: Some(incarnation.to_owned()),
            terminal_sequence: Some(screen.value.next_sequence),
            ..Fence::default()
        };
        let nonce = uuid::Uuid::now_v7().simple().to_string();
        match client
            .terminal_detach(
                format!("action/{nonce}"),
                format!("terminal-detach:{nonce}"),
                fence,
                TargetParameters {
                    target_id: attachment.to_owned(),
                    ..TargetParameters::default()
                },
            )
            .await
        {
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) => {}
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::protocol::{Packet, encode_attach, encode_detach, encode_resize};
    use serde_json::json;

    fn screen(rows: usize, lines: &[(usize, &str)], cursor: (usize, usize)) -> TerminalScreen {
        serde_json::from_value(json!({
            "kind": "terminal-screen",
            "terminal_id": "terminal/agent/example",
            "runtime_incarnation": "runtime:i1",
            "revision": format!("{lines:?}"),
            "rows": rows,
            "columns": 20,
            "cursor": {
                "row": cursor.0, "column": cursor.1, "visible": true, "style": "bar",
                "blinking": false
            },
            "title": "example",
            "modes": {
                "alternate_screen": true, "application_cursor": true, "application_keypad": false,
                "bracketed_paste": true, "focus_events": false, "mouse_tracking": "none",
                "mouse_encoding": "default"
            },
            "lines": lines.iter().map(|(row, text)| json!({
                "row": row, "text": text, "runs": [{"text": text}], "redacted": false,
                "truncated": false
            })).collect::<Vec<_>>(),
            "next_sequence": 7,
            "truncated": false
        }))
        .unwrap()
    }

    #[test]
    fn the_first_paint_draws_every_row_and_later_paints_only_changes() {
        let mut painter = Painter::default();
        painter.resize(3);
        let first = String::from_utf8(painter.paint(&screen(
            3,
            &[(0, "$ ls"), (1, "README.md"), (2, "$ ")],
            (2, 2),
        )))
        .unwrap();
        assert!(first.starts_with("\x1b[?2026h\x1b[?7l\x1b[?25l\x1b[?1049h"));
        assert!(first.contains("\x1b[?1h") && first.contains("\x1b[?2004h"));
        for (row, text) in [(1, "$ ls"), (2, "README.md"), (3, "$ ")] {
            assert!(first.contains(&format!("\x1b[{row};1H\x1b[0m\x1b[2K\x1b[0m{text}\x1b[0m")));
        }
        assert!(first.ends_with("\x1b[3;3H\x1b[6 q\x1b[?25h\x1b[?7h\x1b[?2026l"));

        let second = String::from_utf8(painter.paint(&screen(
            3,
            &[(0, "$ ls"), (1, "README.md"), (2, "$ git")],
            (2, 5),
        )))
        .unwrap();
        assert!(second.contains("\x1b[3;1H\x1b[0m\x1b[2K\x1b[0m$ git\x1b[0m"));
        assert!(!second.contains("README.md") && !second.contains("\x1b[1;1H"));
        assert!(!second.contains("1049") && !second.contains("\x1b[?1h"));
    }

    #[test]
    fn a_taller_remote_screen_shows_the_rows_that_hold_its_cursor() {
        let lines = [(0, "top"), (1, "middle"), (2, "prompt")];
        let mut painter = Painter::default();
        painter.resize(2);
        let bottom = String::from_utf8(painter.paint(&screen(3, &lines, (2, 0)))).unwrap();
        assert!(!bottom.contains("top") && bottom.contains("\x1b[2;1H\x1b[0m\x1b[2K\x1b[0mprompt"));
        let mut painter = Painter::default();
        painter.resize(2);
        let top = String::from_utf8(painter.paint(&screen(3, &lines, (0, 1)))).unwrap();
        assert!(top.contains("\x1b[1;1H\x1b[0m\x1b[2K\x1b[0mtop") && !top.contains("prompt"));
        assert!(top.contains("\x1b[1;2H"));
    }

    #[test]
    fn a_shorter_remote_screen_clears_the_local_rows_below_it() {
        let mut painter = Painter::default();
        painter.resize(5);
        let paint = String::from_utf8(painter.paint(&screen(2, &[(0, "one")], (0, 0)))).unwrap();
        assert!(paint.contains("\x1b[3;1H\x1b[0m\x1b[J"));
    }

    #[test]
    fn runs_keep_their_style_and_never_carry_control_characters() {
        let run: TerminalRun = serde_json::from_value(json!({
            "text": "a\u{1b}]0;x\u{7}b", "fg": 9, "bg": "#0a0b0c", "bold": true, "inverse": true
        }))
        .unwrap();
        assert_eq!(graphic_rendition(&run), "\x1b[0;1;7;91;48;2;10;11;12m");
        let line = TerminalLine {
            row: 0,
            text: String::new(),
            runs: vec![run],
            wrapped: None,
            redacted: false,
            truncated: false,
        };
        assert_eq!(render_line(&line), "\x1b[0;1;7;91;48;2;10;11;12ma ]0;x b");
        let palette: TerminalRun =
            serde_json::from_value(json!({"text": "x", "fg": 2, "bg": 200, "dim": true})).unwrap();
        assert_eq!(graphic_rendition(&palette), "\x1b[0;2;32;48;5;200m");
        assert_eq!(rgb("#zz0000"), None);
    }

    #[test]
    fn waiting_input_merges_and_waiting_resizes_keep_the_latest() {
        let (sender, mut pending) = mpsc::unbounded_channel();
        for control in [
            Control::Input(b"ab".to_vec()),
            Control::Input(b"c".to_vec()),
            Control::Resize {
                rows: 10,
                columns: 40,
            },
            Control::Resize {
                rows: 20,
                columns: 80,
            },
            Control::Input(b"d".to_vec()),
        ] {
            sender.send(control).unwrap();
        }
        let mut held = None;
        let first = pending.try_recv().unwrap();
        assert_eq!(
            coalesce(first, &mut pending, &mut held),
            Control::Input(b"abc".to_vec())
        );
        let resize = held.take().unwrap();
        assert_eq!(
            coalesce(resize, &mut pending, &mut held),
            Control::Resize {
                rows: 20,
                columns: 80
            }
        );
        assert_eq!(held, Some(Control::Input(b"d".to_vec())));
    }

    async fn packets(
        socket: &mut tokio::net::UnixStream,
        reader: &mut PacketReader,
    ) -> Vec<Packet> {
        let mut buffer = vec![0_u8; 65_536];
        let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
            .await
            .expect("the session answers")
            .unwrap();
        reader.feed(&buffer[..count]).unwrap()
    }

    #[tokio::test]
    async fn a_session_paints_screens_and_forwards_input_until_the_person_detaches() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (screens, screen_updates) = mpsc::channel(4);
        let (controls, mut forwarded) = mpsc::unbounded_channel();
        let (_failed, failures) = mpsc::unbounded_channel();
        let session = tokio::spawn(serve_session(
            server,
            screen(2, &[(0, "$ ")], (0, 2)),
            screen_updates,
            controls,
            failures,
        ));
        let mut reader = PacketReader::new();

        client.write_all(&encode_attach(2, 20)).await.unwrap();
        let attached = packets(&mut client, &mut reader).await;
        assert_eq!(attached[0].type_, MessageType::Screen);
        assert!(String::from_utf8_lossy(&attached[0].payload).contains("$ "));

        client.write_all(&encode_data(b"ls\r")).await.unwrap();
        assert_eq!(
            forwarded.recv().await,
            Some(Control::Input(b"ls\r".to_vec()))
        );
        client.write_all(&encode_resize(30, 100)).await.unwrap();
        assert_eq!(
            forwarded.recv().await,
            Some(Control::Resize {
                rows: 30,
                columns: 100
            })
        );
        let repainted = packets(&mut client, &mut reader).await;
        assert_eq!(repainted[0].type_, MessageType::Data);

        screens
            .send(Ok(screen(2, &[(0, "$ ls"), (1, "README.md")], (1, 9))))
            .await
            .unwrap();
        let changed = packets(&mut client, &mut reader).await;
        assert_eq!(changed[0].type_, MessageType::Data);
        assert!(String::from_utf8_lossy(&changed[0].payload).contains("README.md"));

        client.write_all(&encode_detach()).await.unwrap();
        assert_eq!(session.await.unwrap(), SessionEnd::Detached);
    }

    #[tokio::test]
    async fn a_session_ends_with_the_reason_its_terminal_stopped() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (screens, screen_updates) = mpsc::channel(4);
        let (controls, _forwarded) = mpsc::unbounded_channel();
        let (failed, failures) = mpsc::unbounded_channel();
        let session = tokio::spawn(serve_session(
            server,
            screen(2, &[(0, "$ ")], (0, 2)),
            screen_updates,
            controls,
            failures,
        ));
        client.write_all(&encode_attach(2, 20)).await.unwrap();
        failed
            .send("the terminal restarted; attach again".into())
            .unwrap();
        assert_eq!(
            session.await.unwrap(),
            SessionEnd::Ended("the terminal restarted; attach again".into())
        );

        let (_client, server) = tokio::net::UnixStream::pair().unwrap();
        let (controls, _forwarded) = mpsc::unbounded_channel();
        let (_failed, failures) = mpsc::unbounded_channel();
        let (screens_closed, screen_updates) = mpsc::channel(4);
        drop(screens);
        let session = tokio::spawn(serve_session(
            server,
            screen(2, &[], (0, 0)),
            screen_updates,
            controls,
            failures,
        ));
        screens_closed
            .send(Err("the terminal stream closed".into()))
            .await
            .unwrap();
        assert_eq!(
            session.await.unwrap(),
            SessionEnd::Ended("the terminal stream closed".into())
        );
    }
}
