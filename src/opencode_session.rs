//! Controlled OpenCode launch: presence, observed harness state, and native server delivery.
//!
//! OpenCode's interactive TUI is also a server: the wrapper allocates a loopback port and a
//! per-seat password, launches the TUI bound to them, and speaks plain HTTP to its own child. Two
//! consumers hang off that surface. The observed-state producer subscribes to the `/event` SSE
//! stream and projects session status, permission asks, and questions into the generic
//! `harness-state` record — evidence-gated, so a dropped stream stops the heartbeat and the record
//! ages out rather than restating a state nobody is watching. The delivery pump mirrors the Codex
//! FIFO discipline through the shared [`crate::delivery_ledger`]: an `attempted` entry is durable
//! before transport, the transport is `POST /session/<id>/prompt_async` with a caller-derived
//! stable `messageID`, and the only receipt this transport can produce is the message read back
//! from the server — never the `/tui/*` endpoints, which acknowledge input even when no TUI is
//! attached. That read-back is graded as `persisted`, because it proves the server STORED the
//! message and says nothing about the session scheduler admitting it; persistence therefore holds
//! the inbox entry rather than releasing it, and is never mistaken for consumption.
//!
//! Fail-closed gate: delivery requires both a supported `opencode --version` and a live `/doc`
//! subset check proving the exact API arms st2 depends on. Observation requires only the `/doc`
//! check — its vocabulary already degrades to indeterminate on anything unrecognized.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::driver_diagnostic::{
    Driver as DiagnosticDriver, Publisher as DiagnosticPublisher, Reason as DiagnosticReason,
    Source as DiagnosticSource, Stage as DiagnosticStage, Support as DiagnosticSupport,
};
use crate::harness_state::{self, Activity, Ask, BlockedOn, InputBuffer, Observation, Writer};
use crate::provider_session::{PROVIDER_POLL, STOP, install_signal_handler};
use crate::{delivery_ledger, ding, harness_context, harness_version, message, status};

/// OpenCode MINORS whose `/event`, `/session`, and `prompt_async` surfaces were verified
/// (1.18, measured at 1.18.19). The live `/doc` check below guards the shape; this list guards
/// the semantics behind it.
///
/// Admission is per minor so a patch bump inside a verified minor keeps native delivery instead
/// of silently degrading to none — the profile shipping 1.18.25 against a 1.18.19 allowlist is
/// exactly that case. The tradeoff is explicit: `/doc` still catches a SHAPE change on any
/// version, but a SEMANTIC change within an admitted minor would not be caught, which is the
/// risk this widening accepts. A new MINOR still needs the surfaces re-verified.
const SUPPORTED_OPENCODE_MINORS: [(u32, u32); 1] = [(1, 18)];

const STOP_GRACE: Duration = Duration::from_secs(5);
const INBOX_REFRESH_FALLBACK: Duration = Duration::from_secs(2);
const DELIVERY_RETRY: Duration = Duration::from_secs(2);
const SEED_RETRY: Duration = Duration::from_millis(250);
const SSE_RECONNECT: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Every API arm the wrapper depends on, as it appears in the served OpenAPI document. A missing
/// marker names itself in the refusal, so a surface change is a diagnosis rather than a mystery.
const REQUIRED_API_MARKERS: [&str; 12] = [
    "prompt_async",
    "messageID",
    "session.status",
    "session.idle",
    "session.error",
    "permission.asked",
    "permission.replied",
    "question.asked",
    // The exit arms and pending listings are as load-bearing as the entries: a release renaming
    // a question exit would otherwise pass the gate and then hold `blockedOn: human` forever,
    // and the reconnect seed reads both listing endpoints.
    "question.replied",
    "question.rejected",
    "/permission",
    "/question",
];

pub fn run(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    opencode_argv: Vec<String>,
) -> Result<()> {
    let this_host = crate::run::detect_host();
    let agent_dir = message::resolve_declared_dir(catalog_root, &identity, &this_host)?
        .with_context(|| format!("opencode driver agent '{identity}' is not declared"))?;
    anyhow::ensure!(
        !opencode_argv.is_empty(),
        "opencode driver '{runtime_id}' has no provider argv"
    );
    let version_probe = supported_version(&opencode_argv[0]);
    let (version_ok, producer_version, support, version_failure) = match version_probe {
        Ok(version) => {
            let supported = version_is_supported(&version);
            if !supported {
                tracing::warn!(
                    "st2 opencode-session: version {version} is unverified (supported minors: {}); native delivery disabled",
                    harness_version::series_display(&SUPPORTED_OPENCODE_MINORS)
                );
            }
            (
                supported,
                Some(version),
                if supported {
                    DiagnosticSupport::Supported
                } else {
                    DiagnosticSupport::Unsupported
                },
                (!supported).then_some(DiagnosticReason::UnsupportedVersion),
            )
        }
        Err(error) => {
            tracing::warn!("st2 opencode-session: cannot read opencode version: {error:#}");
            (
                false,
                None,
                DiagnosticSupport::Unknown,
                Some(DiagnosticReason::VersionProbeFailed),
            )
        }
    };

    let port = allocate_port()?;
    let password = random_password()?;
    let mut argv = opencode_argv;
    argv.extend([
        "--port".to_string(),
        port.to_string(),
        "--hostname".to_string(),
        "127.0.0.1".to_string(),
    ]);
    let client = Client::new(port, &password);

    install_signal_handler();
    // The written claim comes BEFORE the provider spawns: a claim that cannot be written aborts
    // the launch while there is still nothing to leak, and it supersedes whatever a predecessor
    // left — a still-fresh live record included — before this wrapper's first observation.
    let mut session = {
        let session = harness_state::session_token();
        let seq = harness_state::claim(&agent_dir, identity.clone(), "opencode", &session)?;
        let mut diagnostics =
            DiagnosticPublisher::new(&agent_dir, DiagnosticDriver::OpenCode, producer_version, support);
        if let Some(reason) = version_failure {
            diagnostics.publish(
                DiagnosticStage::VersionGate,
                reason,
                DiagnosticSource::VersionProbe,
            );
        } else {
            diagnostics.clear(DiagnosticStage::VersionGate);
        }
        Session {
            client,
            version_ok,
            status_path: status::status_path(&agent_dir),
            // The pty session vouching for the record is the wrapper's task: the runtime ID
            // names the registry entry, and only aliases the identity on driver-expanded seats.
            writer: Writer::new(
                &agent_dir,
                identity.clone(),
                "opencode",
                Some(runtime_id.clone()),
            )
            .with_ownership(session.clone(), seq),
            // The same incarnation token as the state record beside it, carried as provenance
            // only (HC-R15): nothing on this record is fenced on it. The claim above already
            // removed any predecessor's context record, so no second removal belongs here.
            context: match ContextProducer::new(&agent_dir, &identity, &session) {
                Ok(producer) => Some(producer),
                Err(error) => {
                    tracing::warn!(
                        "st2 opencode-session: no context record for this seat: {error:#}"
                    );
                    None
                }
            },
            delivery: Delivery::new(catalog_root, &agent_dir, &this_host, &identity, &runtime_id),
            diagnostics,
        }
    };
    let mut child = match spawn_provider(&argv, &password) {
        Ok(child) => child,
        Err(error) => {
            // The claim already replaced whatever the predecessor left; returning through `?`
            // would leave the exitless `ended (superseded)` placeholder standing as a false
            // takeover. The launch failed under THIS session's ownership, so the record ends
            // honestly here instead (the same contract pi's launch path keeps).
            let _ = session.writer.observe(
                Observation::new(Activity::Ended, BlockedOn::None, InputBuffer::Unknown)
                    .with_reason("launch-error")
                    .with_exit("exit unknown"),
            );
            return Err(error);
        }
    };

    run_session(session, &mut child, &agent_dir)
}

// ---- wrapper session loop --------------------------------------------------------------------

struct Session {
    client: Client,
    version_ok: bool,
    status_path: PathBuf,
    writer: Writer,
    /// The numeric axis, `None` only where the record has nowhere safe to stage. Its absence is a
    /// missing advisory number, never a reason to fail a launch that is otherwise healthy.
    context: Option<ContextProducer>,
    delivery: Delivery,
    diagnostics: DiagnosticPublisher,
}

fn run_session(mut session: Session, child: &mut Child, agent_dir: &Path) -> Result<()> {
    let (wake_tx, wake_rx) = mpsc::channel();
    let _watcher = crate::watch::watch_delivery_inputs(agent_dir, wake_tx);
    let (event_tx, event_rx) = mpsc::channel();
    let sse_stop = std::sync::Arc::new(AtomicBool::new(false));

    let mut machine = EventMachine::default();
    let mut api_ok = false;
    let mut sse_started = false;
    let mut sse_connected = false;
    let mut evidence = false;
    let mut next_seed_attempt = Instant::now();
    let mut next_gate_attempt = Instant::now();
    let mut next_presence = Instant::now();
    let mut next_inbox = Instant::now();
    // The context producer's own handle on the server: `Client` is a port and a password, and a
    // separate one keeps its pull from borrowing the session while the producer is borrowed
    // mutably.
    let context_client = session.client.clone();

    let outcome = loop {
        if STOP.load(Ordering::SeqCst) {
            // The terminal record precedes the escalation: SIGKILL takes this wrapper with its
            // group, so nothing after the kill can write (§D of the harness-state design). When
            // the group yields inside the grace window the wrapper survives, so the record is
            // rewritten with the exit the reap actually observed — "stopped" remains only as
            // escalation cover.
            let _ = session.writer.ended("stopped");
            let reaped = stop_provider_group(child);
            if let Ok(Some(exit)) = &reaped {
                let _ = session.writer.ended(describe_exit(*exit));
            }
            break reaped.map(|_| ());
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                let _ = session.writer.ended(describe_exit(exit));
                break completed(exit);
            }
            Ok(None) => {}
            Err(error) => {
                // The liveness check failing is a terminal outcome too: without this write the
                // claim placeholder stands as the visible state (the self-review's error-arm
                // class).
                let _ = session.writer.observe(
                    Observation::new(Activity::Ended, BlockedOn::None, InputBuffer::Unknown)
                        .with_reason("launch-error")
                        .with_exit("exit unknown"),
                );
                return Err(error).context("checking opencode provider");
            }
        }

        // The API gate needs the child's server to answer; retry until it does.
        if !api_ok && Instant::now() >= next_gate_attempt {
            match session.client.get_json("/doc") {
                Ok(doc) => match check_openapi_subset(&doc) {
                    Ok(()) => {
                        api_ok = true;
                        session.diagnostics.clear(DiagnosticStage::ApiGate);
                    }
                    Err(error) => {
                        session.diagnostics.publish(
                            DiagnosticStage::ApiGate,
                            DiagnosticReason::IncompatibleApi,
                            DiagnosticSource::OpenApiDocument,
                        );
                        tracing::warn!("st2 opencode-session: API gate failed: {error:#}");
                        // A failed shape check is terminal for this launch: the surface will not
                        // change until the binary does. Stop probing; run presence-only.
                        next_gate_attempt = Instant::now() + Duration::from_secs(3600);
                    }
                },
                Err(_) => {
                    session.diagnostics.publish(
                        DiagnosticStage::ApiGate,
                        DiagnosticReason::ApiUnavailable,
                        DiagnosticSource::OpenApiDocument,
                    );
                    next_gate_attempt = Instant::now() + Duration::from_millis(250);
                }
            }
        }
        if api_ok && !sse_started {
            spawn_sse_reader(session.client.clone(), event_tx.clone(), sse_stop.clone());
            sse_started = true;
        }

        // Accumulated across the whole drain and published once at its end: `message.updated` and
        // `session.updated` arrive back-to-back, so one publish per batch carries the turn's cost
        // and cumulative total alongside the numerator that made the reading fresh.
        let mut fresh_reading = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                SseMessage::Connected => {
                    machine = EventMachine::default();
                    sse_connected = true;
                    session.diagnostics.clear(DiagnosticStage::Sse);
                    // Evidence turns on only once the level seed succeeds: resuming heartbeats
                    // on a transiently failed seed would re-stamp whatever the disk last said.
                    evidence = seed_with_diagnostics(
                        &session.client,
                        &mut machine,
                        &mut session.diagnostics,
                    );
                    next_seed_attempt = Instant::now() + SEED_RETRY;
                }
                SseMessage::Disconnected(reason) => {
                    sse_connected = false;
                    evidence = false;
                    session.diagnostics.publish(
                        DiagnosticStage::Sse,
                        reason,
                        DiagnosticSource::EventStream,
                    );
                    // Everything from here until a successful reseed happened unobserved: the
                    // next observation must open a fresh transition even if it restates the
                    // pre-outage tuple.
                    session.writer.interrupt();
                }
                SseMessage::Event(value) => {
                    if let Some(sid) = event_session_id(&value) {
                        session.delivery.saw_session(sid);
                    }
                    machine.apply(&value);
                    if let Some(context) = session.context.as_mut() {
                        fresh_reading |= context.apply(&value);
                    }
                }
            }
        }
        // The numeric axis is independent of the categorical one (HC-R07): it is not gated on
        // `evidence`, not reset by a reconnect, and not interrupted by a poisoned projection. A
        // token count stays true while its record ages, and the reader returns it with that age.
        if let Some(context) = session.context.as_mut() {
            context.refresh_windows(&context_client, api_ok);
            if fresh_reading {
                context.publish();
            }
        }
        if machine.poisoned && machine.ended.is_none() && evidence {
            // The projection went untrustworthy mid-stream: stop heartbeating over it and let
            // the level seed rebuild the whole picture from the server's own truth.
            evidence = false;
            session.writer.interrupt();
            next_seed_attempt = Instant::now();
            session.diagnostics.publish(
                DiagnosticStage::Sse,
                DiagnosticReason::UnknownEvent,
                DiagnosticSource::EventStream,
            );
        }
        if sse_connected && !evidence && Instant::now() >= next_seed_attempt {
            evidence = seed_with_diagnostics(
                &session.client,
                &mut machine,
                &mut session.diagnostics,
            );
        }
        if evidence && let Some(observation) = machine.observation() {
            let _ = session.writer.observe(observation);
        }

        let now = Instant::now();
        if now >= next_presence {
            let _ = status::refresh(&session.status_path);
            if evidence {
                let _ = session.writer.heartbeat();
            }
            next_presence = now + status::STATUS_REFRESH;
        }

        let mut inbox_due = now >= next_inbox;
        while wake_rx.try_recv().is_ok() {
            inbox_due = true;
        }
        if inbox_due {
            if session.version_ok && api_ok {
                session
                    .delivery
                    .pump_diagnosed(&session.client, &mut session.diagnostics);
            }
            next_inbox = Instant::now() + INBOX_REFRESH_FALLBACK;
        }

        thread::sleep(PROVIDER_POLL);
    };
    sse_stop.store(true, Ordering::SeqCst);
    outcome
}

fn spawn_provider(argv: &[String], password: &str) -> Result<Child> {
    use std::os::unix::process::CommandExt as _;
    let (program, args) = argv
        .split_first()
        .context("opencode provider argv is empty")?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env("OPENCODE_SERVER_PASSWORD", password)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("starting opencode provider {program}"))
}

fn completed(exit: ExitStatus) -> Result<()> {
    anyhow::ensure!(exit.success(), "opencode provider exited with {exit}");
    Ok(())
}

fn stop_provider_group(child: &mut Child) -> Result<Option<ExitStatus>> {
    let process_group = unsafe { libc::getpgrp() };
    anyhow::ensure!(
        process_group > 1,
        "refusing to signal process group {process_group}"
    );
    unsafe {
        libc::kill(-process_group, libc::SIGTERM);
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if let Some(exit) = child.try_wait()? {
            return Ok(Some(exit));
        }
        thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    Ok(child.wait().ok())
}

fn describe_exit(exit: ExitStatus) -> String {
    match (exit.code(), exit.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "exit unknown".to_string(),
    }
}

fn supported_version(binary: &str) -> Result<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {binary} --version"))?;
    anyhow::ensure!(output.status.success(), "{binary} --version failed");
    let version = String::from_utf8_lossy(&output.stdout);
    let version = version
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    anyhow::ensure!(!version.is_empty(), "{binary} --version printed nothing");
    Ok(version)
}

/// Whether a reported opencode version is inside a verified MINOR. This is the whole admission
/// decision, named so a test can exercise the same code the wrapper runs rather than re-deriving
/// it — an assertion that re-implements the rule cannot notice the rule changing.
fn version_is_supported(version: &str) -> bool {
    harness_version::find_release(version, "opencode")
        .is_some_and(|(_, release)| SUPPORTED_OPENCODE_MINORS.contains(&release.series()))
}

/// Verify the served OpenAPI document still carries every arm st2 consumes. Substring markers are
/// deliberate: the document nests these identifiers at unstable depths across versions, and the
/// check must name what went missing rather than fail on structure.
fn check_openapi_subset(doc: &Value) -> Result<()> {
    let serialized = serde_json::to_string(doc).unwrap_or_default();
    let missing: Vec<&str> = REQUIRED_API_MARKERS
        .iter()
        .copied()
        .filter(|marker| !serialized.contains(marker))
        .collect();
    anyhow::ensure!(
        missing.is_empty(),
        "OpenCode /doc no longer offers: {}",
        missing.join(", ")
    );
    Ok(())
}

fn allocate_port() -> Result<u16> {
    // Bind-then-release: the child rebinds the port. The race window is real but tiny, loopback
    // only, and a lost race fails the launch loudly at spawn.
    let listener = TcpListener::bind("127.0.0.1:0").context("allocating opencode port")?;
    Ok(listener.local_addr()?.port())
}

fn random_password() -> Result<String> {
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("reading /dev/urandom for the opencode server password")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

// ---- minimal loopback HTTP client ------------------------------------------------------------

/// Plain HTTP/1.1 over loopback with basic auth; one connection per request, `Connection: close`.
/// The tree ships no HTTP client and the runner is sync by design, so this stays hand-rolled and
/// exactly as small as the four requests the wrapper makes.
#[derive(Clone)]
struct Client {
    addr: String,
    auth: String,
    /// The SSE silence horizon; [`SSE_SILENCE`] in production, shrunk only by tests.
    sse_silence: Duration,
}

impl Client {
    fn new(port: u16, password: &str) -> Self {
        Self {
            addr: format!("127.0.0.1:{port}"),
            auth: base64(format!("opencode:{password}").as_bytes()),
            sse_silence: SSE_SILENCE,
        }
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let (status, body) = self.request("GET", path, None)?;
        anyhow::ensure!(status == 200, "GET {path} returned {status}");
        serde_json::from_slice(&body).with_context(|| format!("GET {path} returned invalid JSON"))
    }

    fn status_of_get(&self, path: &str) -> Result<u16> {
        Ok(self.request("GET", path, None)?.0)
    }

    fn post_json(&self, path: &str, payload: &Value) -> Result<u16> {
        Ok(self.request("POST", path, Some(payload))?.0)
    }

    fn request(&self, method: &str, path: &str, payload: Option<&Value>) -> Result<(u16, Vec<u8>)> {
        let mut stream = TcpStream::connect(&self.addr)
            .with_context(|| format!("connecting to opencode at {}", self.addr))?;
        stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
        let body = payload.map(|value| value.to_string()).unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.addr,
            self.auth,
            body.len()
        )?;
        let mut reader = BufReader::new(stream);
        let status = read_http_status(&mut reader)?;
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            if line == "\r\n" || line == "\n" {
                break;
            }
            line.clear();
        }
        let mut body = Vec::new();
        reader.read_to_end(&mut body)?;
        Ok((status, body))
    }

    fn open_sse(&self) -> Result<BufReader<TcpStream>> {
        let mut stream = TcpStream::connect(&self.addr)
            .with_context(|| format!("connecting to opencode at {}", self.addr))?;
        stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
        // A stalled socket must not keep evidence alive forever: the server emits periodic
        // `server.heartbeat` events (measured well inside a minute on 1.18.19), so a read that
        // sees NOTHING for the silence horizon — comfortably more than twice that cadence — is a
        // dead stream, surfaced as a disconnect (evidence off, reconnect and reseed).
        stream.set_read_timeout(Some(self.sse_silence))?;
        // HTTP/1.0 on purpose: over 1.1 the server chunk-encodes the stream (measured on
        // 1.18.19), which interleaves chunk-size lines into the line-oriented SSE read and can
        // split a `data:` line across chunks — a silently dropped event. Over 1.0 the same server
        // streams raw SSE bytes until close, which is exactly the framing this reader parses.
        write!(
            stream,
            "GET /event HTTP/1.0\r\nHost: {}\r\nAuthorization: Basic {}\r\nAccept: text/event-stream\r\n\r\n",
            self.addr, self.auth
        )?;
        let mut reader = BufReader::new(stream);
        let status = read_http_status(&mut reader)?;
        anyhow::ensure!(status == 200, "GET /event returned {status}");
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            if line == "\r\n" || line == "\n" {
                break;
            }
            line.clear();
        }
        Ok(reader)
    }
}

/// The SSE silence horizon: at least twice the measured `server.heartbeat` cadence, so a healthy
/// stream can never trip it while a stalled socket cannot outlive it.
const SSE_SILENCE: Duration = Duration::from_secs(120);

fn read_http_status(reader: &mut BufReader<TcpStream>) -> Result<u16> {
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("invalid HTTP status line {status_line:?}"))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let buffer = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let combined = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
        for position in 0..4 {
            if position <= chunk.len() {
                let index = ((combined >> (18 - 6 * position)) & 0x3f) as usize;
                out.push(ALPHABET[index] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---- SSE subscription ------------------------------------------------------------------------

enum SseMessage {
    Connected,
    Event(Value),
    Disconnected(DiagnosticReason),
}

fn spawn_sse_reader(client: Client, tx: Sender<SseMessage>, stop: std::sync::Arc<AtomicBool>) {
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match client.open_sse() {
                Ok(mut reader) => {
                    if tx.send(SseMessage::Connected).is_err() {
                        return;
                    }
                    let mut data = String::new();
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if let Some(chunk) = trimmed.strip_prefix("data:") {
                            data.push_str(chunk.trim_start());
                        } else if trimmed.is_empty() && !data.is_empty() {
                            if let Ok(event) = serde_json::from_str::<Value>(&data)
                                && tx.send(SseMessage::Event(event)).is_err()
                            {
                                return;
                            }
                            data.clear();
                        }
                    }
                    if tx
                        .send(SseMessage::Disconnected(DiagnosticReason::SseDisconnected))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => {
                    if tx
                        .send(SseMessage::Disconnected(DiagnosticReason::SseConnectFailed))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            thread::sleep(SSE_RECONNECT);
        }
    });
}

/// Re-seed observed state from the level surface after (re)connecting: events missed while
/// disconnected are unrecoverable, and `/session/status` omits idle sessions, so an empty map over
/// a live server is itself the idle proof.
type SeedFailure = (DiagnosticReason, DiagnosticSource);

fn seed_from_server(
    client: &Client,
    machine: &mut EventMachine,
) -> std::result::Result<(), SeedFailure> {
    // The seed is built in a FRESH machine and swapped in only once every read validates:
    // mutating the live one would leave half-seeded asks behind a mid-seed failure, and a
    // successful re-seed must also CLEAR stale busy/blocked entries whose exits passed while
    // the stream was down — the level surface at seed time is the whole truth, and any event
    // that raced the seed re-arrives or is re-listed on the next reconnect. Delivery targeting
    // deliberately learns nothing here: status-map iteration order is not recency (W8-5); the
    // pending-work recovery path resolves targets from the session listing instead.
    let statuses = client.get_json("/session/status").map_err(|_| {
        (
            DiagnosticReason::StatusUnavailable,
            DiagnosticSource::StatusSnapshot,
        )
    })?;
    // A response that is not the documented object shape proves nothing: seeding definite idle
    // from a null or an array would fabricate level evidence out of a shape this version cannot
    // read. Fail the seed and retry.
    let map = statuses.as_object().ok_or((
        DiagnosticReason::MalformedStatus,
        DiagnosticSource::StatusSnapshot,
    ))?;
    let mut seeded = EventMachine::default();
    seeded.seed_idle();
    for (session_id, status) in map {
        // Exactly the pinned vocabulary: an unknown future word is not "busy" — it is surface
        // drift the /doc gate vocabulary did not cover.
        match status.get("type").and_then(Value::as_str) {
            Some("idle") => {}
            Some("busy") => seeded.seed_busy(session_id.clone(), false),
            Some("retry") => seeded.seed_busy(session_id.clone(), true),
            _ => {
                return Err((
                    DiagnosticReason::UnknownStatus,
                    DiagnosticSource::StatusSnapshot,
                ));
            }
        }
    }
    // Both pending-ask listings must succeed for the seed to count: a transient failure must not
    // restore evidence on an unblocked picture and silently wedge an ask opened during the outage.
    for (endpoint, kind, unavailable, malformed, source) in [
        (
            "/permission",
            "permission",
            DiagnosticReason::PermissionUnavailable,
            DiagnosticReason::MalformedPermissions,
            DiagnosticSource::PermissionSnapshot,
        ),
        (
            "/question",
            "question",
            DiagnosticReason::QuestionUnavailable,
            DiagnosticReason::MalformedQuestions,
            DiagnosticSource::QuestionSnapshot,
        ),
    ] {
        let pending = client.get_json(endpoint).map_err(|_| (unavailable, source))?;
        let items = pending.as_array().ok_or((malformed, source))?;
        for item in items {
            // An unreadable id could never be released by its id-matched exit.
            let id = item
                .get("id")
                .or_else(|| item.get("requestID"))
                .and_then(Value::as_str)
                .ok_or((DiagnosticReason::MissingAskId, source))?;
            seeded.seed_ask(id.to_string(), kind);
        }
    }
    *machine = seeded;
    Ok(())
}

fn seed_with_diagnostics(
    client: &Client,
    machine: &mut EventMachine,
    diagnostics: &mut DiagnosticPublisher,
) -> bool {
    match seed_from_server(client, machine) {
        Ok(()) => {
            diagnostics.clear(DiagnosticStage::Seed);
            diagnostics.clear(DiagnosticStage::Sse);
            true
        }
        Err((reason, source)) => {
            diagnostics.publish(DiagnosticStage::Seed, reason, source);
            false
        }
    }
}

// ---- event projection ------------------------------------------------------------------------

/// The pure projection from OpenCode's event stream to one seat-level observation. A dedicated
/// seat aggregates across the server's sessions: any busy session is activity, any open
/// permission or question is a human block, and idle is only derived from positive level evidence.
#[derive(Default)]
struct EventMachine {
    /// sessionID → currently retrying (vs plainly busy).
    busy: BTreeMap<String, bool>,
    /// permission/question id → what kind of human ask holds it open.
    blocked: BTreeMap<String, &'static str>,
    /// Level evidence seen: idle is a proof, never a default.
    seen_level: bool,
    /// A tracked-busy session moved to a status word this version cannot read: every projection
    /// is withheld until a fresh level seed replaces this machine.
    poisoned: bool,
    /// Terminal reason, once observed.
    ended: Option<&'static str>,
    /// The most recent non-terminal session error, surfaced as the idle reason once.
    last_error: Option<String>,
}

impl EventMachine {
    fn seed_idle(&mut self) {
        self.seen_level = true;
    }

    fn seed_busy(&mut self, session_id: String, retry: bool) {
        self.seen_level = true;
        self.busy.insert(session_id, retry);
    }

    /// Re-enter a human ask found pending at connect time, under its own id so the ordinary
    /// id-matched exit event releases it.
    fn seed_ask(&mut self, id: String, kind: &'static str) {
        self.blocked.insert(id, kind);
    }

    fn apply(&mut self, event: &Value) {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return;
        };
        let properties = event.get("properties").unwrap_or(&Value::Null);
        let session_id = || {
            properties
                .get("sessionID")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match kind {
            "session.status" => {
                let Some(session_id) = session_id() else {
                    return;
                };
                match properties
                    .pointer("/status/type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                {
                    "busy" => {
                        self.seen_level = true;
                        self.busy.insert(session_id, false);
                        self.last_error = None;
                    }
                    "retry" => {
                        self.seen_level = true;
                        self.busy.insert(session_id, true);
                    }
                    "idle" => {
                        self.seen_level = true;
                        self.busy.remove(&session_id);
                    }
                    // A future status arm is not evidence of anything — not even level evidence:
                    // counting it would let an unrecognized word prove `idle` on a quiet server.
                    // It poisons the whole projection on ANY session, tracked or not: a
                    // tracked-busy entry can no longer be trusted to clear, and an untracked
                    // session in a state this version cannot read makes standing idle evidence
                    // a fabrication — that session may already be mid-turn. Withholding until
                    // a level seed rebuilds the picture is the only honest reading.
                    _ => {
                        self.poisoned = true;
                    }
                }
            }
            "session.idle" => {
                if let Some(session_id) = session_id() {
                    self.seen_level = true;
                    self.busy.remove(&session_id);
                }
            }
            "session.error" => {
                let name = properties
                    .pointer("/error/name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if name == "ProviderAuthError" {
                    self.ended = Some("providerAuth");
                } else {
                    if let Some(session_id) = session_id() {
                        self.busy.remove(&session_id);
                    }
                    self.seen_level = true;
                    self.last_error = Some(format!("error:{name}"));
                }
            }
            "permission.asked" => {
                if let Some(id) = ask_id(properties) {
                    self.blocked.insert(id, "permission");
                }
            }
            "permission.replied" => {
                if let Some(id) = ask_id(properties) {
                    self.blocked.remove(&id);
                }
            }
            "question.asked" => {
                if let Some(id) = ask_id(properties) {
                    self.blocked.insert(id, "question");
                }
            }
            "question.replied" | "question.rejected" => {
                if let Some(id) = ask_id(properties) {
                    self.blocked.remove(&id);
                }
            }
            // server.connected, server.heartbeat, plugin.added replay, message.*, … — not state.
            _ => {}
        }
    }

    fn observation(&self) -> Option<Observation> {
        // A sticky terminal outranks poison: `ended` does not depend on the busy map the
        // unknown word made untrustworthy, and withholding it would lose the terminal to the
        // forced reseed's fresh machine.
        if let Some(reason) = self.ended {
            return Some(
                Observation::new(Activity::Ended, BlockedOn::None, InputBuffer::Unknown)
                    .with_reason(reason),
            );
        }
        if self.poisoned {
            return None;
        }
        if let Some(kind) = self.blocked.values().next() {
            let ask = match *kind {
                "question" => Ask::Question,
                _ => Ask::Permission,
            };
            return Some(
                Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
                    .with_ask(ask)
                    .with_reason(*kind),
            );
        }
        if !self.busy.is_empty() {
            let observation =
                Observation::new(Activity::Active, BlockedOn::None, InputBuffer::Unknown);
            return Some(if self.busy.values().all(|retry| *retry) {
                observation.with_reason("retry")
            } else {
                observation
            });
        }
        if self.seen_level {
            let observation =
                Observation::new(Activity::Idle, BlockedOn::None, InputBuffer::Unknown);
            return Some(match &self.last_error {
                Some(reason) => observation.with_reason(reason.clone()),
                None => observation,
            });
        }
        None
    }
}

/// The most recently updated session id from `GET /session`, or the last listed when the entries
/// carry no readable update time. `None` while the seat's TUI has not created a session yet.
fn newest_listed_session(client: &Client) -> Option<String> {
    let listed = client.get_json("/session").ok()?;
    let sessions = listed.as_array()?;
    sessions
        .iter()
        .max_by_key(|session| {
            session
                .pointer("/time/updated")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        })
        .or_else(|| sessions.last())
        .and_then(|session| session.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

/// A permission/question id, wherever this version of the event nests it. Measured on 1.18.19:
/// `permission.asked`/`question.asked` carry `/id`, while their `*.replied`/`*.rejected` exits
/// carry `/requestID` — missing that spelling would hold `blockedOn: human` forever after a grant.
fn ask_id(properties: &Value) -> Option<String> {
    for pointer in ["/id", "/requestID", "/permission/id", "/question/id"] {
        if let Some(id) = properties.pointer(pointer).and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    None
}

fn event_session_id(event: &Value) -> Option<&str> {
    let kind = event.get("type").and_then(Value::as_str)?;
    if !kind.starts_with("session.") {
        return None;
    }
    event
        .pointer("/properties/sessionID")
        .and_then(Value::as_str)
}

// ---- harness context (HC-R02, HC-R11, HC-R12) --------------------------------------------------

/// How long a fruitless `GET /config/providers` waits before another attempt. Without a backoff a
/// model the providers document does not name would re-pull the whole document once per
/// [`PROVIDER_POLL`] for the life of the session.
const PROVIDERS_RETRY: Duration = Duration::from_secs(60);
/// How long a providers document that ANSWERED but does not name the model waits. A custom or
/// local provider can be absent from the config surface for the life of the session, and re-pulling
/// the whole document every minute for it would be the same busy loop one step slower. A model
/// change resets the clock and asks again at once, so nothing is lost by waiting; the horizon
/// matches the API gate's own terminal arm.
const PROVIDERS_UNLISTED_RETRY: Duration = Duration::from_secs(3600);

/// The numeric axis for an OpenCode seat (HC-R02, HC-R11): the one producer whose numerator is
/// pushed and whose denominator has to be pulled.
///
/// - **Numerator**: `tokens.total` of the last **non-summary assistant** `message.updated`. Two
///   measured traps live here. `summary` is overloaded — on user messages it is an object
///   (`{diffs: []}`) and therefore truthy — so the filter reads it as a boolean and requires the
///   assistant role. And the compaction summarizer's own message is an assistant message whose
///   `tokens.total` is the cost of the summarization call (1,511 measured), not the size of the
///   new context; taking it would report a freshly compacted window as nearly empty.
/// - **Denominator**: `providers[].models[<modelID>].limit.context` from `GET /config/providers`,
///   pulled on demand and cached per `providerID/modelID`. While it is unknown both the window and
///   the percent are withheld (HC-R02), never guessed from a model table.
/// - **`usedPercent`**: st2's own division, unrounded and unclamped — OpenCode displays no
///   percentage of its own, so there is no harness number to carry. Rounding would put the written
///   value in a different 1% bucket than the truth and make
///   [`harness_context::HARNESS_CONTEXT_WARN_PERCENT`] fire below the threshold it names.
/// - **`sessionTotalTokens`**: the sum of `session.info.tokens`, which carries no `total` key and
///   is cumulative over the session's lifetime. It grows without bound and is never the numerator
///   of the percent — the field's name is that guard.
///
/// The state lives here rather than on [`EventMachine`], which is rebuilt from scratch on every SSE
/// reconnect: a window cache and a numerator hung off it would be discarded by a blip. A reconnect
/// deliberately does not disturb this producer — a token count stays true while its record ages,
/// which is the whole of HC-R06.
///
/// Which session's reading this is, when one server holds several, is `DQ-C10`: v1 is
/// last-writer-wins over every session on the stream, matching the categorical producer's
/// aggregate rather than inventing a seat-session rule the open question exists to defer.
struct ContextProducer {
    writer: harness_context::Writer,
    /// `providerID/modelID` → `limit.context`.
    windows: BTreeMap<String, u64>,
    /// The model that produced the current numerator, in OpenCode's own `providerID/modelID`
    /// spelling — `modelID` alone is ambiguous across providers, and this is the key the window is
    /// looked up under, so the record always pairs a numerator with its own denominator.
    model: Option<String>,
    /// The session's configured model, which can change before any assistant message proves it.
    /// It only warms the window cache; it never becomes the reading's model.
    configured_model: Option<String>,
    used_tokens: Option<u64>,
    cost_usd: Option<f64>,
    session_total_tokens: Option<u64>,
    next_providers_attempt: Instant,
}

impl ContextProducer {
    fn new(agent_dir: &Path, identity: &str, session: &str) -> Result<Self> {
        Ok(Self {
            writer: harness_context::Writer::new(
                agent_dir,
                identity,
                harness_context::Harness::OpenCode,
            )?
            .with_session(session),
            windows: BTreeMap::new(),
            model: None,
            configured_model: None,
            used_tokens: None,
            cost_usd: None,
            session_total_tokens: None,
            next_providers_attempt: Instant::now(),
        })
    }

    /// Fold one SSE frame in, returning whether it carried a fresh occupancy numerator.
    ///
    /// A compaction edge writes immediately: it is rare, it always lands, and it deliberately
    /// carries no reading — the numbers already on the record predate the compaction, and
    /// re-stamping them would hide that. Nothing zeroes the numerator on the edge either
    /// (HC-R03); the next turn's `message.updated` replaces it with an observation.
    fn apply(&mut self, event: &Value) -> bool {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return false;
        };
        let properties = event.get("properties").unwrap_or(&Value::Null);
        let info = || properties.get("info").unwrap_or(&Value::Null);
        match kind {
            "message.updated" => self.apply_message(info()),
            "session.updated" => {
                self.apply_session(info());
                false
            }
            // `{sessionID}` and nothing else — no reason, no timestamp, no before/after sizes — so
            // the trigger is `unknown` and the count is st2's, scoped to this incarnation. The
            // richer v2 `session.next.compaction.*` events do carry a reason, but did not fire once
            // on the measured legacy path; nothing here depends on them.
            "session.compacted" => {
                if let Err(error) = self.writer.compacted(harness_context::Compaction::new(
                    harness_context::CompactionTrigger::Unknown,
                )) {
                    tracing::warn!(
                        "st2 opencode-session: recording a compaction failed: {error:#}"
                    );
                }
                false
            }
            _ => false,
        }
    }

    fn apply_message(&mut self, info: &Value) -> bool {
        if info.get("role").and_then(Value::as_str) != Some("assistant") {
            return false;
        }
        // `summary` is a BOOLEAN on assistant messages and an object on user messages; `as_bool`
        // reads only the former, so `{diffs: []}` can never be mistaken for a compaction summary.
        if info.get("summary").and_then(Value::as_bool) == Some(true) {
            return false;
        }
        // The key's PRESENCE is the proof, not its value: the first `message.updated` of a message
        // carries all-zero token fields and no `total` at all, and reading a missing key as zero
        // would publish an empty window at the start of every turn.
        let Some(total) = info.pointer("/tokens/total").and_then(Value::as_u64) else {
            return false;
        };
        let (Some(provider), Some(model)) = (
            info.get("providerID").and_then(Value::as_str),
            info.get("modelID").and_then(Value::as_str),
        ) else {
            return false;
        };
        self.used_tokens = Some(total);
        self.model = Some(model_key(provider, model));
        true
    }

    /// The adjacent facts (HC-R16). None of them is a reading: this frame never publishes, and its
    /// values ride out on the next numerator instead. Publishing here would stamp `observedAtMs` on
    /// a numerator that may be hours old — the one thing [`harness_context::Writer::observe`]
    /// forbids its callers.
    fn apply_session(&mut self, info: &Value) {
        if let Some(cost) = info.get("cost").and_then(Value::as_f64) {
            self.cost_usd = Some(cost);
        }
        if let Some(tokens) = info.get("tokens")
            && let Some(total) = cumulative_tokens(tokens)
        {
            self.session_total_tokens = Some(total);
        }
        if let (Some(provider), Some(model)) = (
            info.pointer("/model/providerID").and_then(Value::as_str),
            info.pointer("/model/id").and_then(Value::as_str),
        ) {
            let key = model_key(provider, model);
            if self.configured_model.as_deref() != Some(key.as_str()) {
                // A model change re-opens the denominator question before any assistant message
                // proves the new model, so the cache is warmed now rather than the percent being
                // withheld for a whole turn.
                self.configured_model = Some(key);
                self.next_providers_attempt = Instant::now();
            }
        }
    }

    /// A model whose window is not cached yet, if any.
    fn window_wanted(&self) -> Option<&str> {
        [self.model.as_deref(), self.configured_model.as_deref()]
            .into_iter()
            .flatten()
            .find(|key| !self.windows.contains_key(*key))
    }

    /// Pull the denominator when a window this producer needs is missing, at most once per
    /// [`PROVIDERS_RETRY`]. The only HTTP this producer does.
    fn refresh_windows(&mut self, client: &Client, api_ok: bool) {
        if !api_ok || self.window_wanted().is_none() {
            return;
        }
        if Instant::now() < self.next_providers_attempt {
            return;
        }
        self.next_providers_attempt = Instant::now() + PROVIDERS_RETRY;
        match client.get_json("/config/providers") {
            Ok(doc) => {
                self.ingest_providers(&doc);
                if self.window_wanted().is_some() {
                    self.next_providers_attempt = Instant::now() + PROVIDERS_UNLISTED_RETRY;
                }
            }
            Err(error) => {
                tracing::warn!("st2 opencode-session: reading model windows failed: {error:#}");
            }
        }
    }

    /// The pure half of that pull, so the join is provable from a captured document.
    fn ingest_providers(&mut self, doc: &Value) {
        let Some(providers) = doc.get("providers").and_then(Value::as_array) else {
            return;
        };
        for provider in providers {
            let Some(provider_id) = provider.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            for (model_id, model) in models {
                if let Some(context) = model.pointer("/limit/context").and_then(Value::as_u64) {
                    self.windows
                        .insert(model_key(provider_id, model_id), context);
                }
            }
        }
    }

    fn reading(&self) -> harness_context::Reading {
        let window = self
            .model
            .as_deref()
            .and_then(|key| self.windows.get(key))
            .copied();
        harness_context::Reading {
            used_tokens: self.used_tokens,
            window_tokens: window,
            used_percent: match (self.used_tokens, window) {
                (Some(used), Some(window)) if window > 0 => {
                    Some(used as f64 / window as f64 * 100.0)
                }
                _ => None,
            },
            model: self.model.clone(),
            cost_usd: self.cost_usd,
            session_total_tokens: self.session_total_tokens,
            rate_limits: harness_context::RateLimits::default(),
        }
    }

    /// Publish the current reading, returning whether a write landed — the core's quantization is
    /// what bounds a stream this chatty to one write per 1% of the window.
    ///
    /// Called only where a fresh numerator just arrived. This producer's numerator is pushed and
    /// not pullable, so it never holds a reading to heartbeat with: a quiet seat's record ages
    /// visibly instead, and a reader gets the numbers it has with their age (HC-R06) — which stay
    /// true, because a window taking no turn does not fill.
    fn publish(&mut self) -> bool {
        match self.writer.observe(self.reading()) {
            Ok(landed) => landed,
            Err(error) => {
                tracing::warn!(
                    "st2 opencode-session: writing the context record failed: {error:#}"
                );
                false
            }
        }
    }
}

/// OpenCode's own spelling for a model, and the key a window is cached under.
fn model_key(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}/{model_id}")
}

/// The cumulative session total: `session.info.tokens` carries no `total` key, so the producer sums
/// the leaves it does carry. Never occupancy — across the measured two-turn run this reached 16,400
/// while occupancy stayed at 8,200.
fn cumulative_tokens(tokens: &Value) -> Option<u64> {
    let leaf = |pointer: &str| tokens.pointer(pointer).and_then(Value::as_u64);
    let mut total = leaf("/input")?;
    for pointer in ["/output", "/reasoning", "/cache/read", "/cache/write"] {
        total = total.saturating_add(leaf(pointer).unwrap_or(0));
    }
    Some(total)
}

// ---- native delivery -------------------------------------------------------------------------

/// What `GET /session/{session}/message/{message}` proved. A storage receipt, graded to
/// [`delivery_ledger::Phase::Persisted`]: it says the server holds the exact client message, never
/// that anything read it.
#[derive(Clone, Copy)]
enum ReadBack {
    Durable,
    Absent,
    Indeterminate,
}

struct Delivery {
    catalog_root: PathBuf,
    inbox: PathBuf,
    status_path: PathBuf,
    this_host: String,
    identity: String,
    ledger: delivery_ledger::Ledger,
    /// The session a new delivery binds to: the most recently observed one.
    target_session: Option<String>,
    next_attempt: Instant,
}

impl Delivery {
    fn new(
        catalog_root: &Path,
        agent_dir: &Path,
        this_host: &str,
        identity: &str,
        runtime_id: &str,
    ) -> Self {
        let ledger_path = state_dir(catalog_root, identity).join(delivery_ledger::LEDGER_FILE);
        Self::with_state_path(
            catalog_root,
            agent_dir,
            this_host,
            identity,
            runtime_id,
            ledger_path,
        )
    }

    fn with_state_path(
        catalog_root: &Path,
        agent_dir: &Path,
        this_host: &str,
        identity: &str,
        runtime_id: &str,
        ledger_path: PathBuf,
    ) -> Self {
        let owner = identity.to_string();
        // The same derivation the transport uses, so a record whose messageID does not match its
        // own session and filename is provably not this agent's and fails closed.
        let ledger = delivery_ledger::Ledger::open(
            &ledger_path,
            delivery_ledger::Harness::OpenCode.profile(),
            identity,
            runtime_id,
            |session, filename| stable_message_id(&owner, session, filename),
        );
        Self {
            catalog_root: catalog_root.to_path_buf(),
            inbox: message::inbox_dir(agent_dir),
            status_path: status::status_path(agent_dir),
            this_host: this_host.to_string(),
            identity: identity.to_string(),
            ledger,
            target_session: None,
            next_attempt: Instant::now(),
        }
    }

    fn saw_session(&mut self, session_id: &str) {
        self.target_session = Some(session_id.to_string());
    }

    #[cfg(test)]
    fn pump(&mut self, client: &Client) {
        self.pump_with_diagnostics(client, None);
    }

    fn pump_diagnosed(&mut self, client: &Client, diagnostics: &mut DiagnosticPublisher) {
        self.pump_with_diagnostics(client, Some(diagnostics));
    }

    fn pump_with_diagnostics(
        &mut self,
        client: &Client,
        diagnostics: Option<&mut DiagnosticPublisher>,
    ) {
        if let Err(error) = self.pump_inner(client, diagnostics) {
            tracing::warn!("st2 opencode-session: delivery: {error:#}");
        }
    }

    fn pump_inner(
        &mut self,
        client: &Client,
        mut diagnostics: Option<&mut DiagnosticPublisher>,
    ) -> Result<()> {
        let unread = message::list_inbox(&self.inbox)?;
        // Archive is the recipient agent's act and the only settlement authority. An entry whose
        // file left the inbox releases ownership here; this pump never moves a file itself.
        self.ledger
            .prune(|filename| unread.iter().any(|entry| entry.filename == filename))?;
        if status::read_state(&self.status_path) == status::State::Dnd {
            return Ok(());
        }
        let Some(head) = unread.into_iter().next() else {
            if let Some(diagnostics) = diagnostics.as_deref_mut() {
                diagnostics.clear(DiagnosticStage::Delivery);
                diagnostics.clear(DiagnosticStage::ReadBack);
            }
            return Ok(());
        };
        let target = match self.target_session.clone() {
            Some(target) => target,
            None => {
                // A session that settled before this observer connected is invisible to both the
                // event stream and `/session/status` (idle sessions are omitted), so a pending
                // delivery would otherwise stall forever. With work waiting, recover the binding
                // from the session listing — retried every pump pass until a session exists.
                let Some(recovered) = newest_listed_session(client) else {
                    return Ok(());
                };
                self.saw_session(&recovered);
                recovered
            }
        };
        // A newly selected session is a different delivery binding (the Codex thread rule): the
        // old binding's receipt may neither suppress nor acknowledge delivery to this one.
        if self.ledger.binding().is_some_and(|binding| binding != target) {
            self.ledger.rebind(&target)?;
        }
        // Fail closed. An unreadable ledger holds and surfaces instead of guessing, and it never
        // refused to start: a driver that will not start delivers nothing at all. The
        // operator-visible surface is the existing typed boundary — the transport is unavailable —
        // and the raw reason stays in tracing, so no unbounded prose reaches the record.
        if let Some(reason) = self.ledger.quarantined() {
            tracing::warn!("st2 opencode-session: delivery ledger is quarantined: {reason}");
            if let Some(diagnostics) = diagnostics.as_deref_mut() {
                diagnostics.publish(
                    DiagnosticStage::Delivery,
                    DiagnosticReason::DeliveryUnavailable,
                    DiagnosticSource::PromptTransport,
                );
            }
            return Ok(());
        }
        if let Some(entry) = self.ledger.entry(&head.filename).cloned() {
            // Storage is a receipt, not consumption: it stops the POST loop and holds the inbox
            // entry, and only prompt admission would release ownership.
            if entry.phase >= delivery_ledger::Phase::Persisted {
                if let Some(diagnostics) = diagnostics.as_deref_mut() {
                    diagnostics.clear(DiagnosticStage::Delivery);
                    diagnostics.clear(DiagnosticStage::ReadBack);
                }
                return Ok(());
            }
            return self.reconcile_or_retry(client, entry, diagnostics);
        }
        if !self.ledger.entries().is_empty() {
            return Ok(()); // The bound message is behind the head; archive precedence resolves it.
        }

        let message_id = stable_message_id(&self.identity, &target, &head.filename);
        let entry = self.ledger.begin(delivery_ledger::Begin {
            filename: head.filename.clone(),
            binding: target.clone(),
            correlation: delivery_ledger::Correlation::native(message_id.clone()),
            // Read-back is a durable query, not a live frame, so a pre-crash attempt is
            // reconcilable without an incarnation token.
            incarnation: None,
        })?;
        let text = ding::poke_text(&self.catalog_root, &self.this_host, &self.identity, &head);
        self.send(client, &entry, &text, diagnostics)
    }

    fn reconcile_or_retry(
        &mut self,
        client: &Client,
        entry: delivery_ledger::Entry,
        mut diagnostics: Option<&mut DiagnosticPublisher>,
    ) -> Result<()> {
        let read_back = self.read_back(client, &entry);
        report_read_back(read_back, &mut diagnostics);
        match read_back {
            ReadBack::Durable => {
                self.ledger
                    .record(&entry.filename, delivery_ledger::Evidence::Persisted)?;
                return Ok(());
            }
            // Measured on 1.18.19: a second POST with the same messageID appends its parts again
            // into the same message, so an indeterminate read-back must never trigger a resend —
            // the read-back itself is retried on a later pass.
            ReadBack::Indeterminate => return Ok(()),
            // A 404 for the exact client message is an authoritative absence: the only receipt
            // that may authorize another POST of the same identity.
            ReadBack::Absent => {
                self.ledger
                    .negative(&entry.filename, delivery_ledger::NegativeReceipt::Absent)?;
            }
        }
        if self.ledger.retry(&entry.filename) != delivery_ledger::RetryDecision::Retry {
            return Ok(());
        }
        if Instant::now() < self.next_attempt {
            return Ok(());
        }
        let unread = message::list_inbox(&self.inbox)?;
        let Some(head) = unread
            .into_iter()
            .find(|message| message.filename == entry.filename)
        else {
            return Ok(());
        };
        let text = ding::poke_text(&self.catalog_root, &self.this_host, &self.identity, &head);
        self.send(client, &entry, &text, diagnostics)
    }

    fn send(
        &mut self,
        client: &Client,
        entry: &delivery_ledger::Entry,
        text: &str,
        mut diagnostics: Option<&mut DiagnosticPublisher>,
    ) -> Result<()> {
        self.next_attempt = Instant::now() + DELIVERY_RETRY;
        let payload = json!({
            "messageID": entry.correlation.value,
            "parts": [{ "type": "text", "text": text }],
        });
        let path = format!("/session/{}/prompt_async", entry.binding);
        let status = match client.post_json(&path, &payload) {
            Ok(status) => status,
            Err(error) => {
                if let Some(diagnostics) = diagnostics.as_deref_mut() {
                    diagnostics.publish(
                        DiagnosticStage::Delivery,
                        DiagnosticReason::DeliveryUnavailable,
                        DiagnosticSource::PromptTransport,
                    );
                }
                return Err(error);
            }
        };
        if !(200..300).contains(&status) {
            if let Some(diagnostics) = diagnostics.as_deref_mut() {
                diagnostics.publish(
                    DiagnosticStage::Delivery,
                    DiagnosticReason::DeliveryRejected,
                    DiagnosticSource::PromptTransport,
                );
            }
            anyhow::bail!("POST {path} returned {status}");
        }
        // The transport call succeeded. That is a fact about the call, not about the server's
        // state, so it grades no higher than `transportAccepted`.
        self.ledger
            .record(&entry.filename, delivery_ledger::Evidence::TransportAccepted)?;
        if let Some(diagnostics) = diagnostics.as_deref_mut() {
            diagnostics.clear(DiagnosticStage::Delivery);
        }
        let read_back = self.read_back(client, entry);
        report_read_back(read_back, &mut diagnostics);
        if matches!(read_back, ReadBack::Durable) {
            self.ledger
                .record(&entry.filename, delivery_ledger::Evidence::Persisted)?;
        }
        Ok(())
    }

    /// The only receipt this transport accepts: the exact client message read back durably.
    fn read_back(&self, client: &Client, entry: &delivery_ledger::Entry) -> ReadBack {
        let path = format!(
            "/session/{}/message/{}",
            entry.binding, entry.correlation.value
        );
        match client.status_of_get(&path) {
            Ok(200) => ReadBack::Durable,
            Ok(404) => ReadBack::Absent,
            // A 5xx or a transport error proves nothing about the message either way.
            Ok(_) | Err(_) => ReadBack::Indeterminate,
        }
    }
}

fn report_read_back(
    read_back: ReadBack,
    diagnostics: &mut Option<&mut DiagnosticPublisher>,
) {
    let Some(diagnostics) = diagnostics.as_deref_mut() else {
        return;
    };
    match read_back {
        ReadBack::Durable => {
            diagnostics.clear(DiagnosticStage::ReadBack);
            diagnostics.clear(DiagnosticStage::Delivery);
        }
        ReadBack::Absent => diagnostics.publish(
            DiagnosticStage::ReadBack,
            DiagnosticReason::NotDurable,
            DiagnosticSource::MessageReadBack,
        ),
        ReadBack::Indeterminate => diagnostics.publish(
            DiagnosticStage::ReadBack,
            DiagnosticReason::ReadBackUnavailable,
            DiagnosticSource::MessageReadBack,
        ),
    }
}

fn stable_message_id(recipient: &str, session_id: &str, filename: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"st2.opencode-client-message.v1");
    for value in [
        recipient.as_bytes(),
        session_id.as_bytes(),
        filename.as_bytes(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    // OpenCode message ids match `^msg`; a hex tail keeps the identity stable and grammatical.
    format!("msg{:.26}", format!("{:x}", hash.finalize()))
}

pub fn state_dir(catalog_root: &Path, identity: &str) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut hash = Sha256::new();
    for value in [
        catalog_root.as_os_str().as_encoded_bytes(),
        identity.as_bytes(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    let digest = format!("{:x}", hash.finalize());
    base.join("st2").join("opencode").join(&digest[..24])
}

#[cfg(test)]
mod tests {

    /// Pins the admitted set itself: widening opencode has to be a deliberate edit here, beside
    /// the verification the minor was admitted on.
    #[test]
    fn admitted_opencode_minors_are_exactly_the_measured_set() {
        assert_eq!(SUPPORTED_OPENCODE_MINORS, [(1, 18)]);
    }

    /// The principal's actual case: the profile ships 1.18.25 against a list built at 1.18.19.
    /// A patch inside the verified minor must keep native delivery rather than degrade to none.
    #[test]
    fn a_patch_inside_an_admitted_opencode_minor_keeps_native_delivery() {
        for version in ["1.18.19", "1.18.25"] {
            assert!(
                version_is_supported(version),
                "{version} is inside verified minor 1.18"
            );
        }
    }

    /// Fail closed the other way: a different minor, a neighbouring minor sharing a prefix, and
    /// unparseable output must all leave native delivery disabled.
    #[test]
    fn other_minors_and_garbled_versions_stay_unverified() {
        for version in ["1.19.0", "1.180.0", "2.18.0", "1.18", "not-a-version", ""] {
            assert!(
                !version_is_supported(version),
                "{version} must not be treated as verified"
            );
        }
    }
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn event(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    fn observed(machine: &EventMachine) -> Observation {
        machine.observation().expect("an observation")
    }

    #[test]
    fn no_evidence_yields_no_observation_so_nothing_is_written() {
        let mut machine = EventMachine::default();
        assert_eq!(machine.observation(), None);
        // Replay noise on connect is not evidence either.
        for noise in [
            r#"{"type":"server.connected","properties":{}}"#,
            r#"{"type":"server.heartbeat","properties":{}}"#,
            r#"{"type":"plugin.added","properties":{"name":"x"}}"#,
            r#"{"type":"message.updated","properties":{"sessionID":"ses_a"}}"#,
        ] {
            machine.apply(&event(noise));
        }
        assert_eq!(machine.observation(), None);
    }

    #[test]
    fn session_status_drives_active_and_idle_across_sessions() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"busy"}}}"#,
        ));
        assert_eq!(observed(&machine).state, Activity::Active);

        // A second idle session does not mask the busy one (aggregate rule)…
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_b","status":{"type":"idle"}}}"#,
        ));
        assert_eq!(observed(&machine).state, Activity::Active);

        // …and duplicates are idempotent.
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"busy"}}}"#,
        ));
        machine.apply(&event(
            r#"{"type":"session.idle","properties":{"sessionID":"ses_a"}}"#,
        ));
        let idle = observed(&machine);
        assert_eq!(idle.state, Activity::Idle);
        assert_eq!(idle.blocked_on, BlockedOn::None);
    }

    #[test]
    fn retry_reads_active_with_a_reason() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"retry","attempt":2}}}"#,
        ));
        let retrying = observed(&machine);
        assert_eq!(retrying.state, Activity::Active);
        assert_eq!(retrying.reason.as_deref(), Some("retry"));
    }

    #[test]
    fn permission_blocks_until_the_same_id_is_replied() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"busy"}}}"#,
        ));
        machine.apply(&event(
            r#"{"type":"permission.asked","properties":{"id":"per_1","sessionID":"ses_a"}}"#,
        ));
        let blocked = observed(&machine);
        assert_eq!(blocked.state, Activity::Active);
        assert_eq!(blocked.blocked_on, BlockedOn::Human);
        assert_eq!(blocked.ask, Ask::Permission);
        assert_eq!(blocked.reason.as_deref(), Some("permission"));

        // A different id resolving is not this ask's exit edge. Replies spell the id `requestID`
        // on the measured wire.
        machine.apply(&event(
            r#"{"type":"permission.replied","properties":{"requestID":"per_other"}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::Human);

        machine.apply(&event(
            r#"{"type":"permission.replied","properties":{"requestID":"per_1"}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::None);
        assert_eq!(observed(&machine).state, Activity::Active);
    }

    /// The verbatim event pair captured live from opencode 1.18.19 (isolated server, config-file
    /// `"permission":{"bash":"ask"}`, free model): entry carries `properties.id`, the grant carries
    /// `properties.requestID`. A vocabulary drift on either side must fail here, not in the field.
    #[test]
    fn captured_permission_grant_pair_enters_and_exits_blocked() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"id":"evt_02fdc8e3f001djGGTdLwNiVJce","type":"session.status","properties":{"sessionID":"ses_fd0241376ffe3KDznnEB55qvKi","status":{"type":"busy"}}}"#,
        ));
        machine.apply(&event(
            r#"{"id":"evt_02fdc246b0020Xw65txB3nXBC4","type":"permission.asked","properties":{"id":"per_02fdc246b001BB5pclAd62tzpJ","sessionID":"ses_fd0241376ffe3KDznnEB55qvKi","permission":"bash","patterns":["echo capture-test-42"],"metadata":{"command":"echo capture-test-42"},"always":["echo *"],"tool":{"messageID":"msg_02fdc0989001nfz93uTCTLeO6O","callID":"call_6614fd927fe74d86ab089078"}}}"#,
        ));
        let blocked = observed(&machine);
        assert_eq!(blocked.state, Activity::Active);
        assert_eq!(blocked.blocked_on, BlockedOn::Human);
        assert_eq!(blocked.ask, Ask::Permission);
        assert_eq!(blocked.reason.as_deref(), Some("permission"));

        machine.apply(&event(
            r#"{"id":"evt_02fdc8342001TQBwhszchZw1U6","type":"permission.replied","properties":{"sessionID":"ses_fd0241376ffe3KDznnEB55qvKi","requestID":"per_02fdc246b001BB5pclAd62tzpJ","reply":"once"}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::None);
    }

    /// The verbatim question pair captured in the same run: same id/requestID asymmetry.
    #[test]
    fn captured_question_reply_pair_enters_and_exits_blocked() {
        let mut machine = EventMachine::default();
        machine.seed_idle();
        machine.apply(&event(
            r#"{"type":"question.asked","properties":{"id":"que_02fdd3e83001GwptE1fgJam0jB","sessionID":"ses_fd0241376ffe3KDznnEB55qvKi","questions":[{"question":"Do you want me to continue helping with a coding or shell task in this workspace?","header":"Next step","options":[{"label":"Yes","description":"You have a follow-up task you will describe next"},{"label":"No","description":"No further action needed for now"}],"multiple":true}],"tool":{"messageID":"msg_02fdd2672001Mr1YBNCe4YM5Ro","callID":"call_59b5baf00c9f4e248d48f04b"}}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::Human);
        assert_eq!(observed(&machine).reason.as_deref(), Some("question"));

        machine.apply(&event(
            r#"{"type":"question.replied","properties":{"sessionID":"ses_fd0241376ffe3KDznnEB55qvKi","requestID":"que_02fdd3e83001GwptE1fgJam0jB","answers":[["Yes"]]}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::None);
    }

    #[test]
    fn questions_block_like_permissions_and_rejection_releases() {
        let mut machine = EventMachine::default();
        machine.seed_idle();
        machine.apply(&event(
            r#"{"type":"question.asked","properties":{"id":"que_1","sessionID":"ses_a"}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::Human);
        assert_eq!(observed(&machine).reason.as_deref(), Some("question"));
        machine.apply(&event(
            r#"{"type":"question.rejected","properties":{"requestID":"que_1"}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::None);
    }

    #[test]
    fn provider_auth_error_is_terminal_and_other_errors_settle_to_idle_with_a_reason() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"busy"}}}"#,
        ));
        machine.apply(&event(
            r#"{"type":"session.error","properties":{"sessionID":"ses_a","error":{"name":"MessageAbortedError"}}}"#,
        ));
        let idle = observed(&machine);
        assert_eq!(idle.state, Activity::Idle);
        assert_eq!(idle.reason.as_deref(), Some("error:MessageAbortedError"));

        machine.apply(&event(
            r#"{"type":"session.error","properties":{"sessionID":"ses_a","error":{"name":"ProviderAuthError"}}}"#,
        ));
        let ended = observed(&machine);
        assert_eq!(ended.state, Activity::Ended);
        assert_eq!(ended.reason.as_deref(), Some("providerAuth"));
    }

    #[test]
    fn openapi_gate_names_every_missing_marker() {
        let complete: Value = serde_json::from_str(&format!(
            r#"{{"paths":{{}},"markers":{:?}}}"#,
            REQUIRED_API_MARKERS
        ))
        .unwrap();
        check_openapi_subset(&complete).unwrap();

        let error = check_openapi_subset(&serde_json::json!({"paths": {"/session": {}}}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("prompt_async"), "{error}");
        assert!(error.contains("permission.asked"), "{error}");
    }

    #[test]
    fn stable_message_ids_are_grammatical_and_distinct_per_binding() {
        let id = stable_message_id("h.worker", "ses_a", "1786380000000-abc123.md");
        assert!(id.starts_with("msg"));
        assert_eq!(
            id,
            stable_message_id("h.worker", "ses_a", "1786380000000-abc123.md")
        );
        for other in [
            stable_message_id("h.other", "ses_a", "1786380000000-abc123.md"),
            stable_message_id("h.worker", "ses_b", "1786380000000-abc123.md"),
            stable_message_id("h.worker", "ses_a", "1786380000000-def456.md"),
        ] {
            assert_ne!(id, other);
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"opencode:pw", "b3BlbmNvZGU6cHc="),
        ] {
            assert_eq!(base64(input), expected, "{input:?}");
        }
    }

    // ---- fake-server delivery tests ----------------------------------------------------------

    struct FakeServer {
        port: u16,
        /// Every messageID a prompt_async POST carried, accepted or not.
        posts: Arc<Mutex<Vec<String>>>,
        accept_posts: Arc<AtomicBool>,
        /// When set, the message read-back answers 500: durable state is unknowable.
        read_back_error: Arc<AtomicBool>,
        /// When set, /session/status answers 500: the level seed fails.
        status_error: Arc<AtomicBool>,
        /// Ask ids /permission reports as pending.
        pending_permissions: Arc<Mutex<Vec<String>>>,
        /// Ask ids /question reports as pending.
        pending_questions: Arc<Mutex<Vec<String>>>,
        /// When set, both pending-ask listings answer 500: the ask seed fails.
        ask_error: Arc<AtomicBool>,
        /// Session ids `GET /session` lists (idle sessions appear here and nowhere else).
        listed_sessions: Arc<Mutex<Vec<String>>>,
        /// When set, /session/status serves this raw body instead of an object.
        status_body: Arc<Mutex<Option<String>>>,
        /// When set, /permission serves this raw body instead of the pending ids.
        ask_body: Arc<Mutex<Option<String>>>,
    }

    fn spawn_fake_server() -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let posts = Arc::new(Mutex::new(Vec::new()));
        let durable = Arc::new(Mutex::new(BTreeSet::new()));
        let accept_posts = Arc::new(AtomicBool::new(true));
        let read_back_error = Arc::new(AtomicBool::new(false));
        let status_error = Arc::new(AtomicBool::new(false));
        let pending_permissions = Arc::new(Mutex::new(Vec::<String>::new()));
        let pending_questions = Arc::new(Mutex::new(Vec::<String>::new()));
        let ask_error = Arc::new(AtomicBool::new(false));
        let ask_body = Arc::new(Mutex::new(None::<String>));
        let listed_sessions = Arc::new(Mutex::new(Vec::<String>::new()));
        let status_body = Arc::new(Mutex::new(None::<String>));
        let (posts_t, durable_t, accept_t) = (posts.clone(), durable.clone(), accept_posts.clone());
        let (read_back_t, status_err_t, pending_t, questions_t, ask_err_t, listed_t, status_body_t) = (
            read_back_error.clone(),
            status_error.clone(),
            pending_permissions.clone(),
            pending_questions.clone(),
            ask_error.clone(),
            listed_sessions.clone(),
            status_body.clone(),
        );
        let ask_body_t = ask_body.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut content_length = 0_usize;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let mut body = vec![0_u8; content_length];
                let _ = reader.read_exact(&mut body);
                let mut parts = request_line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let status = if method == "POST" && path.ends_with("/prompt_async") {
                    let message_id =
                        serde_json::from_slice::<Value>(&body)
                            .ok()
                            .and_then(|value| {
                                value
                                    .get("messageID")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            });
                    match message_id {
                        Some(message_id) => {
                            posts_t.lock().unwrap().push(message_id.clone());
                            if accept_t.load(Ordering::SeqCst) {
                                durable_t.lock().unwrap().insert(message_id);
                                200
                            } else {
                                500
                            }
                        }
                        None => 400,
                    }
                } else if method == "GET" && path.contains("/message/") {
                    let message_id = path.rsplit('/').next().unwrap_or("");
                    if read_back_t.load(Ordering::SeqCst) {
                        500
                    } else if durable_t.lock().unwrap().contains(message_id) {
                        200
                    } else {
                        404
                    }
                } else if method == "GET" && path == "/session" {
                    200
                } else if method == "GET" && path == "/config/providers" {
                    200
                } else if method == "GET" && path == "/session/status" {
                    if status_err_t.load(Ordering::SeqCst) {
                        500
                    } else {
                        200
                    }
                } else if method == "GET" && (path == "/permission" || path == "/question") {
                    if ask_err_t.load(Ordering::SeqCst) {
                        500
                    } else {
                        200
                    }
                } else {
                    404
                };
                let body = if method == "GET" && path == "/session" {
                    let ids = listed_t.lock().unwrap();
                    serde_json::to_string(
                        &ids.iter()
                            .map(|id| serde_json::json!({ "id": id }))
                            .collect::<Vec<_>>(),
                    )
                    .unwrap()
                } else if method == "GET" && path == "/config/providers" {
                    OC_PROVIDERS.to_string()
                } else if method == "GET" && path == "/session/status" {
                    if let Some(body) = status_body_t.lock().unwrap().clone() {
                        body
                    } else {
                        "{}".to_string()
                    }
                } else if method == "GET" && (path == "/permission" || path == "/question") {
                    if path == "/permission"
                        && let Some(body) = ask_body_t.lock().unwrap().clone()
                    {
                        let mut stream = reader.into_inner();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        continue;
                    }
                    let ids = if path == "/permission" {
                        pending_t.lock().unwrap()
                    } else {
                        questions_t.lock().unwrap()
                    };
                    serde_json::to_string(
                        &ids.iter()
                            .map(|id| serde_json::json!({ "id": id }))
                            .collect::<Vec<_>>(),
                    )
                    .unwrap()
                } else {
                    "{}".to_string()
                };
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        FakeServer {
            port,
            posts,
            accept_posts,
            read_back_error,
            status_error,
            pending_permissions,
            pending_questions,
            ask_error,
            listed_sessions,
            status_body,
            ask_body,
        }
    }

    /// Measured on 1.18.19: a second POST with the same messageID appends its parts again into
    /// the same message, so an indeterminate read-back must retry the read-back, never the POST.
    #[test]
    fn an_indeterminate_read_back_retries_the_read_back_and_never_re_posts() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, filename) = delivery_fixture(tmp.path(), state_path.clone());

        server.read_back_error.store(true, Ordering::SeqCst);
        delivery.pump(&client);
        assert_eq!(server.posts.lock().unwrap().len(), 1, "one POST, attempted");
        assert_eq!(
            ledger_phase(&state_path, &filename),
            Some(delivery_ledger::Phase::TransportAccepted),
            "the POST landed; nothing yet proves the server holds it"
        );

        // While the read-back stays indeterminate, no pass may re-POST.
        delivery.pump(&client);
        delivery.pump(&client);
        assert_eq!(server.posts.lock().unwrap().len(), 1);

        // The read-back recovering flips the same attempt to Accepted with no second POST.
        server.read_back_error.store(false, Ordering::SeqCst);
        delivery.pump(&client);
        assert_eq!(
            ledger_phase(&state_path, &filename),
            Some(delivery_ledger::Phase::Persisted),
            "a recovered read-back proves storage, not consumption"
        );
        assert_eq!(server.posts.lock().unwrap().len(), 1);
    }

    #[test]
    fn delivery_and_read_back_boundaries_publish_and_clear_diagnostics_without_changing_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, _filename) = delivery_fixture(tmp.path(), state_path);
        let agent_dir = tmp.path().join("agents/h/worker");
        let mut diagnostics = DiagnosticPublisher::new(
            &agent_dir,
            DiagnosticDriver::OpenCode,
            Some("1.18.19".to_string()),
            DiagnosticSupport::Supported,
        );

        server.accept_posts.store(false, Ordering::SeqCst);
        delivery.pump_diagnosed(&client, &mut diagnostics);
        let crate::driver_diagnostic::Observed::Failure(failure) =
            crate::driver_diagnostic::read(&crate::driver_diagnostic::path(&agent_dir))
        else {
            panic!("rejected prompt must be diagnosed")
        };
        assert_eq!(failure.stage, DiagnosticStage::Delivery);
        assert_eq!(failure.reason, DiagnosticReason::DeliveryRejected);

        server.accept_posts.store(true, Ordering::SeqCst);
        delivery.next_attempt = Instant::now();
        delivery.pump_diagnosed(&client, &mut diagnostics);
        assert_eq!(
            crate::driver_diagnostic::read(&crate::driver_diagnostic::path(&agent_dir)),
            crate::driver_diagnostic::Observed::Absent,
            "successful retry and durable read-back clear both boundaries"
        );

        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, _filename) = delivery_fixture(tmp.path(), state_path);
        let agent_dir = tmp.path().join("agents/h/worker");
        let mut diagnostics = DiagnosticPublisher::new(
            &agent_dir,
            DiagnosticDriver::OpenCode,
            Some("1.18.19".to_string()),
            DiagnosticSupport::Supported,
        );
        server.read_back_error.store(true, Ordering::SeqCst);
        delivery.pump_diagnosed(&client, &mut diagnostics);
        let crate::driver_diagnostic::Observed::Failure(failure) =
            crate::driver_diagnostic::read(&crate::driver_diagnostic::path(&agent_dir))
        else {
            panic!("indeterminate read-back must be diagnosed")
        };
        assert_eq!(failure.stage, DiagnosticStage::ReadBack);
        assert_eq!(failure.reason, DiagnosticReason::ReadBackUnavailable);

        server.read_back_error.store(false, Ordering::SeqCst);
        delivery.pump_diagnosed(&client, &mut diagnostics);
        assert_eq!(
            crate::driver_diagnostic::read(&crate::driver_diagnostic::path(&agent_dir)),
            crate::driver_diagnostic::Observed::Absent,
            "read-back recovery clears without a second POST"
        );
        assert_eq!(server.posts.lock().unwrap().len(), 1);
    }

    /// An ask opened before the SSE connection must survive the reconnect seed with its id, so
    /// the ordinary id-matched exit still releases it.
    #[test]
    fn seeding_recovers_a_pending_ask_and_gates_evidence_on_the_level_seed() {
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");

        // A transiently failing level seed yields no evidence at all.
        server.status_error.store(true, Ordering::SeqCst);
        let mut machine = EventMachine::default();
        assert!(seed_from_server(&client, &mut machine).is_err());
        server.status_error.store(false, Ordering::SeqCst);

        // So does a failing pending-ask listing: an ask opened during the outage must not be
        // reported unblocked with heartbeats restored — the seed retries instead.
        server.ask_error.store(true, Ordering::SeqCst);
        let mut machine = EventMachine::default();
        assert!(seed_from_server(&client, &mut machine).is_err());
        server.ask_error.store(false, Ordering::SeqCst);

        // A successful seed recovers the pending ask under its own id.
        server.status_error.store(false, Ordering::SeqCst);
        server
            .pending_permissions
            .lock()
            .unwrap()
            .push("per_pending".to_string());
        let mut machine = EventMachine::default();
        assert!(seed_from_server(&client, &mut machine).is_ok());
        let blocked = observed(&machine);
        assert_eq!(blocked.state, Activity::Active);
        assert_eq!(blocked.blocked_on, BlockedOn::Human);

        assert_eq!(blocked.ask, Ask::Permission);

        // The id-matched exit releases exactly the recovered ask.
        machine.apply(&event(
            r#"{"type":"permission.replied","properties":{"requestID":"per_pending","reply":"once"}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::None);

        // Pending questions are recovered from their own listing endpoint (measured on 1.18.19),
        // classified as question asks, and released by the question's id-matched reply.
        server.pending_permissions.lock().unwrap().clear();
        server
            .pending_questions
            .lock()
            .unwrap()
            .push("que_pending".to_string());
        let mut machine = EventMachine::default();
        assert!(seed_from_server(&client, &mut machine).is_ok());
        let blocked = observed(&machine);
        assert_eq!(blocked.blocked_on, BlockedOn::Human);
        assert_eq!(blocked.ask, Ask::Question);
        machine.apply(&event(
            r#"{"type":"question.replied","properties":{"requestID":"que_pending","answers":[["Yes"]]}}"#,
        ));
        assert_eq!(observed(&machine).blocked_on, BlockedOn::None);
    }

    fn delivery_fixture(tmp: &Path, state_path: PathBuf) -> (Delivery, String) {
        let agent_dir = tmp.join("agents/h/worker");
        let inbox = message::inbox_dir(&agent_dir);
        std::fs::create_dir_all(&inbox).unwrap();
        let filename =
            message::send_to_inbox(&inbox, "h.sender", Some("subject"), None, &[], "body").unwrap();
        let mut delivery =
            Delivery::with_state_path(tmp, &agent_dir, "h", "h.worker", "h.worker", state_path);
        delivery.saw_session("ses_target");
        (delivery, filename)
    }

    /// Read the ledger back through its own loader and the real correlation derivation: a test
    /// that read the bytes directly would not notice a record the pump itself would refuse.
    fn reopen_ledger(ledger_path: &Path) -> delivery_ledger::Ledger {
        delivery_ledger::Ledger::open(
            ledger_path,
            delivery_ledger::Harness::OpenCode.profile(),
            "h.worker",
            "h.worker",
            |session, file| stable_message_id("h.worker", session, file),
        )
    }

    fn ledger_phase(ledger_path: &Path, filename: &str) -> Option<delivery_ledger::Phase> {
        reopen_ledger(ledger_path)
            .entry(filename)
            .map(|entry| entry.phase)
    }

    #[test]
    fn a_durable_read_back_is_persistence_that_never_releases_the_inbox_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, filename) = delivery_fixture(tmp.path(), state_path.clone());

        delivery.pump(&client);
        let expected_id = stable_message_id("h.worker", "ses_target", &filename);
        assert_eq!(
            server.posts.lock().unwrap().as_slice(),
            [expected_id.clone()]
        );
        // Same server fixture, same single-POST conclusion, honest label: `GET 200` is storage.
        let entry = reopen_ledger(&state_path).entry(&filename).cloned().unwrap();
        assert_eq!(entry.phase, delivery_ledger::Phase::Persisted);
        assert_eq!(entry.correlation.value, expected_id);
        assert_eq!(
            reopen_ledger(&state_path).retention(&filename),
            delivery_ledger::Retention::Hold(delivery_ledger::HoldReason::UnreadReceipt),
            "storage is not admission: ownership is retained until the scheduler proves it, and \
             only the recipient's own archive settles the message"
        );
        assert!(
            message::inbox_dir(&tmp.path().join("agents/h/worker"))
                .join(&filename)
                .is_file(),
            "the ledger never archives an inbox file"
        );

        // Persistence is terminal for the POST loop: further pumps send nothing.
        delivery.pump(&client);
        delivery.pump(&client);
        assert_eq!(server.posts.lock().unwrap().len(), 1);
    }



    #[test]
    fn a_failed_transport_retries_the_same_identity_never_a_second_one() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        server.accept_posts.store(false, Ordering::SeqCst);
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, filename) = delivery_fixture(tmp.path(), state_path.clone());

        delivery.pump(&client);
        assert_eq!(
            ledger_phase(&state_path, &filename),
            Some(delivery_ledger::Phase::Attempted),
            "a refused POST never reaches transportAccepted"
        );

        server.accept_posts.store(true, Ordering::SeqCst);
        delivery.next_attempt = Instant::now();
        delivery.pump(&client);

        let expected_id = stable_message_id("h.worker", "ses_target", &filename);
        assert_eq!(
            server.posts.lock().unwrap().as_slice(),
            [expected_id.clone(), expected_id]
        );
        assert_eq!(
            ledger_phase(&state_path, &filename),
            Some(delivery_ledger::Phase::Persisted)
        );
    }

    #[test]
    fn dnd_suppresses_delivery_and_a_missing_target_session_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, _) = delivery_fixture(tmp.path(), state_path);

        let agent_dir = tmp.path().join("agents/h/worker");
        status::set_state(&status::status_path(&agent_dir), status::State::Dnd).unwrap();
        delivery.pump(&client);
        assert!(server.posts.lock().unwrap().is_empty());

        status::set_state(&status::status_path(&agent_dir), status::State::Available).unwrap();
        delivery.target_session = None;
        delivery.pump(&client);
        assert!(
            server.posts.lock().unwrap().is_empty(),
            "no session yet: wait, never create"
        );
    }

    /// T5: an unrecognized future `session.status` word is no evidence at all — counting it as
    /// level evidence would let a quiet server derive `idle` from a word we cannot read.
    #[test]
    fn an_unrecognized_status_word_is_not_level_evidence() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"hibernating"}}}"#,
        ));
        assert_eq!(
            machine.observation(),
            None,
            "no level evidence, no observation"
        );
    }

    /// O1: a seat whose session settled before the observer connected is invisible to the event
    /// stream and to /session/status — with work pending, the delivery binding is recovered from
    /// the session listing rather than stalling forever.
    #[test]
    fn a_pre_settled_session_is_recovered_from_the_listing_for_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let state_path = tmp.path().join("state/delivery-ledger.json");
        let (mut delivery, _filename) = delivery_fixture(tmp.path(), state_path);
        delivery.target_session = None;

        // No listing yet: nothing to bind, nothing sent — and no wedge, it simply retries.
        delivery.pump(&client);
        assert!(server.posts.lock().unwrap().is_empty());

        // The idle session exists only in the listing; the next pass binds and delivers.
        server
            .listed_sessions
            .lock()
            .unwrap()
            .push("ses_settled".to_string());
        delivery.pump(&client);
        let posts = server.posts.lock().unwrap();
        assert_eq!(posts.len(), 1, "delivery bound to the recovered session");
    }

    /// W8-4: the seed is atomic against the LIVE machine — a mid-seed failure leaves no
    /// half-seeded asks behind, and a successful re-seed clears stale entries whose exits
    /// passed while the stream was down.
    #[test]
    fn the_seed_swaps_in_whole_and_clears_what_the_level_no_longer_states() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let _keep = tmp;

        // A machine holding a stale blocked ask and a stale busy session from before the drop.
        let mut machine = EventMachine::default();
        machine.seed_busy("ses_stale".to_string(), false);
        machine.seed_ask("per_stale".to_string(), "permission");

        // /permission succeeds, /question fails mid-seed: the live machine is untouched —
        // the stale ask is still held (not half-cleared) and no new ask leaked in.
        server
            .pending_permissions
            .lock()
            .unwrap()
            .push("per_new".to_string());
        server.ask_error.store(true, Ordering::SeqCst);
        // ask_error fails BOTH listings; simulate the split by failing only after /permission:
        // the atomicity claim is the same — nothing of the attempt lands.
        assert!(seed_from_server(&client, &mut machine).is_err());
        let snapshot = observed(&machine);
        assert_eq!(
            snapshot.blocked_on,
            BlockedOn::Human,
            "stale ask still held"
        );
        server.ask_error.store(false, Ordering::SeqCst);

        // A successful re-seed states the whole level truth: the settled session and the
        // resolved ask disappear, the listed pending ask remains.
        assert!(seed_from_server(&client, &mut machine).is_ok());
        let snapshot = observed(&machine);
        assert_eq!(snapshot.blocked_on, BlockedOn::Human);
        machine.apply(&event(
            r#"{"type":"permission.replied","properties":{"requestID":"per_new","reply":"once"}}"#,
        ));
        let snapshot = observed(&machine);
        assert_eq!(
            snapshot.blocked_on,
            BlockedOn::None,
            "per_stale is gone, not wedged"
        );
        assert_eq!(
            snapshot.state,
            Activity::Idle,
            "ses_stale cleared by the re-seed"
        );
    }

    /// An unknown status word on ANY session poisons the whole projection until a level seed
    /// replaces the machine: a tracked-busy entry can no longer be trusted to clear, and an
    /// untracked session in a state this version cannot read makes standing idle evidence a
    /// fabrication — that session may already be mid-turn.
    #[test]
    fn an_unreadable_status_word_on_a_tracked_busy_session_poisons_the_projection() {
        let mut machine = EventMachine::default();
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"busy"}}}"#,
        ));
        assert_eq!(observed(&machine).state, Activity::Active);
        machine.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"hibernating"}}}"#,
        ));
        assert!(
            machine.observation().is_none(),
            "a busy entry that can no longer be trusted to clear must withhold every projection"
        );

        // The same word on a session the projection does NOT track: standing idle evidence
        // must not keep heartbeating on top of unreadable activity.
        let mut untracked = EventMachine::default();
        untracked.seed_idle();
        assert_eq!(observed(&untracked).state, Activity::Idle);
        untracked.apply(&event(
            r#"{"type":"session.status","properties":{"sessionID":"ses_9","status":{"type":"hibernating"}}}"#,
        ));
        assert!(
            untracked.observation().is_none(),
            "an unknown word on an untracked session withholds the definite idle"
        );

        // A sticky terminal outranks the poison: it does not depend on the busy map the
        // unknown word made untrustworthy, and withholding it would lose the terminal.
        machine.apply(&event(
            r#"{"type":"session.error","properties":{"sessionID":"ses_1","error":{"name":"ProviderAuthError"}}}"#,
        ));
        let terminal = observed(&machine);
        assert_eq!(terminal.state, Activity::Ended);
        assert_eq!(terminal.reason.as_deref(), Some("providerAuth"));
    }

    /// A pending listing entry whose id this version cannot read must fail the whole seed:
    /// seeding around it would restore evidence on a picture that silently drops a human block.
    #[test]
    fn an_unreadable_pending_entry_fails_the_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let _keep = tmp;

        *server.ask_body.lock().unwrap() = Some(r#"[{"id":"per_ok"},{"token":42}]"#.to_string());
        let mut machine = EventMachine::default();
        assert!(
            seed_from_server(&client, &mut machine).is_err(),
            "an entry without a readable id must fail the seed, not be skipped"
        );

        *server.ask_body.lock().unwrap() = None;
        server
            .pending_permissions
            .lock()
            .unwrap()
            .push("per_ok".to_string());
        let mut machine = EventMachine::default();
        assert!(seed_from_server(&client, &mut machine).is_ok());
        assert_eq!(observed(&machine).blocked_on, BlockedOn::Human);
    }

    /// Cluster 3: a /session/status response that is not the documented object shape proves
    /// nothing — null and array shapes must fail the seed, never read as definite idle.
    #[test]
    fn non_object_status_shapes_fail_the_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let _keep = tmp;
        for shape in ["null", "[]", "[\"ses_a\"]", "3"] {
            *server.status_body.lock().unwrap() = Some(shape.to_string());
            let mut machine = EventMachine::default();
            assert!(
                seed_from_server(&client, &mut machine).is_err(),
                "shape {shape} must fail the seed"
            );
            assert_eq!(
                machine.observation(),
                None,
                "no level evidence from {shape}"
            );
        }
        *server.status_body.lock().unwrap() = None;
        let mut machine = EventMachine::default();
        assert!(seed_from_server(&client, &mut machine).is_ok());
    }

    /// n8: a server that accepts the stream and then goes silent must surface as a disconnect
    /// within the silence horizon — a stalled socket cannot keep evidence alive forever.
    #[test]
    fn a_silent_sse_stream_disconnects_at_the_silence_horizon() {
        use std::io::Write as _;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
            // Say nothing, forever; hold the socket open.
            thread::sleep(Duration::from_secs(30));
        });
        let mut client = Client::new(port, "pw");
        client.sse_silence = Duration::from_millis(200);

        let (tx, rx) = mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        spawn_sse_reader(client, tx, stop.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_disconnect = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(SseMessage::Disconnected(_)) => {
                    saw_disconnect = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        stop.store(true, Ordering::SeqCst);
        assert!(saw_disconnect, "silence must surface as a disconnect");
    }

    // ---- harness context: frames captured verbatim from OpenCode 1.18.25 (HC-R13) -------------
    //
    // Every constant below is one `data:` payload of the credential-free lab run recorded in
    // `docs/vrs/08-harness-context/spec.md` §opencode, unedited. The version the frames were
    // captured against is asserted from the frames themselves — `session.updated` carries
    // `info.version` — so a bump that changes the numerator, the denominator, or the shape of the
    // join fails here rather than silently publishing a differently-meaning number.

    /// Turn 1's FIRST `message.updated` for the assistant message: all-zero token fields and **no**
    /// `total` key at all.
    const OC_TURN_1_OPENING: &str = r#"{"id":"evt_04cfa35b9001JZ9d0L9djTMOAJ","type":"message.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"id":"msg_04cfa35b9001v0KGCf03ofZ2UX","parentID":"msg0000000000000000000000001","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/tmp/oclab/work","root":"/"},"cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},"modelID":"hy3-free","providerID":"opencode","time":{"created":1787997861305},"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM"}}}"#;
    /// Turn 1's FINAL `message.updated`, the one carrying `tokens.total`. Emitted twice with
    /// identical content, which is why the second is fed to the producer in the same test.
    const OC_TURN_1_FINAL: &str = r#"{"id":"evt_04cfa4404001t29p1VUcmmPoRG","type":"message.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"id":"msg_04cfa35b9001v0KGCf03ofZ2UX","parentID":"msg0000000000000000000000001","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/tmp/oclab/work","root":"/"},"cost":0,"tokens":{"total":8200,"input":6455,"output":4,"reasoning":13,"cache":{"write":0,"read":1728}},"modelID":"hy3-free","providerID":"opencode","time":{"created":1787997861305,"completed":1787997864970},"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","finish":"stop"}}}"#;
    /// The `session.updated` that follows turn 1: cumulative `info.tokens`, no `total` key.
    const OC_TURN_1_SESSION: &str = r#"{"id":"evt_04cfa4415001vEbJR0Dd0RfwXA","type":"session.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"id":"ses_fb305cb62ffeA6a053WBLMBcRM","slug":"tidy-falcon","projectID":"global","directory":"/tmp/oclab/work","path":"tmp/oclab/work","title":"New session - 2026-08-29T10:04:21.022Z","agent":"build","model":{"id":"hy3-free","providerID":"opencode","variant":"default"},"version":"1.18.25","summary":{"additions":0,"deletions":0,"files":0},"cost":0,"tokens":{"input":6455,"output":4,"reasoning":13,"cache":{"read":1728,"write":0}},"time":{"created":1787997861022,"updated":1787997864980}}}}"#;
    /// Turn 2's final `message.updated`: a different message, the SAME occupancy (8,200) — the
    /// window did not grow because the earlier turn moved into the cache.
    const OC_TURN_2_FINAL: &str = r#"{"id":"evt_04cfb088c001CyXonjcqRdxBfA","type":"message.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"id":"msg_04cfb00ba001bh7MmStxmbJ0J3","parentID":"msg0000000000000000000000002","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/tmp/oclab/work","root":"/"},"cost":0,"tokens":{"total":8200,"input":133,"output":3,"reasoning":0,"cache":{"write":0,"read":8064}},"modelID":"hy3-free","providerID":"opencode","time":{"created":1787997913274,"completed":1787997915280},"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","finish":"stop"}}}"#;
    /// The `session.updated` after turn 2: the cumulative total has doubled while occupancy has
    /// not moved. 6588 + 7 + 13 + 9792 = 16,400 = 2 × 8,200.
    const OC_TURN_2_SESSION: &str = r#"{"id":"evt_04cfb089b0013nTIMhUVKqyBW6","type":"session.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"id":"ses_fb305cb62ffeA6a053WBLMBcRM","slug":"tidy-falcon","projectID":"global","directory":"/tmp/oclab/work","path":"tmp/oclab/work","title":"Request to say pineapple","agent":"build","model":{"id":"hy3-free","providerID":"opencode","variant":"default"},"version":"1.18.25","summary":{"additions":0,"deletions":0,"files":0},"cost":0,"tokens":{"input":6588,"output":7,"reasoning":13,"cache":{"read":9792,"write":0}},"time":{"created":1787997861022,"updated":1787997915290}}}}"#;
    /// A USER message whose `summary` is an OBJECT — truthy, and not a compaction summary.
    const OC_USER_SUMMARY_OBJECT: &str = r#"{"id":"evt_04cfa360b001CDm6cFnqfAt7cc","type":"message.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"role":"user","time":{"created":1787997861241},"agent":"build","model":{"providerID":"opencode","modelID":"hy3-free"},"id":"msg0000000000000000000000001","sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","summary":{"diffs":[]}}}}"#;
    /// The compaction summarizer's own assistant message: `summary: true`, and a `tokens.total` of
    /// 1,511 that is the cost of the summarization call, not the size of the new context.
    const OC_SUMMARIZER_MESSAGE: &str = r#"{"id":"evt_04cfc5814001Vn5oChuQ2RQooq","type":"message.updated","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","info":{"id":"msg_04cfc2fce0012p4wkSC7k1IXrQ","role":"assistant","parentID":"msg_04cfc2fc00017s1bHmHcf4uNkQ","sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM","mode":"compaction","agent":"compaction","summary":true,"path":{"cwd":"/tmp/oclab/work","root":"/"},"cost":0,"tokens":{"total":1511,"input":387,"output":103,"reasoning":957,"cache":{"write":0,"read":64}},"modelID":"hy3-free","providerID":"opencode","time":{"created":1787997990862,"completed":1787998001175},"finish":"stop"}}}"#;
    /// The whole compaction edge: a session id and nothing else.
    const OC_SESSION_COMPACTED: &str = r#"{"id":"evt_04cfc5818001PhY4Ho4tMNhoNz","type":"session.compacted","properties":{"sessionID":"ses_fb305cb62ffeA6a053WBLMBcRM"}}"#;
    /// `GET /config/providers`, verbatim in envelope and in the two model entries kept, which are
    /// the join's whole surface: `providers[].models[<modelID>].limit.context`.
    const OC_PROVIDERS: &str = r#"{"providers":[{"id":"opencode","name":"OpenCode Zen","source":"custom","env":["OPENCODE_API_KEY"],"options":{"apiKey":"public"},"models":{"nemotron-3-ultra-free":{"id":"nemotron-3-ultra-free","providerID":"opencode","api":{"id":"nemotron-3-ultra-free","url":"https://opencode.ai/zen/v1","npm":"@ai-sdk/openai-compatible"},"name":"Nemotron 3 Ultra Free","family":"nemotron-free","cost":{"input":0,"output":0,"cache":{"read":0,"write":0}},"limit":{"context":1000000,"output":128000},"status":"active","options":{},"headers":{},"release_date":"2026-06-04","variants":{}},"hy3-free":{"id":"hy3-free","providerID":"opencode","api":{"id":"hy3-free","url":"https://opencode.ai/zen/v1","npm":"@ai-sdk/openai-compatible"},"name":"Hy3 Free","family":"hy3-free","cost":{"input":0,"output":0,"cache":{"read":0,"write":0}},"limit":{"context":190000,"output":64000},"status":"active","options":{},"headers":{},"release_date":"2026-07-06","variants":{"low":{"reasoningEffort":"low"},"medium":{"reasoningEffort":"medium"},"high":{"reasoningEffort":"high"}}}}}],"default":{"opencode":"big-pickle"}}"#;

    /// The window `hy3-free` published, and the occupancy every measured turn settled at.
    const OC_WINDOW: u64 = 190_000;
    const OC_OCCUPANCY: u64 = 8_200;

    struct ContextFixture {
        _tmp: tempfile::TempDir,
        agent_dir: PathBuf,
        producer: ContextProducer,
    }

    impl ContextFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let agent_dir = tmp.path().join("agents").join("hetz").join("seat");
            std::fs::create_dir_all(&agent_dir).unwrap();
            let producer = ContextProducer::new(&agent_dir, "hetz.seat", "incarnation-1").unwrap();
            Self {
                _tmp: tmp,
                agent_dir,
                producer,
            }
        }

        /// One batch of frames, exactly as the wrapper's drain treats them: fold them all in, then
        /// publish once if any of them carried a fresh numerator. Returns whether a write landed.
        /// Batching is what lets a turn's cost and cumulative total ride out with the numerator
        /// that made the reading fresh — they arrive on `session.updated` a frame later.
        fn drain(&mut self, frames: &[&str]) -> bool {
            let mut fresh = false;
            for raw in frames {
                fresh |= self.producer.apply(&event(raw));
            }
            fresh && self.producer.publish()
        }

        fn feed(&mut self, raw: &str) -> bool {
            self.drain(&[raw])
        }

        fn record(&self) -> Option<harness_context::Observed> {
            harness_context::read(&harness_context::harness_context_path(&self.agent_dir))
        }
    }

    /// The version the fixtures are pinned to, read out of the frames themselves.
    fn captured_version(raw: &str) -> String {
        event(raw)
            .pointer("/properties/info/version")
            .and_then(Value::as_str)
            .expect("session.updated carries the server version")
            .to_string()
    }

    /// HC-R02, HC-R13: two measured turns become readings — last non-summary assistant
    /// `tokens.total` over the model's `limit.context` — and the duplicate final frame OpenCode
    /// emits for every completion lands no second write (the DQ-H2 flood the sibling record hit).
    #[test]
    fn captured_opencode_turns_publish_the_assistant_total_over_the_providers_window() {
        assert_eq!(captured_version(OC_TURN_1_SESSION), "1.18.25");
        assert_eq!(captured_version(OC_TURN_2_SESSION), "1.18.25");

        let mut fixture = ContextFixture::new();
        fixture.producer.ingest_providers(&event(OC_PROVIDERS));

        // The opening frame of a message carries no `total`: not a reading, and nothing written.
        assert!(!fixture.feed(OC_TURN_1_OPENING));
        assert!(
            fixture.record().is_none(),
            "an all-zero opening frame must not publish a 0% window"
        );

        // Turn 1's batch: the final frame OpenCode emits twice, then the session frame carrying
        // the turn's adjacent facts. One landed write for the whole turn.
        assert!(
            fixture.drain(&[OC_TURN_1_FINAL, OC_TURN_1_FINAL, OC_TURN_1_SESSION]),
            "turn 1 is a reading"
        );
        let record = fixture.record().expect("a context record");
        assert_eq!(record.harness, harness_context::Harness::OpenCode);
        assert_eq!(record.used_tokens, Some(OC_OCCUPANCY));
        assert_eq!(record.window_tokens, Some(OC_WINDOW));
        let expected = OC_OCCUPANCY as f64 / OC_WINDOW as f64 * 100.0;
        assert!(
            (record.used_percent.expect("a percent") - expected).abs() < 1e-9,
            "the percent is st2's own unrounded division: {:?}",
            record.used_percent
        );
        assert_eq!(record.model.as_deref(), Some("opencode/hy3-free"));
        assert_eq!(record.cost_usd, Some(0.0));
        assert_eq!(record.compactions, 0);

        // Turn 2 is a genuinely fresh reading in the same 1% bucket, so nothing is written and the
        // record stays the coherent turn-1 snapshot it was — its own `observedAtMs` says so. That
        // is the guard the sibling record's restatement flood (DQ-H2) taught this one to keep.
        assert!(
            !fixture.drain(&[OC_TURN_2_FINAL, OC_TURN_2_FINAL, OC_TURN_2_SESSION]),
            "a reading inside the written bucket must not write again"
        );
        let after = fixture.record().expect("a context record");
        assert_eq!(after.observed_at_ms, record.observed_at_ms);
        assert_eq!(after.session_total_tokens, record.session_total_tokens);
    }

    /// HC-R16: `session.info.tokens` is cumulative and carries no `total` key. It is the session
    /// total and never the numerator — after two turns it is twice the occupancy.
    #[test]
    fn cumulative_session_tokens_are_the_session_total_and_never_the_occupancy() {
        let mut fixture = ContextFixture::new();
        fixture.producer.ingest_providers(&event(OC_PROVIDERS));

        // Adjacent facts alone are not a reading: `session.updated` never publishes, because its
        // numbers say nothing about occupancy and stamping them would date a numerator that may
        // be hours old.
        fixture.feed(OC_TURN_1_SESSION);
        assert!(
            fixture.record().is_none(),
            "a cumulative-token frame must not stamp a reading of its own"
        );

        fixture.drain(&[OC_TURN_1_FINAL, OC_TURN_1_SESSION]);
        fixture.drain(&[OC_TURN_2_FINAL, OC_TURN_2_SESSION]);
        // The reading the producer now holds, whatever the write guard decides to do with it: the
        // cumulative total has reached twice the occupancy, and the percent divides the occupancy.
        let reading = fixture.producer.reading();
        assert_eq!(reading.session_total_tokens, Some(16_400));
        assert_eq!(reading.used_tokens, Some(OC_OCCUPANCY));
        assert_eq!(reading.window_tokens, Some(OC_WINDOW));
        assert!(
            (reading.used_percent.expect("a percent") - 4.315_789_473_684_21).abs() < 1e-9,
            "the cumulative total is never the numerator: {reading:?}"
        );
        // 16,400 of a 190,000 window would read 8.6%: twice the truth, and growing every turn.
        assert!(reading.used_percent.expect("a percent") < 5.0);
        let record = fixture.record().expect("a context record");
        assert_eq!(record.used_tokens, Some(OC_OCCUPANCY));
    }

    /// HC-R02: the two measured `summary` traps. The user message's `summary` is an OBJECT and
    /// therefore truthy, and the compaction summarizer's own assistant message carries a
    /// `tokens.total` of 1,511 that would report a freshly compacted window as nearly empty.
    #[test]
    fn neither_summary_shape_is_ever_the_reading() {
        let mut fixture = ContextFixture::new();
        fixture.producer.ingest_providers(&event(OC_PROVIDERS));
        fixture.feed(OC_TURN_2_FINAL);
        let before = fixture.record().expect("a context record");

        assert!(!fixture.feed(OC_USER_SUMMARY_OBJECT));
        assert!(!fixture.feed(OC_SUMMARIZER_MESSAGE));

        let after = fixture.record().expect("a context record");
        assert_eq!(after.used_tokens, Some(OC_OCCUPANCY));
        assert_ne!(after.used_tokens, Some(1_511));
        assert_eq!(after.observed_at_ms, before.observed_at_ms);
    }

    /// HC-R12: `session.compacted` carries `{sessionID}` and nothing else, so the trigger is
    /// `unknown` and the count is st2's, scoped to this incarnation. The edge carries no reading,
    /// so the numbers keep the age they had — they genuinely predate the compaction — and nothing
    /// zeroes them (HC-R03); the next turn replaces them with an observation.
    #[test]
    fn a_captured_compaction_counts_once_with_an_unknown_trigger_and_no_restamp() {
        let mut fixture = ContextFixture::new();
        fixture.producer.ingest_providers(&event(OC_PROVIDERS));
        fixture.feed(OC_TURN_1_FINAL);
        let before = fixture.record().expect("a context record");
        assert_eq!(before.compactions, 0);

        fixture.feed(OC_SUMMARIZER_MESSAGE);
        fixture.feed(OC_SESSION_COMPACTED);

        let after = fixture.record().expect("a context record");
        assert_eq!(after.compactions, 1);
        assert_eq!(
            after.last_compaction_trigger,
            Some(harness_context::CompactionTrigger::Unknown)
        );
        assert!(after.last_compaction_ms.is_some());
        assert_eq!(
            after.observed_at_ms, before.observed_at_ms,
            "a compaction edge carries no reading and must not re-stamp one"
        );
        assert_eq!(after.used_tokens, Some(OC_OCCUPANCY));
    }

    /// HC-R02, HC-R03: with no window for the reading's model the percent is withheld rather than
    /// guessed from a table, and the first successful providers pull is itself a bucket change, so
    /// the join lands as soon as it is known.
    #[test]
    fn an_unjoined_model_withholds_the_window_and_the_percent_until_the_pull_lands() {
        let mut fixture = ContextFixture::new();
        assert!(fixture.feed(OC_TURN_1_FINAL));
        let withheld = fixture.record().expect("a context record");
        assert_eq!(withheld.used_tokens, Some(OC_OCCUPANCY));
        assert_eq!(withheld.window_tokens, None);
        assert_eq!(withheld.used_percent, None);
        assert_eq!(
            fixture.producer.window_wanted(),
            Some("opencode/hy3-free"),
            "an unjoined model is what makes the producers pull due"
        );

        fixture.producer.ingest_providers(&event(OC_PROVIDERS));
        assert_eq!(fixture.producer.window_wanted(), None);
        assert!(
            fixture.feed(OC_TURN_2_FINAL),
            "withheld-to-known is itself a bucket change"
        );
        let joined = fixture.record().expect("a context record");
        assert_eq!(joined.window_tokens, Some(OC_WINDOW));
        assert!(joined.used_percent.is_some());
    }

    /// The denominator's whole live path: the exact endpoint string, through the real `Client`, into
    /// the cache the reading divides by. The pull is gated on the API gate having passed, and a
    /// document that answers without naming the model backs off far rather than re-pulling every
    /// minute for the life of the session.
    #[test]
    fn the_window_pull_reads_config_providers_through_the_real_client() {
        let server = spawn_fake_server();
        let client = Client::new(server.port, "pw");
        let mut fixture = ContextFixture::new();
        assert!(fixture.feed(OC_TURN_1_FINAL));
        assert_eq!(fixture.record().expect("a record").window_tokens, None);

        fixture.producer.refresh_windows(&client, false);
        assert_eq!(
            fixture.producer.window_wanted(),
            Some("opencode/hy3-free"),
            "the pull waits for the API gate"
        );

        fixture.producer.refresh_windows(&client, true);
        assert_eq!(fixture.producer.window_wanted(), None);
        assert!(fixture.feed(OC_TURN_2_FINAL));
        assert_eq!(
            fixture.record().expect("a record").window_tokens,
            Some(OC_WINDOW)
        );

        // A model the answered document does not name: the retry moves out to the long horizon
        // instead of re-pulling the whole document once a minute forever.
        let switched = OC_TURN_1_SESSION.replace(
            r#""model":{"id":"hy3-free","providerID":"opencode","variant":"default"}"#,
            r#""model":{"id":"big-pickle","providerID":"opencode","variant":"default"}"#,
        );
        fixture.feed(&switched);
        fixture.producer.refresh_windows(&client, true);
        assert_eq!(
            fixture.producer.window_wanted(),
            Some("opencode/big-pickle")
        );
        assert!(
            fixture.producer.next_providers_attempt
                > Instant::now() + PROVIDERS_UNLISTED_RETRY - Duration::from_secs(60),
            "an unlisted model must not keep the pull due every minute"
        );
    }

    /// A model change on `session.updated` re-opens the denominator question before any assistant
    /// message proves the new model, so the pull is due again — and it never becomes the reading's
    /// own model, which stays the one that produced the numerator.
    #[test]
    fn a_configured_model_change_makes_the_window_pull_due_without_retagging_the_reading() {
        let mut fixture = ContextFixture::new();
        fixture.producer.ingest_providers(&event(OC_PROVIDERS));
        fixture.feed(OC_TURN_1_FINAL);
        fixture.feed(OC_TURN_1_SESSION);
        assert_eq!(fixture.producer.window_wanted(), None);

        let switched = OC_TURN_1_SESSION.replace(
            r#""model":{"id":"hy3-free","providerID":"opencode","variant":"default"}"#,
            r#""model":{"id":"big-pickle","providerID":"opencode","variant":"default"}"#,
        );
        fixture.feed(&switched);
        assert_eq!(
            fixture.producer.window_wanted(),
            Some("opencode/big-pickle"),
            "a model the providers document does not name keeps the pull due"
        );
        let record = fixture.record().expect("a context record");
        assert_eq!(
            record.model.as_deref(),
            Some("opencode/hy3-free"),
            "the record pairs a numerator with the model that produced it"
        );
        assert_eq!(record.window_tokens, Some(OC_WINDOW));
    }
}
