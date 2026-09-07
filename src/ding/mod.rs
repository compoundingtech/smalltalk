//! Native inbox-to-terminal DING delivery.
//!
//! Fresh delivery first proves an empty maintained composer, bracketed-pastes without Return, then
//! requires two adjacent adapter observations to prove the exact retained composer is safe before
//! sending bare Return. Once paste starts, any command or receipt ambiguity retains staged
//! ownership. A later retry uses the same adjacent-observation requirement.
//!
//! Once a paste command starts, the sidecar owns that payload and retries by inspection only. It
//! never pastes the same notice again while that transport attempt remains owned.
//! PTY or Return success is not delivery: a harness adapter must positively classify the expected
//! notice text in its submitted-prompt or queued-message pattern while the live composer is empty.
//! This preserves FIFO/archive behavior without letting a command timeout create duplicate text.
//! Startup can adopt an exact staged recovery or backlog notice before coalescing remaining unread
//! work into one generic recovery DING. `busy` never suppresses a notification; fresh `dnd` does.

use std::collections::{HashSet, VecDeque};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

mod composer;
mod harness;

use crate::message::{self, Message};
use crate::run::{CAPTURE_CAP_BYTES, read_bounded_tail, reap_detached};
use crate::status;
use crate::supervisor_chain::{SUPERVISOR_CHAIN_LIMIT, chain_bus_ids, resolve_spec};

use composer::{ComposerState, classify_composer, classify_located_composer, classify_receipt};
use harness::ReceiptState;

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
const SUBJECT_MAX_CHARS: usize = 160;
const SENDER_MAX_CHARS: usize = 80;
/// The marker for a declared non-agent event source. A fixed st2-chosen literal — never
/// producer-supplied text — so the bounded-notice proofs are unaffected.
const SOURCE_MARKER: &str = "»";
const RECOVERY_POKE: &str = "[DING] unread st2 messages remain; check your inbox";
// Must exceed face607's bounded 0.5s delivery delay plus PTY/Node startup overhead; otherwise a
// successful pane write is misreported as a timeout and retried, duplicating the owned payload.
const PTY_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMPOSER_OBSERVATION_WINDOW: Duration = Duration::from_millis(450);
const COMPOSER_OBSERVATION_POLL: Duration = Duration::from_millis(10);
/// A human or active turn can keep a staged notice unsafe for minutes. Retrying `pty peek` every
/// inbox poll creates a short-lived child for each attempt, so keep the correctness fallback but
/// bound that descendant churn independently of the filesystem poll cadence.
const DELIVERY_RETRY_BACKOFF: Duration = Duration::from_secs(15);

/// The `<rand6>` of a `<unix-ms>-<rand6>.md` filename — the stable id an agent dedups re-pokes on.
/// Falls back to the `.md`-stripped stem for anything off-grammar.
pub fn poke_id(filename: &str) -> &str {
    if message::is_message_filename(filename) {
        // `13 digits` + `-` = 14 bytes of prefix, then the 6 rand chars.
        &filename[14..20]
    } else {
        filename.strip_suffix(".md").unwrap_or(filename)
    }
}

/// Convert arbitrary text into one printable line.
///
/// Every control or whitespace run becomes at most one ordinary space. Removing terminal control
/// bytes before bracketed-paste framing makes it impossible for an untrusted field to inject the
/// closing marker.
fn normalize_line(input: &str) -> String {
    let mut normalized = String::with_capacity(input.len());
    let mut pending_space = false;

    for ch in input.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.push(ch);
    }

    normalized
}

fn normalize_field(value: Option<&str>, fallback: &str, max_chars: usize) -> String {
    let normalized = normalize_line(value.unwrap_or_default());
    let bounded: String = normalized.chars().take(max_chars).collect();
    let bounded = bounded.trim_end();
    if bounded.is_empty() {
        fallback.to_string()
    } else {
        bounded.to_string()
    }
}

struct RelationshipResolver {
    specs: Vec<crate::AgentSpec>,
    valid: bool,
}

impl RelationshipResolver {
    fn read(catalog_root: &Path) -> Self {
        let discovered = crate::discover_strict(catalog_root);
        Self {
            specs: discovered.specs,
            valid: discovered.errors.is_empty(),
        }
    }
}

fn relationship_marker(
    resolver: &RelationshipResolver,
    this_host: &str,
    recipient: &str,
    claimed_sender: Option<&str>,
) -> String {
    if !resolver.valid {
        return "?".to_string();
    }
    let Some(sender) = claimed_sender.and_then(|id| resolve_spec(&resolver.specs, id, this_host))
    else {
        return "?".to_string();
    };
    let Some(recipient) = resolve_spec(&resolver.specs, recipient, this_host) else {
        return "?".to_string();
    };
    let sender_id = sender.bus_id(this_host);
    let recipient_id = recipient.bus_id(this_host);
    if sender_id == recipient_id {
        return "↺".to_string();
    }
    let Ok(recipient_chain) = chain_bus_ids(&resolver.specs, recipient, this_host) else {
        return "?".to_string();
    };
    let Ok(sender_chain) = chain_bus_ids(&resolver.specs, sender, this_host) else {
        return "?".to_string();
    };

    if let Some(depth) = recipient_chain.iter().position(|id| id == &sender_id)
        && depth > 0
    {
        return "↓".repeat(depth);
    }
    if let Some(depth) = sender_chain.iter().position(|id| id == &recipient_id)
        && depth > 0
    {
        return "↑".repeat(depth);
    }
    let recipient_ancestors = recipient_chain.iter().collect::<HashSet<_>>();
    if sender_chain
        .iter()
        .any(|ancestor| recipient_ancestors.contains(ancestor))
    {
        return "←".to_string();
    }
    "?".to_string()
}

/// The `[DING] …` line an agent sees for one newly arrived message. Consumers must key on the
/// prefix and stable id rather than descriptive words. Subject and sender are bounded, normalized
/// untrusted fields. The marker describes the relationship implied by the claimed sender identity;
/// it does not authenticate that identity.
pub fn poke_text(catalog_root: &Path, this_host: &str, recipient: &str, msg: &Message) -> String {
    poke_text_with_resolver(
        &RelationshipResolver::read(catalog_root),
        this_host,
        recipient,
        msg,
    )
}

fn poke_text_with_resolver(
    resolver: &RelationshipResolver,
    this_host: &str,
    recipient: &str,
    msg: &Message,
) -> String {
    let subject = normalize_field(msg.subject.as_deref(), "(no subject)", SUBJECT_MAX_CHARS);
    let from = normalize_field(msg.from.as_deref(), "unknown", SENDER_MAX_CHARS);
    let marker = if msg.stream.is_some() && msg.event_id.is_some() {
        SOURCE_MARKER.to_string()
    } else {
        relationship_marker(resolver, this_host, recipient, msg.from.as_deref())
    };
    format!(
        "[DING] {marker} {from}: {subject} [id:{}]",
        poke_id(&msg.filename)
    )
}

fn bracketed_paste(text: &str) -> String {
    let normalized = normalize_line(text);
    format!("{BRACKETED_PASTE_START}{normalized}{BRACKETED_PASTE_END}")
}

/// Bracketed-paste one normalized notice without Return.
pub fn pty_stage_args(session: &str, text: &str) -> Vec<String> {
    vec![
        "send".into(),
        session.into(),
        "--seq".into(),
        bracketed_paste(text),
    ]
}

/// Submit a composer that two immediately adjacent inspections proved contains the exact notice.
pub fn pty_submit_args(session: &str) -> Vec<String> {
    vec![
        "send".into(),
        session.into(),
        "--seq".into(),
        "key:return".into(),
    ]
}

/// Recovery transport: one bounded PTY transaction containing paste and Return.
///
/// A successful command proves only that the PTY accepted the input sequence. [`PokeOutcome::Delivered`]
/// still requires a separate harness receipt for the exact notice.
pub fn pty_delivery_args(session: &str, text: &str) -> Vec<String> {
    vec![
        "send".into(),
        session.into(),
        "--with-delay".into(),
        "0.5".into(),
        "--seq".into(),
        bracketed_paste(text),
        "--seq".into(),
        "key:return".into(),
    ]
}

/// One delivery attempt either has positive harness acceptance for the exact notice, owns an
/// ambiguous or retained payload that must be retried by inspection only, or performed no input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PokeOutcome {
    Delivered,
    Staged,
    /// A maintained adapter positively proved that the exact staged notice is absent. Queue state
    /// decides whether an archive receipt makes that proof sufficient to relinquish ownership.
    NotRetained,
    Deferred(DeferralReason),
}

/// Why one attempt performed no input at all.
///
/// A deferral is the one outcome that both delivers nothing and leaves nothing behind, so it is
/// the one that must say why. The two composer verdicts are deliberately distinct: a human drafting
/// in a harness we understand is a wait, while a pane no maintained harness can locate is a gap in
/// coverage that will never resolve on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferralReason {
    /// The lowest maintained composer holds text that is not the exact notice — typically a human
    /// draft. Named by harness only: the text on that pane belongs to whoever is typing it.
    ComposerChanged { harness: &'static str },
    /// A maintained harness located its composer but proved nothing about this screen — an active
    /// turn, a modal, or a footer it does not recognise. Distinct from an unlocatable pane: this
    /// one is covered and can clear on its own, so it is a wait rather than a coverage gap.
    ComposerUnproven { harness: &'static str },
    /// No maintained harness could locate a composer on this pane, so nothing is proven either
    /// way. An unrecognised, resized, or not-yet-drawn TUI lands here.
    NoMaintainedComposer,
    /// The poker performs no input of its own; delivery is somebody else's job.
    NoInputPerformed,
}

impl std::fmt::Display for DeferralReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ComposerChanged { harness } => write!(
                formatter,
                "the {harness} composer holds other text (a draft or an unfinished turn); waiting rather than typing over it"
            ),
            Self::ComposerUnproven { harness } => write!(
                formatter,
                "the {harness} composer was located but proved nothing about this screen (an active turn, a modal, or an unrecognised footer); waiting for it to settle"
            ),
            Self::NoMaintainedComposer => formatter.write_str(
                "no maintained harness could locate a composer on this pane; nothing will be delivered here until one can",
            ),
            Self::NoInputPerformed => formatter.write_str("this poker performs no input"),
        }
    }
}

/// Whether a deferral is news, so a pane stuck in one verdict costs one log line rather than one
/// per retry. The first deferral after progress is news, a changed verdict is news, and the same
/// verdict repeating is the same fact.
#[derive(Debug, Default)]
pub struct DeferralJournal {
    last: Option<DeferralReason>,
}

impl DeferralJournal {
    pub fn observe(&mut self, current: Option<DeferralReason>) -> bool {
        if self.last == current {
            return false;
        }
        self.last = current;
        current.is_some()
    }
}

/// What one `flush_pending` chose not to do. Deliberately a plain struct rather than an
/// `Option`: callers that only want the flush keep compiling as statements.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlushReport {
    /// Set when the front notice performed no input this pass.
    pub deferred: Option<DeferralReason>,
}

/// How DING delivers a poke and checks liveness, abstracted so the watch loop is testable without a
/// real `pty`.
pub trait Poker {
    fn poke(&self, text: &str) -> anyhow::Result<PokeOutcome>;
    fn retry_staged(&self, _text: &str) -> anyhow::Result<PokeOutcome> {
        Ok(PokeOutcome::Deferred(DeferralReason::NoInputPerformed))
    }
    fn adopt_staged(&self, _candidates: &[String]) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn session_alive(&self) -> bool;
}

/// Production [`Poker`]: shells out to the sibling `pty` binary and probes its pidfile for liveness.
pub struct PtyPoker {
    bin: String,
    session: String,
}

impl PtyPoker {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            bin: "pty".to_string(),
            session: session.into(),
        }
    }

    fn run(&self, args: Vec<String>, operation: &str) -> anyhow::Result<()> {
        let out = output_with_timeout(Command::new(&self.bin).args(args), PTY_COMMAND_TIMEOUT)?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty {operation} {}` failed: {}",
                self.session,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Reads the terminal screen of the session. Output capture is tail-capped at
    /// [`crate::run::CAPTURE_CAP_BYTES`]; semantics are preserved because a terminal screen is
    /// far below that bound.
    fn peek(&self) -> anyhow::Result<String> {
        let out = output_with_timeout(
            Command::new(&self.bin).args(["peek", self.session.as_str()]),
            PTY_COMMAND_TIMEOUT,
        )?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty peek {}` failed: {}",
                self.session,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        String::from_utf8(out.stdout).map_err(|error| {
            anyhow::anyhow!("`pty peek {}` returned non-UTF-8: {error}", self.session)
        })
    }

    /// Central production path shared by inbox DING and any caller that needs to record an attempt
    /// immediately before the command containing Return.
    pub fn poke_with(
        &self,
        text: &str,
        before_submit: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<PokeOutcome> {
        observed_poke_with_window(
            text,
            &mut || self.peek(),
            &mut || self.run(pty_stage_args(&self.session, text), "send"),
            &mut || self.run(pty_submit_args(&self.session), "send"),
            &mut || thread::sleep(COMPOSER_OBSERVATION_POLL),
            before_submit,
            COMPOSER_OBSERVATION_WINDOW,
        )
    }
}

impl Poker for PtyPoker {
    fn poke(&self, text: &str) -> anyhow::Result<PokeOutcome> {
        self.poke_with(text, &mut || Ok(()))
    }

    fn retry_staged(&self, text: &str) -> anyhow::Result<PokeOutcome> {
        retry_staged_with_window(
            text,
            &mut || self.peek(),
            &mut || self.run(pty_submit_args(&self.session), "send"),
            &mut || thread::sleep(COMPOSER_OBSERVATION_POLL),
            &mut || Ok(()),
            COMPOSER_OBSERVATION_WINDOW,
        )
    }

    fn adopt_staged(&self, candidates: &[String]) -> anyhow::Result<Option<String>> {
        let screen = self.peek()?;
        Ok(exact_staged_candidate(&screen, candidates))
    }

    fn session_alive(&self) -> bool {
        session_alive(&self.session)
    }
}

/// Run a non-interactive child with bounded output capture: each stream keeps at most its last
/// [`crate::run::CAPTURE_CAP_BYTES`] bytes (tail-preserving, with a diagnostic line on
/// truncation). Temporary files keep an escaped descendant that inherited stdout/stderr from
/// blocking cleanup after the direct child times out.
fn output_with_timeout(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id() as i32;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = child.kill();
            reap_detached(child);
            anyhow::bail!("timed out after {:.1}s", timeout.as_secs_f64());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout_stream = read_bounded_tail(&mut stdout, CAPTURE_CAP_BYTES)?;
    let stderr_stream = read_bounded_tail(&mut stderr, CAPTURE_CAP_BYTES)?;
    let program = command.get_program().to_string_lossy();
    for (stream, name) in [(&stdout_stream, "stdout"), (&stderr_stream, "stderr")] {
        if stream.truncated() {
            eprintln!(
                "st2: truncated {name} capture of `{program}`: keeping last {} of {} bytes (cap {CAPTURE_CAP_BYTES})",
                stream.bytes.len(),
                stream.total,
            );
        }
    }
    Ok(Output {
        status,
        stdout: stdout_stream.bytes,
        stderr: stderr_stream.bytes,
    })
}

fn exact_staged_candidate(screen: &str, candidates: &[String]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        matches!(
            classify_composer(screen, candidate),
            ComposerState::ExactSafe | ComposerState::ExactBlocked
        )
        .then(|| candidate.clone())
    })
}

#[cfg(test)]
fn transport_and_observe_with_window(
    text: &str,
    transport: &mut dyn FnMut() -> anyhow::Result<()>,
    peek: &mut dyn FnMut() -> anyhow::Result<String>,
    poll: &mut dyn FnMut(),
    before_submit: &mut dyn FnMut() -> anyhow::Result<()>,
    observation_window: Duration,
) -> anyhow::Result<PokeOutcome> {
    before_submit()?;
    // Preserve the accepted transport-first transaction. Once it starts, any command or
    // observation failure is ambiguous: the paste may have landed even if Return did not.
    if let Err(error) = transport() {
        tracing::warn!(
            "st2 ding: DING transport became ambiguous; retaining staged ownership: {error}"
        );
        return Ok(PokeOutcome::Staged);
    }
    observe_receipt_with_window(text, peek, poll, observation_window)
}

/// Observe one bounded post-submit window. PTY success, disappearance, and generic screen change
/// are not receipts; only the adapter's positive accepted-pattern classification completes
/// delivery.
fn observe_receipt_with_window(
    text: &str,
    peek: &mut dyn FnMut() -> anyhow::Result<String>,
    poll: &mut dyn FnMut(),
    observation_window: Duration,
) -> anyhow::Result<PokeOutcome> {
    let deadline = Instant::now() + observation_window;
    loop {
        let screen = match peek() {
            Ok(screen) => screen,
            Err(error) => {
                tracing::warn!(
                    "st2 ding: post-submit receipt observation failed; retaining staged ownership: {error}"
                );
                return Ok(PokeOutcome::Staged);
            }
        };
        if classify_receipt(&screen, text) == ReceiptState::Accepted {
            return Ok(PokeOutcome::Delivered);
        }
        if Instant::now() >= deadline {
            return Ok(PokeOutcome::Staged);
        }
        poll();
    }
}

/// Inspect-only retry for a transport-owned payload. It never pastes; one bare Return is allowed
/// only after two adjacent adapter observations prove the exact retained composer is safe.
fn retry_staged_with_window(
    text: &str,
    peek: &mut dyn FnMut() -> anyhow::Result<String>,
    submit: &mut dyn FnMut() -> anyhow::Result<()>,
    poll: &mut dyn FnMut(),
    before_submit: &mut dyn FnMut() -> anyhow::Result<()>,
    observation_window: Duration,
) -> anyhow::Result<PokeOutcome> {
    let screen = match peek() {
        Ok(screen) => screen,
        Err(error) => {
            tracing::warn!(
                "st2 ding: staged retry observation failed; retaining ownership: {error}"
            );
            return Ok(PokeOutcome::Staged);
        }
    };
    match classify_receipt(&screen, text) {
        ReceiptState::Accepted => Ok(PokeOutcome::Delivered),
        ReceiptState::RetainedSafe => submit_retained_after_final_observation(
            text,
            peek,
            submit,
            poll,
            before_submit,
            observation_window,
        ),
        ReceiptState::NotRetained => Ok(PokeOutcome::NotRetained),
        ReceiptState::RetainedBlocked | ReceiptState::Unproven => Ok(PokeOutcome::Staged),
    }
}

fn submit_retained_after_final_observation(
    text: &str,
    peek: &mut dyn FnMut() -> anyhow::Result<String>,
    submit: &mut dyn FnMut() -> anyhow::Result<()>,
    poll: &mut dyn FnMut(),
    before_submit: &mut dyn FnMut() -> anyhow::Result<()>,
    observation_window: Duration,
) -> anyhow::Result<PokeOutcome> {
    let screen = match peek() {
        Ok(screen) => screen,
        Err(error) => {
            tracing::warn!(
                "st2 ding: final retained-composer observation failed; retaining ownership: {error}"
            );
            return Ok(PokeOutcome::Staged);
        }
    };
    match classify_receipt(&screen, text) {
        ReceiptState::Accepted => return Ok(PokeOutcome::Delivered),
        ReceiptState::RetainedSafe => {}
        ReceiptState::NotRetained => return Ok(PokeOutcome::NotRetained),
        ReceiptState::RetainedBlocked | ReceiptState::Unproven => {
            return Ok(PokeOutcome::Staged);
        }
    }
    if let Err(error) = before_submit() {
        tracing::warn!("st2 ding: pre-submit receipt failed; retaining staged ownership: {error}");
        return Ok(PokeOutcome::Staged);
    }
    if let Err(error) = submit() {
        tracing::warn!(
            "st2 ding: Return command became ambiguous; retaining staged ownership: {error}"
        );
        return Ok(PokeOutcome::Staged);
    }
    observe_receipt_with_window(text, peek, poll, observation_window)
}

/// Two-phase DING delivery with injected operations for deterministic regression tests.
fn observed_poke_with_window(
    text: &str,
    peek: &mut dyn FnMut() -> anyhow::Result<String>,
    stage: &mut dyn FnMut() -> anyhow::Result<()>,
    submit: &mut dyn FnMut() -> anyhow::Result<()>,
    poll: &mut dyn FnMut(),
    before_submit: &mut dyn FnMut() -> anyhow::Result<()>,
    observation_window: Duration,
) -> anyhow::Result<PokeOutcome> {
    let (state, harness) = classify_located_composer(&peek()?, text);
    match state {
        ComposerState::ExactSafe => {
            return submit_after_final_observation(
                text,
                peek,
                submit,
                poll,
                before_submit,
                observation_window,
            );
        }
        ComposerState::ExactBlocked => return Ok(PokeOutcome::Staged),
        ComposerState::EmptySafe => {}
        ComposerState::Changed | ComposerState::Ambiguous => {
            // Whether a harness was located is the difference between a wait and a coverage gap,
            // so it decides the reason rather than being folded into one catch-all.
            return Ok(PokeOutcome::Deferred(match (state, harness) {
                (ComposerState::Changed, Some(harness)) => {
                    DeferralReason::ComposerChanged { harness }
                }
                // `Changed` cannot arise without a located harness — only a located composer can
                // be read as holding other text — but the classifier owns that invariant, not
                // this call site, so an unlocated pane is reported as exactly what was observed.
                (_, Some(harness)) => DeferralReason::ComposerUnproven { harness },
                (_, None) => DeferralReason::NoMaintainedComposer,
            }));
        }
    }

    // Once this command starts, success is ambiguous on any error or timeout: the paste may already
    // have reached the TUI. Preserve ownership and let retry_staged inspect instead of re-pasting.
    if let Err(error) = stage() {
        tracing::warn!(
            "st2 ding: paste command became ambiguous; retaining staged ownership: {error}"
        );
        return Ok(PokeOutcome::Staged);
    }

    let deadline = Instant::now() + observation_window;
    loop {
        let screen = match peek() {
            Ok(screen) => screen,
            Err(error) => {
                tracing::warn!(
                    "st2 ding: post-paste observation failed; retaining staged ownership: {error}"
                );
                return Ok(PokeOutcome::Staged);
            }
        };
        match classify_composer(&screen, text) {
            ComposerState::ExactSafe => {
                return submit_after_final_observation(
                    text,
                    peek,
                    submit,
                    poll,
                    before_submit,
                    observation_window,
                );
            }
            ComposerState::ExactBlocked => return Ok(PokeOutcome::Staged),
            ComposerState::Changed => return Ok(PokeOutcome::Staged),
            ComposerState::EmptySafe | ComposerState::Ambiguous => {}
        }
        if Instant::now() >= deadline {
            return Ok(PokeOutcome::Staged);
        }
        poll();
    }
}

/// The final observation is intentionally adjacent to the bare-Return operation. Any change or
/// uncertainty after the first exact observation prevents submission.
fn submit_after_final_observation(
    text: &str,
    peek: &mut dyn FnMut() -> anyhow::Result<String>,
    submit: &mut dyn FnMut() -> anyhow::Result<()>,
    poll: &mut dyn FnMut(),
    before_submit: &mut dyn FnMut() -> anyhow::Result<()>,
    observation_window: Duration,
) -> anyhow::Result<PokeOutcome> {
    let screen = match peek() {
        Ok(screen) => screen,
        Err(error) => {
            tracing::warn!(
                "st2 ding: final composer observation failed; retaining staged ownership: {error}"
            );
            return Ok(PokeOutcome::Staged);
        }
    };
    match classify_composer(&screen, text) {
        ComposerState::ExactSafe => {}
        ComposerState::ExactBlocked | ComposerState::Ambiguous => {
            return Ok(PokeOutcome::Staged);
        }
        ComposerState::EmptySafe | ComposerState::Changed => {
            return Ok(PokeOutcome::Staged);
        }
    }
    if let Err(error) = before_submit() {
        tracing::warn!("st2 ding: pre-submit receipt failed; retaining staged ownership: {error}");
        return Ok(PokeOutcome::Staged);
    }
    if let Err(error) = submit() {
        tracing::warn!(
            "st2 ding: Return command became ambiguous; retaining staged ownership: {error}"
        );
        return Ok(PokeOutcome::Staged);
    }
    observe_receipt_with_window(text, peek, poll, observation_window)
}

/// `<pty-session-dir>/<session>.pid` + `kill(pid, 0)`; any miss means gone. This mirrors the
/// session registry's own liveness probe without forking `pty`.
pub fn session_alive(session: &str) -> bool {
    let pidfile = pty_session_dir().join(format!("{session}.pid"));
    let Ok(raw) = std::fs::read_to_string(&pidfile) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    // Signal 0 probes existence and permission without delivering a signal.
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

/// Positive-evidence session liveness for observed-harness-state readers. Unlike
/// [`session_alive`], whose delivery callers must fail closed ("any miss means gone"), a reader
/// deriving `unknown` needs proof of death: an unreadable or unparseable pidfile is
/// `Indeterminate` — the reader may not share the writer's PTY root — and a pid that exists but
/// is not signalable (EPERM) is still alive.
pub fn session_liveness(session: &str) -> crate::harness_state::SessionLiveness {
    session_liveness_in(&pty_session_dir(), session)
}

/// [`session_liveness`], probing an explicit registry root instead of the ambient environment —
/// readers that know the catalog derive the runner's own root rather than requiring PTY_ROOT in
/// the shell.
pub fn session_liveness_in(
    root: &std::path::Path,
    session: &str,
) -> crate::harness_state::SessionLiveness {
    use crate::harness_state::SessionLiveness;
    let pidfile = root.join(format!("{session}.pid"));
    let Ok(raw) = std::fs::read_to_string(&pidfile) else {
        return SessionLiveness::Indeterminate;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return SessionLiveness::Indeterminate;
    };
    if pid <= 0 {
        return SessionLiveness::Indeterminate;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return SessionLiveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::ESRCH => SessionLiveness::Dead,
        // EPERM proves existence; anything else proves nothing.
        Some(code) if code == libc::EPERM => SessionLiveness::Alive,
        _ => SessionLiveness::Indeterminate,
    }
}

/// The `pty` session registry dir. This must mirror the sibling tool's resolution order.
fn pty_session_dir() -> PathBuf {
    for var in ["PTY_ROOT", "PTY_SESSION_DIR"] {
        if let Ok(directory) = std::env::var(var)
            && !directory.is_empty()
        {
            return PathBuf::from(directory);
        }
    }
    home_dir().join(".local").join("state").join("pty")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Refuse to start if the `pty` binary is unreachable.
pub fn probe_pty_on_path() -> anyhow::Result<()> {
    match output_with_timeout(Command::new("pty").arg("--help"), PTY_COMMAND_TIMEOUT) {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => anyhow::bail!("`pty --help` exited {}", out.status),
        Err(error) => anyhow::bail!("`pty` not runnable on PATH: {error}"),
    }
}

/// Logically unread messages in `inbox_dir` not in `seen`, in send order, while updating `seen` to
/// exactly the current unread set. A same-named archive receipt suppresses and cleans a restored raw
/// inbox copy. The first call returns the whole backlog; the sidecar coalesces it into one recovery
/// notice.
pub fn new_arrivals(inbox_dir: &Path, seen: &mut HashSet<String>) -> Vec<Message> {
    let messages = message::list_inbox(inbox_dir).unwrap_or_default();
    let current: HashSet<&str> = messages
        .iter()
        .map(|message| message.filename.as_str())
        .collect();
    seen.retain(|filename| current.contains(filename.as_str()));
    messages
        .into_iter()
        .filter(|message| seen.insert(message.filename.clone()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingNotice {
    Recovery {
        startup: HashSet<String>,
        in_inbox: bool,
        staged_text: Option<String>,
    },
    Message {
        message: Message,
        in_inbox: bool,
        staged_text: Option<String>,
    },
    /// Exact composer text adopted on sidecar startup. The ordinary recovery notice remains behind
    /// it so any other unread backlog is still coalesced after this owned payload resolves.
    Adopted { staged_text: Option<String> },
}

impl PendingNotice {
    fn text(
        &self,
        context: DingContext<'_>,
        resolver: &mut Option<RelationshipResolver>,
    ) -> String {
        match self {
            Self::Recovery { .. } => RECOVERY_POKE.to_string(),
            Self::Message { message, .. } => poke_text_with_resolver(
                resolver.get_or_insert_with(|| RelationshipResolver::read(context.catalog_root)),
                context.this_host,
                context.recipient,
                message,
            ),
            Self::Adopted {
                staged_text: Some(text),
            } => text.clone(),
            Self::Adopted { staged_text: None } => String::new(),
        }
    }

    fn staged_text(&self) -> Option<&str> {
        match self {
            Self::Recovery { staged_text, .. }
            | Self::Message { staged_text, .. }
            | Self::Adopted { staged_text } => staged_text.as_deref(),
        }
    }

    fn set_staged_text(&mut self, value: Option<String>) {
        match self {
            Self::Recovery { staged_text, .. }
            | Self::Message { staged_text, .. }
            | Self::Adopted { staged_text } => *staged_text = value,
        }
    }

    fn in_inbox(&self) -> bool {
        match self {
            Self::Recovery { in_inbox, .. } | Self::Message { in_inbox, .. } => *in_inbox,
            Self::Adopted { .. } => false,
        }
    }

    fn is_archived(&self) -> bool {
        match self {
            Self::Recovery { in_inbox, .. } | Self::Message { in_inbox, .. } => !*in_inbox,
            Self::Adopted { .. } => false,
        }
    }

    fn adopted(text: String) -> Self {
        Self::Adopted {
            staged_text: Some(text),
        }
    }

    fn message(message: Message) -> Self {
        Self::Message {
            message,
            in_inbox: true,
            staged_text: None,
        }
    }
}

/// Consecutive liveness misses tolerated after a session has first been observed alive.
const SESSION_GONE_DEBOUNCE_MISSES: u32 = 3;

#[derive(Default)]
struct SessionWatch {
    /// A startup miss is not terminal: the sidecar may register before its target session.
    seen_alive: bool,
    misses: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum WatchStep {
    Poll,
    Gone,
}

impl SessionWatch {
    fn step(&mut self, alive: bool) -> WatchStep {
        if alive {
            self.seen_alive = true;
            self.misses = 0;
            return WatchStep::Poll;
        }
        if !self.seen_alive {
            return WatchStep::Poll;
        }
        self.misses += 1;
        if self.misses >= SESSION_GONE_DEBOUNCE_MISSES {
            WatchStep::Gone
        } else {
            WatchStep::Poll
        }
    }
}

/// Tunables for the watch loop.
pub struct DingConfig {
    /// Fallback poll cadence and liveness-check cadence.
    pub poll: Duration,
    /// Presence refresh cadence while the target session is alive.
    pub status_refresh: Duration,
}

/// Catalog coordinates needed to classify a claimed sender relative to the DING recipient.
#[derive(Clone, Copy)]
pub struct DingContext<'a> {
    /// Catalog whose Agent Specs define the supervision graph.
    pub catalog_root: &'a Path,
    /// Local host used to resolve hostless specs and bare identities.
    pub this_host: &'a str,
    /// Identity whose inbox the sidecar watches.
    pub recipient: &'a str,
}

impl Default for DingConfig {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(1000),
            status_refresh: status::STATUS_REFRESH,
        }
    }
}

/// Watch `inbox_dir` and notify until stopped or the target session is gone.
///
/// Existing unread contents become one generic recovery DING. New arrivals remain FIFO-queued
/// across fresh `dnd` status and transport failures; `busy` does not suppress delivery. Archive
/// receipts prune queued work before delivery.
pub fn run_ding(
    context: DingContext<'_>,
    inbox_dir: &Path,
    status_path: Option<&Path>,
    poker: &dyn Poker,
    config: &DingConfig,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    // Arm the watcher before seeding. The timer remains the correctness fallback if watching fails.
    // The subscription is the inbox itself, never a recursive walk over the agent dir: Resource
    // payload trees live under `resources/` beside the inbox, and eager registration would pay
    // one inotify watch per payload directory before any filtering ever ran. The inbox is
    // created up front so the watch anchors on it directly (senders create it on demand anyway).
    let _ = std::fs::create_dir_all(inbox_dir);
    let (tx, rx) = channel::<()>();
    let _watcher = crate::watch::watch_recursive_mutations(inbox_dir, tx);

    let mut seen = HashSet::new();
    let backlog = new_arrivals(inbox_dir, &mut seen);
    let mut startup_candidates = (!backlog.is_empty()).then(|| {
        let resolver = RelationshipResolver::read(context.catalog_root);
        std::iter::once(RECOVERY_POKE.to_string())
            .chain(backlog.iter().map(|message| {
                poke_text_with_resolver(&resolver, context.this_host, context.recipient, message)
            }))
            .collect::<Vec<_>>()
    });
    let mut pending = VecDeque::new();
    if !backlog.is_empty() {
        pending.push_back(PendingNotice::Recovery {
            startup: backlog
                .iter()
                .map(|message| message.filename.clone())
                .collect(),
            in_inbox: true,
            staged_text: None,
        });
    }
    eprintln!(
        "st2 ding: ready — found {} existing unread message(s){}; watching for new arrivals.",
        backlog.len(),
        if backlog.is_empty() {
            ""
        } else {
            " and queued one recovery notice"
        }
    );

    let mut watch = SessionWatch::default();
    let mut logged_waiting = false;
    let mut last_refresh: Option<Instant> = None;
    let mut next_delivery_attempt: Option<Instant> = None;
    let mut deferrals = DeferralJournal::default();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let alive = poker.session_alive();
        if watch.step(alive) == WatchStep::Gone {
            eprintln!("st2 ding: target pty session is gone — exiting.");
            break;
        }

        if alive {
            if let Some(path) = status_path
                && last_refresh.is_none_or(|instant| instant.elapsed() >= config.status_refresh)
            {
                let _ = status::refresh(path);
                last_refresh = Some(Instant::now());
            }

            pending.extend(
                new_arrivals(inbox_dir, &mut seen)
                    .into_iter()
                    .map(PendingNotice::message),
            );
            prune_archived_pending(inbox_dir, &mut pending);

            let delivery_due =
                next_delivery_attempt.is_none_or(|deadline| Instant::now() >= deadline);
            if delivery_due && !delivery_suppressed(status_path) {
                if let Some(candidates) = startup_candidates.as_ref() {
                    match poker.adopt_staged(candidates) {
                        Ok(Some(text)) => {
                            if text == RECOVERY_POKE {
                                if let Some(recovery) = pending
                                    .iter_mut()
                                    .find(|notice| matches!(notice, PendingNotice::Recovery { .. }))
                                {
                                    recovery.set_staged_text(Some(text));
                                }
                            } else {
                                pending.push_front(PendingNotice::adopted(text));
                            }
                            startup_candidates = None;
                        }
                        Ok(None) => startup_candidates = None,
                        Err(error) => {
                            tracing::warn!(
                                "st2 ding: startup staged-notice adoption failed: {error}"
                            )
                        }
                    }
                }
                let report = flush_pending(context, status_path, &mut pending, poker);
                if deferrals.observe(report.deferred)
                    && let Some(reason) = report.deferred
                {
                    tracing::warn!(
                        "st2 ding: delivery deferred for '{}', no input performed: {reason}",
                        context.recipient
                    );
                }
                next_delivery_attempt = (startup_candidates.is_some() || !pending.is_empty())
                    .then(|| Instant::now() + DELIVERY_RETRY_BACKOFF);
            }
        } else if !watch.seen_alive && !logged_waiting {
            eprintln!(
                "st2 ding: target pty session not yet registered; waiting before enabling exit-when-gone."
            );
            logged_waiting = true;
        }

        match rx.recv_timeout(config.poll) {
            Ok(()) => drain(&rx),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(config.poll);
            }
        }
    }

    Ok(())
}

fn prune_archived_pending(inbox_dir: &Path, pending: &mut VecDeque<PendingNotice>) {
    let Ok(current) = message::list_inbox(inbox_dir) else {
        return;
    };
    let filenames: HashSet<&str> = current
        .iter()
        .map(|message| message.filename.as_str())
        .collect();
    for notice in pending.iter_mut() {
        match notice {
            PendingNotice::Recovery {
                startup, in_inbox, ..
            } => {
                *in_inbox = startup
                    .iter()
                    .any(|filename| filenames.contains(filename.as_str()));
            }
            PendingNotice::Message {
                message, in_inbox, ..
            } => {
                *in_inbox = filenames.contains(message.filename.as_str());
            }
            PendingNotice::Adopted { .. } => {}
        }
    }
    // A paste that already started stays owned across an archive race. It is never pasted again;
    // the inspect-only retry either submits the exact safe payload or proves ownership disappeared.
    pending.retain(|notice| notice.staged_text().is_some() || notice.in_inbox());
}

fn delivery_suppressed(status_path: Option<&Path>) -> bool {
    status_path.is_some_and(|path| status::read_state(path) == status::State::Dnd)
}

fn flush_pending(
    context: DingContext<'_>,
    status_path: Option<&Path>,
    pending: &mut VecDeque<PendingNotice>,
    poker: &dyn Poker,
) -> FlushReport {
    let mut report = FlushReport::default();
    if delivery_suppressed(status_path) {
        return report;
    }

    let mut resolver = None;

    while let Some(notice) = pending.front_mut() {
        let staged = notice.staged_text().map(str::to_string);
        let was_staged = staged.is_some();
        let text = staged.unwrap_or_else(|| notice.text(context, &mut resolver));
        let outcome = if was_staged {
            poker.retry_staged(&text)
        } else {
            poker.poke(&text)
        };
        match outcome {
            Ok(PokeOutcome::Delivered) => {
                pending.pop_front();
            }
            Ok(PokeOutcome::Staged) => {
                notice.set_staged_text(Some(text));
                break;
            }
            Ok(PokeOutcome::NotRetained) if was_staged && notice.is_archived() => {
                pending.pop_front();
            }
            Ok(PokeOutcome::NotRetained) => {
                notice.set_staged_text(Some(text));
                break;
            }
            Ok(PokeOutcome::Deferred(_)) if was_staged => {
                // The exact owned payload disappeared or changed. Adopted startup text has the
                // generic recovery notice behind it, while unread ordinary work may make one later
                // fresh guarded attempt. Archived work is done.
                notice.set_staged_text(None);
                if notice.in_inbox() {
                    break;
                }
                pending.pop_front();
            }
            // The one outcome that delivers nothing and leaves nothing behind. Report it out so
            // the watch loop can say so; an unreported break here is how an eleven-day fleet-wide
            // delivery failure stayed invisible.
            Ok(PokeOutcome::Deferred(reason)) => {
                report.deferred = Some(reason);
                break;
            }
            Err(error) => {
                tracing::warn!("st2 ding: {error}");
                break;
            }
        }
    }
    report
}

/// Set by SIGINT/SIGTERM so `st2 ding` exits cleanly when st2 tears the sidecar down.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handler() {
    let handler = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

/// Boot and run the sidecar, refreshing presence while the target pty is alive.
pub fn serve(
    catalog_root: &Path,
    this_host: &str,
    recipient: &str,
    inbox_dir: &Path,
    status_path: &Path,
    session: &str,
    config: &DingConfig,
) -> anyhow::Result<()> {
    probe_pty_on_path()?;
    install_signal_handler();
    run_ding(
        DingContext {
            catalog_root,
            this_host,
            recipient,
        },
        inbox_dir,
        Some(status_path),
        &PtyPoker::new(session),
        config,
        &STOP,
    )
}

fn drain(rx: &Receiver<()>) {
    while rx.try_recv().is_ok() {}
}

#[cfg(test)]
mod tests;
