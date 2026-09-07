//! Minimal pi channel watcher.
//!
//! The inbox is the durable source of truth. This process keeps only an ephemeral set of filenames
//! delivered during its current lifetime; a restart scans the inbox again. It is spawned by the
//! shipped pi extension and owned by it over stdio, so EOF on stdin is the session-lifetime
//! boundary. The outer pi session wrapper owns presence, because pi can outlive a failed extension.
//!
//! The wire is newline-delimited JSON in both directions. st2 — not the extension — decides how a
//! delivered message is handed to the agent, so the delivery mode travels on the frame: changing
//! that policy is a Rust change, not a redeploy of a TypeScript asset.

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::{context, driver_diagnostic, harness_context, harness_state, message};

const POLL: Duration = Duration::from_millis(250);

/// How old durable working state may be and still be restored, matching the lifecycle hooks'
/// `ST_REHYDRATE_STALE_S` default. Stale state is worse than none: it describes a world the agent
/// has already left.
const CONTEXT_MAX_AGE: Duration = Duration::from_secs(86_400);

/// The boot instruction every maintained harness restores verbatim. It is the shipped bus contract:
/// declare presence, then drain the inbox.
const RITUAL: &str = "Run the st2 boot ritual now: set your status to available, then drain your \
inbox by reading, acting on, replying when useful, and archiving each handled message. Before \
resuming or starting work, set your status to busy; set available only when yielding or ready for \
new work.";

/// The wire version the shipped extension is written against. A mismatch is the extension's to
/// refuse: st2 never guesses what an older asset understands.
pub const PROTOCOL: u32 = 1;

/// Last-resort durable state when compaction begins before the agent authored a checkpoint.
///
/// The stable text deliberately carries no extension-owned path or clock. Rust owns both the
/// canonical context path and its atomic writer; the file mtime supplies freshness.
const PRE_COMPACT_STUB: &str = "# now — pre-compact stub\n\n\
PreCompact fired before the model captured durable working state. Reconstruct from git status,\n\
recent commits, and the st2 inbox, then write a real checkpoint with `st2 context write`.\n";
const PRE_COMPACT_ERROR_REASON: &str = "pre-compact context recovery failed";

/// How pi is asked to hand one delivered message to the agent.
///
/// `steer` is the only value st2 currently emits. It is the earliest point at which pi accepts
/// input without discarding the running turn, and the same choice the Codex native path makes when
/// it routes an active turn to a steer. Holding the message until the agent settles instead would
/// defer delivery inside pi, where st2 cannot see it, which is what the "`busy` delivers
/// immediately" rule exists to prevent. Per-message selection is tracked in #277.
///
/// The boundary is finer than "after the current turn's tool calls" suggests. Measured against a
/// live provider: each tool call and its result form their own assistant message, so a steer sent
/// during a four-step job landed on the *first* tool-result boundary — in the same millisecond as
/// that result, after waiting out only the remainder of the one in-flight call. Steer latency is
/// therefore bounded by a single tool call's duration, not by the length of the job. What is not
/// guaranteed is that displaced work resumes: the model chose to continue, once, on one model.
const DELIVER_AS: &str = "steer";

fn channel_content(subject: Option<&str>, body: &str) -> String {
    match subject.filter(|value| !value.is_empty()) {
        Some(subject) => format!("Subject: {subject}\n\n{body}"),
        None => body.to_owned(),
    }
}

/// The harness-specific facts the shared channel loop needs: which env names carry the wrapper's
/// exported ownership triple, what label goes on records and errors, and which native-driver
/// diagnostic word — if any — this channel publishes under.
pub struct ChannelKind {
    pub label: &'static str,
    /// Which producer row of the harness-context table these numbers come from. It is the record's
    /// only discriminator, and the two kinds genuinely differ: pi's `tokens` is the last assistant
    /// message's `totalTokens`, omp's is its prompt-only `input`. A reader that knows the harness
    /// knows which arithmetic made the number.
    pub harness: harness_context::Harness,
    /// The `driver-diagnostic` driver word, present only where the shipped extension emits a typed
    /// turn result to classify. pi's does not: it has no error-classification field to forward, so
    /// this channel would have nothing but provider prose to key on and refuses to guess from it.
    pub diagnostic_driver: Option<driver_diagnostic::Driver>,
    pub runtime_id_env: &'static str,
    pub session_env: &'static str,
    pub seq_env: &'static str,
}

const PI_KIND: ChannelKind = ChannelKind {
    label: "pi",
    harness: harness_context::Harness::Pi,
    diagnostic_driver: None,
    runtime_id_env: crate::pi_session::CHANNEL_RUNTIME_ID,
    session_env: crate::pi_session::CHANNEL_SESSION,
    seq_env: crate::pi_session::CHANNEL_SEQ,
};

const OMP_KIND: ChannelKind = ChannelKind {
    label: "omp",
    harness: harness_context::Harness::Omp,
    diagnostic_driver: Some(driver_diagnostic::Driver::Omp),
    runtime_id_env: crate::omp_session::CHANNEL_RUNTIME_ID,
    session_env: crate::omp_session::CHANNEL_SESSION,
    seq_env: crate::omp_session::CHANNEL_SEQ,
};

/// Run the pi native message channel over stdio.
pub fn run(catalog_root: &Path, identity: &str) -> Result<()> {
    run_for(catalog_root, identity, &PI_KIND)
}

/// Run the omp native message channel over stdio (the omp extension's child).
pub fn run_omp(catalog_root: &Path, identity: &str) -> Result<()> {
    run_for(catalog_root, identity, &OMP_KIND)
}

fn run_for(catalog_root: &Path, identity: &str, kind: &ChannelKind) -> Result<()> {
    let agent_dir = message::resolve_declared_dir(catalog_root, identity, &crate::run::detect_host())?
        .with_context(|| format!("{} channel agent '{identity}' is not declared", kind.label))?;
    let inbox = message::inbox_dir(&agent_dir);
    // Composed here rather than in the extension: what a restarted agent is told is st2's contract,
    // not the asset's, and the Codex and Claude hooks compose the same three blocks in bash.
    let session_context = session_context(&agent_dir, identity);
    let (input_tx, input_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    write_json(
        &mut stdout,
        &json!({
            "type": "hello",
            "protocol": PROTOCOL,
            "identity": identity,
            "sessionContext": session_context,
        }),
    )?;
    stdout.flush()?;
    // The channel owns the live half of observed harness state: it is the one process that sees
    // the harness's own turn events, and its stdio connection to the extension is the evidence
    // that those events are still being watched. The terminal half belongs to the outer session
    // wrapper, which alone sees the provider die.
    // The pty session vouching for the record is the wrapper's task: its runtime ID arrives in
    // the channel environment, and only aliases the identity on driver-expanded seats.
    let pty_session = std::env::var(kind.runtime_id_env)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| identity.to_string());
    // The wrapper mints the session token; adopting it makes the wrapper's terminal record own
    // this channel's live records (so a queued frame after `ended` is suppressed) while a
    // predecessor incarnation's records are foreign: the first frame opens a fresh transition
    // and a predecessor's terminal record never silences this session.
    let wrapper_session = std::env::var(kind.session_env)
        .ok()
        .filter(|value| !value.is_empty());
    // The context record carries the same incarnation as the state record beside it, so a reader
    // can tell "this number came from the session currently running" from "this number predates
    // it". On this record the token is provenance only: nothing is fenced on it, a straggler's
    // write lands, and the next real reading overwrites it (HC-T04). Falling back to this
    // process's own token when the wrapper exported none keeps the field populated rather than
    // claiming a session it cannot name.
    let context_session = wrapper_session
        .clone()
        .unwrap_or_else(harness_state::session_token);
    let mut writer =
        harness_state::Writer::new(&agent_dir, identity, kind.label, Some(pty_session));
    if let Some(session) = wrapper_session {
        // Full adopted ownership when the wrapper exported it: the claimed sequence gives the
        // token a direction, so a straggler channel from a superseded session is refused.
        writer = match std::env::var(kind.seq_env)
            .ok()
            .and_then(|seq| seq.parse::<u64>().ok())
        {
            Some(seq) => writer.with_ownership(session, seq),
            None => writer.with_session(session),
        };
    }
    writer.interrupt();
    // The numeric axis is a sibling record with its own writer, deliberately sharing nothing with
    // the categorical one but the incarnation token: folding a token count into `Observation`
    // would make `sinceMs` reset on every turn whose numbers moved ("idle for 40 minutes" becomes
    // unrecoverable) and turn `transitions` into a turn counter.
    //
    // Failing to construct it must not cost the seat its mail. Delivery never depends on
    // observability anywhere else in this loop, and this is the one fallible construction here —
    // an agent directory with no parent has nowhere safe to stage a temporary file.
    let mut context_writer = match harness_context::Writer::new(&agent_dir, identity, kind.harness)
    {
        Ok(writer) => Some(writer.with_session(context_session)),
        Err(error) => {
            tracing::warn!(
                "st2 {} channel: harness context is unavailable: {error}",
                kind.label
            );
            None
        }
    };
    channel_loop(
        &input_rx,
        &mut stdout,
        &inbox,
        &agent_dir,
        &mut writer,
        context_writer.as_mut(),
        identity,
        kind,
        POLL,
        harness_state::HARNESS_STATE_REFRESH,
    )
}

/// The channel's steady state: forward inbox entries out, fold extension frames in, and keep the
/// observed-state heartbeat exactly as fresh as the stdio connection that justifies it. EOF is the
/// session-lifetime boundary — the loop then returns without writing anything, so the record ages
/// to `unknown` rather than asserting a state nobody is watching.
fn channel_loop(
    input: &Receiver<io::Result<String>>,
    out: &mut impl Write,
    inbox: &Path,
    agent_dir: &Path,
    writer: &mut harness_state::Writer,
    mut context_writer: Option<&mut harness_context::Writer>,
    identity: &str,
    kind: &ChannelKind,
    poll: Duration,
    heartbeat_every: Duration,
) -> Result<()> {
    let mut delivered = HashSet::new();
    let label = kind.label;
    let mut next_heartbeat = Instant::now() + heartbeat_every;
    loop {
        match input.recv_timeout(poll) {
            Ok(line) => {
                let line = line.with_context(|| format!("reading {label} channel input"))?;
                if line.trim().is_empty() {
                    continue;
                }
                // Frames from the extension are otherwise observational. An unknown *or malformed*
                // frame is dropped rather than fatal: a newer asset, or one line of stray output,
                // must not be able to take the channel down and stall an inbox. A failed record
                // write degrades the same way — delivery never depends on observability.
                let frame = serde_json::from_str::<Value>(&line).ok();
                // The typed turn result, decoded once: it feeds two independent records and the
                // credential edge must not depend on the categorical write landing.
                let turn = frame.as_ref().and_then(turn_result);
                if let Some(observation) = frame
                    .as_ref()
                    .and_then(state_observation)
                    .or_else(|| turn.as_ref().and_then(turn_observation))
                    // A queued live frame must never overwrite the wrapper's terminal record:
                    // the channel and the wrapper are separate processes, so the flock alone
                    // serializes but does not order their writes.
                    && let Err(error) = writer.observe_unless_ended(observation)
                {
                    tracing::warn!("st2 {label} channel: recording observed state failed: {error}");
                }
                // The credential axis is a third record, independent of the numbers and of the
                // categorical state: a rejection stands until a turn reaches its ordinary end,
                // whatever the seat's activity does in between.
                if let Some(driver) = kind.diagnostic_driver
                    && let Some(edge) = turn.as_ref().and_then(provider_auth_edge)
                {
                    publish_provider_auth(agent_dir, driver, edge);
                }
                // The numeric axis. There is deliberately no cadence here and no heartbeat timer:
                // a producer holding no fresh reading must write nothing at all, so the record
                // ages visibly through `ageMs` instead of looking refreshed. Every frame is handed
                // to the guard, which decides bucket, compaction edge, or heartbeat.
                if let Some(context) = frame.as_ref().and_then(context_frame)
                    && let Some(context_writer) = context_writer.as_deref_mut()
                    && let Err(error) = write_context(context_writer, context)
                {
                    tracing::warn!(
                        "st2 {label} channel: recording harness context failed: {error}"
                    );
                }
                if frame.as_ref().is_some_and(|frame| {
                    frame.get("type").and_then(Value::as_str) == Some("pre_compact")
                }) && let Err(error) = ensure_pre_compact_context(agent_dir)
                {
                    tracing::warn!(
                        "st2 {label} channel: writing pre-compact context stub failed: {error}"
                    );
                    let actionable = harness_state::Observation::new(
                        harness_state::Activity::Active,
                        harness_state::BlockedOn::None,
                        harness_state::InputBuffer::Unknown,
                    )
                    .with_reason(PRE_COMPACT_ERROR_REASON);
                    if let Err(state_error) = writer.observe_unless_ended(actionable) {
                        tracing::warn!(
                            "st2 {label} channel: recording pre-compact recovery failure failed: \
                             {state_error}"
                        );
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // pi's extension owns this child over stdio. EOF is the session-lifetime boundary, so
            // do not leave a detached watcher behind.
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        let now = Instant::now();
        if now >= next_heartbeat {
            if let Err(error) = writer.heartbeat() {
                tracing::warn!("st2 {label} channel: refreshing observed state failed: {error}");
            }
            next_heartbeat = now + heartbeat_every;
        }
        for msg in message::list_inbox(inbox)? {
            if delivered.insert(msg.filename.clone()) {
                write_json(out, &message_frame(msg, identity))?;
            }
        }
        out.flush()?;
        thread::sleep(poll);
    }
}

/// The observed-state frame the shipped extension emits on the harness's own turn boundaries.
/// Only positively recognized words become observations: an unrecognized state word is dropped
/// like any other unknown frame, so a newer asset cannot make this channel record something it
/// cannot vouch for. pi offers no waiting-on-a-human signal, so pi frames never carry
/// `blockedOn`; the omp extension does (`tool_approval_requested`/`_resolved`), and its optional
/// axes are parsed here for both channels — a frame without them decodes exactly as before.
fn state_observation(frame: &Value) -> Option<harness_state::Observation> {
    if frame.get("type").and_then(Value::as_str) != Some("state") {
        return None;
    }
    let state = match frame.get("state").and_then(Value::as_str)? {
        "active" => harness_state::Activity::Active,
        "idle" => harness_state::Activity::Idle,
        _ => return None,
    };
    let blocked_on = match frame.get("blockedOn").and_then(Value::as_str) {
        Some("human") => harness_state::BlockedOn::Human,
        _ => harness_state::BlockedOn::None,
    };
    let mut observation =
        harness_state::Observation::new(state, blocked_on, harness_state::InputBuffer::Unknown);
    if blocked_on == harness_state::BlockedOn::Human
        && let Some(ask) = frame.get("ask").and_then(Value::as_str)
    {
        observation = observation.with_ask(parse_ask(ask));
    }
    if let Some(reason) = frame.get("reason").and_then(Value::as_str) {
        observation = observation.with_reason(reason);
    }
    Some(observation)
}

/// The observed-state reason a rejected provider credential publishes, shared verbatim with the
/// OpenCode, Codex, and Claude producers: one word for one class, whatever named it.
const PROVIDER_AUTH_REASON: &str = "providerAuth";

/// omp's own error-classification bitfield, as `errorId` carries it on the assistant message whose
/// `stopReason` is `error` (omp's `qe` flags, measured on omp 18.1.7 — see
/// `docs/vrs/06-omp-driver/.experiments/2026-09-05-omp-provider-credential-rejection.md`).
///
/// Only the five flags this classifier needs are named. Reading the field at all is what keeps st2
/// out of provider prose: omp already did the classification, and its own credential-invalidating
/// rule is exactly the conjunction below.
mod omp_error {
    /// `qe.Class` — set by every classified value, and by nothing else. Without a flag the same
    /// field carries a BARE HTTP STATUS, so a bit test that skipped this would be reading digits.
    pub const CLASSIFIED: u64 = 1 << 12;
    /// `qe.AccountPolicy` — an org or content policy refusal; measured co-occurring with
    /// `AuthFailed` on a `cyber_policy` 403, which no re-login satisfies.
    pub const ACCOUNT_POLICY: u64 = 1 << 14;
    /// `qe.Transient` — omp intends to retry; measured co-occurring with `AuthFailed` on a
    /// `CONCURRENT_LIMIT` 403, the case omp's own rule excludes by prose.
    pub const TRANSIENT: u64 = 1 << 17;
    /// `qe.UsageLimit` — an exhausted allowance; measured co-occurring with `AuthFailed` on the
    /// `You have run out of credits` 403 that wedged a live seat for 120 transitions.
    pub const USAGE_LIMIT: u64 = 1 << 19;
    /// `qe.AuthFailed` — omp's name for a refused credential, set from a 401/403 status and from
    /// its own auth-error types.
    pub const AUTH_FAILED: u64 = 1 << 24;
}

/// One `type: "turn"` frame as the shipped omp extension emits it: the typed result of a turn that
/// ACTUALLY ended. A turn omp will retry (`willContinue`) sends no frame at all, so neither
/// credential edge is ever claimed mid-turn.
enum TurnResult<'a> {
    /// The turn reached its ordinary end — positive proof the provider accepted the credential.
    /// It asserts no activity: the extension's sampled idle poll still owns that edge.
    Ordinary,
    /// The turn ended on a provider error, carrying omp's own words for it.
    ProviderError {
        reason: Option<&'a str>,
        classification: Option<u64>,
    },
}

fn turn_result(frame: &Value) -> Option<TurnResult<'_>> {
    if frame.get("type").and_then(Value::as_str) != Some("turn") {
        return None;
    }
    let Some(error) = frame.get("error") else {
        return Some(TurnResult::Ordinary);
    };
    Some(TurnResult::ProviderError {
        reason: error.get("reason").and_then(Value::as_str),
        classification: error.get("errorId").and_then(Value::as_u64),
    })
}

/// Whether omp's classification of the error that ended a turn names a REJECTED CREDENTIAL.
///
/// `AuthFailed` alone is not the answer, because omp sets it from prose that says `401`, `403`, or
/// `forbidden` as well as from a typed status — and three of the four measured 403s were capacity,
/// policy, or concurrency. The three negative flags are each a measured co-occurrence, not a
/// precaution, and together they are omp's own rule for reaching into the credential store.
/// A classification this reader cannot see at all is not a rejection: silence beats a guess.
fn provider_credential_rejected(classification: Option<u64>) -> bool {
    let Some(id) = classification else {
        return false;
    };
    id & omp_error::CLASSIFIED != 0
        && id & omp_error::AUTH_FAILED != 0
        && id & (omp_error::USAGE_LIMIT | omp_error::ACCOUNT_POLICY | omp_error::TRANSIENT) == 0
}

/// The categorical half of a typed turn result.
///
/// A provider error that ended the turn is `active`, not an idle settle: nothing is running, but
/// the seat needs an operator and a record saying `idle` would read as a healthy yield. The reason
/// is the closed `providerAuth` word for the credential class — the same word Claude, Codex, and
/// OpenCode publish — and omp's own bounded prose for every other class, which is the only place
/// a reader learns that a 403 was about credits.
fn turn_observation(result: &TurnResult<'_>) -> Option<harness_state::Observation> {
    let TurnResult::ProviderError {
        reason,
        classification,
    } = result
    else {
        return None;
    };
    let observation = harness_state::Observation::new(
        harness_state::Activity::Active,
        harness_state::BlockedOn::None,
        harness_state::InputBuffer::Unknown,
    );
    Some(if provider_credential_rejected(*classification) {
        observation.with_reason(PROVIDER_AUTH_REASON)
    } else {
        match *reason {
            Some(reason) => observation.with_reason(reason),
            None => observation,
        }
    })
}

/// What one typed turn result proves about the seat's provider credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderAuthEdge {
    Rejected,
    Accepted,
}

/// The credential edge, or `None` when the turn proves nothing about it — which must leave a
/// standing rejection alone rather than clearing it. A provider error of any other class is
/// exactly that case: a rate limit says nothing about whether the credential is still good.
fn provider_auth_edge(result: &TurnResult<'_>) -> Option<ProviderAuthEdge> {
    match result {
        TurnResult::Ordinary => Some(ProviderAuthEdge::Accepted),
        TurnResult::ProviderError { classification, .. } => {
            provider_credential_rejected(*classification).then_some(ProviderAuthEdge::Rejected)
        }
    }
}

/// Record one credential edge on the seat's native-driver diagnostic.
///
/// A fresh publisher per edge, like the Claude hook's: the on-disk fallback is what lets an
/// ordinary turn end clear a rejection, and a channel that restarted mid-session inherits the
/// predecessor's record rather than silently starting clean. Fail-open like every other
/// observation in this loop — the publisher only warns on a write it cannot land, and delivery
/// never depends on it.
fn publish_provider_auth(
    agent_dir: &Path,
    driver: driver_diagnostic::Driver,
    edge: ProviderAuthEdge,
) {
    let mut publisher = driver_diagnostic::Publisher::new(
        agent_dir,
        driver,
        // The wrapper — not the channel — owns the version gate, and it refuses the launch on an
        // unadmitted MINOR (OMP-R05), so a running channel has no version fact of its own to
        // publish and no support verdict to restate.
        None,
        driver_diagnostic::Support::Unknown,
    );
    match edge {
        ProviderAuthEdge::Rejected => publisher.publish(
            driver_diagnostic::Stage::ProviderAuth,
            driver_diagnostic::Reason::ProviderAuthRejected,
            driver_diagnostic::Source::TurnResult,
        ),
        ProviderAuthEdge::Accepted => publisher.clear(driver_diagnostic::Stage::ProviderAuth),
    }
}

/// Write the recovery stub only when durable working state is absent or whitespace-only.
///
/// The extension cannot perform this check: it owns neither the resolved agent directory nor the
/// context module's shared writer lock. The context API keeps predicate and replacement in one
/// critical section and preserves every read error except `NotFound`.
fn ensure_pre_compact_context(agent_dir: &Path) -> Result<bool> {
    context::write_now_if_blank(&context::context_dir(agent_dir), PRE_COMPACT_STUB)
}

/// One `type: "context"` frame as the shipped extension emits it: a reading, a compaction edge, or
/// both. Both halves are optional, and a frame carrying neither is not a frame — it is dropped
/// like any other unrecognized one.
type ContextFrame = (
    Option<harness_context::Reading>,
    Option<harness_context::Compaction>,
);

/// Decode a context frame (HC-R02, HC-R03, HC-R12).
///
/// The withholding discipline lives here as much as in the asset: a number this decoder cannot
/// read as a finite number is `None`, never zero and never the previous value. `usedPercent` is
/// taken exactly as the harness published it — pi and omp report a float that runs well above 100
/// on an overrun (585.6% measured), and st2 neither clamps it nor computes one of its own from a
/// window it would have had to guess at.
fn context_frame(frame: &Value) -> Option<ContextFrame> {
    if frame.get("type").and_then(Value::as_str) != Some("context") {
        return None;
    }
    let reading = frame.get("reading").and_then(Value::as_object).map(|body| {
        let tokens = |key: &str| body.get(key).and_then(token_count);
        harness_context::Reading {
            used_tokens: tokens("usedTokens"),
            window_tokens: tokens("windowTokens"),
            used_percent: body.get("usedPercent").and_then(Value::as_f64),
            model: body
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            cost_usd: body.get("costUsd").and_then(Value::as_f64),
            // pi and omp both withhold this in v1: obtaining it means summing every message's
            // usage, which is a producer-side accumulator whose correctness depends on having seen
            // every message. tokenlens owns lifetime accounting, and a half-observed sum would be
            // a worse answer than none.
            session_total_tokens: None,
            // Neither harness reports account-scoped rate limits.
            rate_limits: harness_context::RateLimits::default(),
        }
    });
    let compaction = frame
        .get("compaction")
        .and_then(Value::as_object)
        .map(|body| {
            let edge = harness_context::Compaction::new(compaction_trigger(
                body.get("trigger").and_then(Value::as_str),
            ));
            // The harness's own session store answers the count for both of these harnesses, so
            // the counter is durable across restarts. A frame that could not read it leaves the
            // count absent, and st2 falls back to incrementing its own — a weaker,
            // incarnation-scoped answer rather than a wrong one.
            match body.get("count").and_then(Value::as_u64) {
                Some(count) => edge.with_count(count),
                None => edge,
            }
        });
    (reading.is_some() || compaction.is_some()).then_some((reading, compaction))
}

/// A token count off the wire, tolerating a fractional one.
///
/// Both harnesses round today — pi's fallback estimator is `Math.ceil(chars / 4)` and every
/// measured reading was integral — so this is deliberately not a decode of anything observed. It
/// exists because the failure if one ever stops rounding is silent in the worst direction: a plain
/// integer parse would return `None` for `1234.75`, and the producer would WITHHOLD a reading the
/// harness actually had. Withholding is reserved for a harness saying it does not know (HC-R03);
/// spending it on a JSON number shape would make the record lie about which of those happened. The
/// percent leg already parses as a float because pi and omp genuinely emit one there; this extends
/// the same tolerance to the operands rather than leaving them stricter for no measured reason.
///
/// A negative or non-finite value is not a token count and is withheld.
fn token_count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number.round() as u64)
    })
}

/// The trigger word, over the record's closed vocabulary and additive-tolerant on read: a word
/// this version does not recognize — and an edge that carries none at all, which is omp's case and
/// three of the five harnesses' — decodes as `unknown`, never as a definite trigger.
fn compaction_trigger(word: Option<&str>) -> harness_context::CompactionTrigger {
    use harness_context::CompactionTrigger as Trigger;
    match word {
        Some("manual") => Trigger::Manual,
        Some("auto") => Trigger::Auto,
        Some("threshold") => Trigger::Threshold,
        Some("overflow") => Trigger::Overflow,
        Some("idle") => Trigger::Idle,
        _ => Trigger::Unknown,
    }
}

/// Land one context frame.
///
/// A frame carrying both halves lands as ONE write, and that is load-bearing rather than an
/// optimization: a compaction edge always writes while a reading whose percent is withheld has no
/// bucket, so an edge written alone would publish the stale pre-compaction numbers beside it and
/// the null reading proving the window was emptied would not appear until the heartbeat came due.
/// pi hands us exactly that pair — measured inside its own `session_compact` handler,
/// `getContextUsage()` already reports `{tokens: null, percent: null}` there.
fn write_context(
    writer: &mut harness_context::Writer,
    (reading, compaction): ContextFrame,
) -> Result<bool> {
    match (reading, compaction) {
        (Some(reading), Some(compaction)) => writer.compacted_with(compaction, reading),
        (Some(reading), None) => writer.observe(reading),
        (None, Some(compaction)) => writer.compacted(compaction),
        (None, None) => Ok(false),
    }
}

/// The machine-readable ask word on a blocked frame. An unrecognized word decodes as unknown —
/// indeterminate, never silently reclassified — matching the record's own decode rule.
fn parse_ask(word: &str) -> harness_state::Ask {
    match word {
        "permission" => harness_state::Ask::Permission,
        "question" => harness_state::Ask::Question,
        "review" => harness_state::Ask::Review,
        _ => harness_state::Ask::Unknown,
    }
}

/// What a starting or restarting pi session is told about its own durable state.
///
/// pi has no session-start hook, so this is the payload that stands in for
/// `$ST_HOOKS/codex-session-start.sh`. The three blocks and their order are deliberately identical
/// to that script's, so a persona written against one harness reads the same on pi. Empty when
/// there is nothing to restore and no unread work, so a fresh agent is told nothing but its ritual.
fn session_context(agent_dir: &Path, identity: &str) -> String {
    let mut blocks = Vec::new();
    let state = context::read_now_fresh(&context::context_dir(agent_dir), CONTEXT_MAX_AGE);
    if !state.trim().is_empty() {
        blocks.push(format!(
            "<context source=\"st2/context/now.md\" agent=\"{identity}\">\n{}\n</context>",
            state.trim_end()
        ));
    }
    blocks.push(RITUAL.to_string());
    let unread = message::list_inbox(&message::inbox_dir(agent_dir)).unwrap_or_default();
    if !unread.is_empty() {
        let mut lines = vec![format!("## st2 inbox ({} unread)", unread.len())];
        lines.extend(unread.iter().map(|msg| {
            let from = msg.from.as_deref().unwrap_or("unknown");
            match msg.subject.as_deref() {
                Some(subject) => format!("- {}  {from}  Subject: {subject}", msg.filename),
                None => format!("- {}  {from}", msg.filename),
            }
        }));
        blocks.push(lines.join("\n"));
    }
    blocks.join("\n\n")
}

/// One inbox entry as the frame the extension hands to pi.
///
/// The envelope matches the Claude channel's exactly, so a persona written against one native
/// harness reads the same on the other.
fn message_frame(msg: message::Message, identity: &str) -> Value {
    let content = channel_content(msg.subject.as_deref(), &msg.body);
    json!({"type":"message","deliverAs":DELIVER_AS,"content":content,"meta":{
        "from": msg.from,
        "messageFilename": msg.filename,
        "threadFilename": msg.in_reply_to.unwrap_or_else(|| msg.filename.clone()),
        "identity": identity
    }})
}

fn write_json(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the two words pi's own turn boundaries can vouch for become observations. Everything
    /// else — other frame types, unknown state words, missing fields — is dropped, so a newer
    /// extension asset cannot push this channel into recording something it cannot prove.
    #[test]
    fn only_recognized_state_frames_become_observations() {
        let active = state_observation(&json!({"type":"state","state":"active"})).unwrap();
        assert_eq!(active.state, harness_state::Activity::Active);
        assert_eq!(active.blocked_on, harness_state::BlockedOn::None);
        assert_eq!(active.input_buffer, harness_state::InputBuffer::Unknown);
        assert_eq!(
            state_observation(&json!({"type":"state","state":"idle"}))
                .unwrap()
                .state,
            harness_state::Activity::Idle
        );

        for frame in [
            json!({"type":"state","state":"child"}),
            json!({"type":"state","state":"unknown"}),
            json!({"type":"state"}),
            json!({"type":"delivered","state":"active"}),
            json!({"state":"active"}),
        ] {
            assert_eq!(state_observation(&frame), None, "frame: {frame}");
        }
    }

    /// The omp extension's approval frames carry the blocked-on-human axis pi never emits. The
    /// optional axes must decode for either channel's frames, an unrecognized ask word decodes
    /// unknown (never silently reclassified), and a blocked frame without the extras decodes as
    /// before.
    #[test]
    fn blocked_frames_carry_the_human_axes() {
        let blocked = state_observation(&json!({
            "type":"state","state":"active","blockedOn":"human",
            "ask":"permission","reason":"bash"
        }))
        .unwrap();
        assert_eq!(blocked.blocked_on, harness_state::BlockedOn::Human);
        assert_eq!(blocked.ask, harness_state::Ask::Permission);
        assert_eq!(blocked.reason.as_deref(), Some("bash"));

        let question = state_observation(&json!({
            "type":"state","state":"active","blockedOn":"human",
            "ask":"question","reason":"Which deployment target?"
        }))
        .unwrap();
        assert_eq!(question.blocked_on, harness_state::BlockedOn::Human);
        assert_eq!(question.ask, harness_state::Ask::Question);
        assert_eq!(
            question.reason.as_deref(),
            Some("Which deployment target?")
        );

        let unknown_ask = state_observation(&json!({
            "type":"state","state":"active","blockedOn":"human",
            "ask":"sacrifice"
        }))
        .unwrap();
        assert_eq!(unknown_ask.blocked_on, harness_state::BlockedOn::Human);
        assert_eq!(unknown_ask.ask, harness_state::Ask::Unknown);

        let plain = state_observation(&json!({"type":"state","state":"idle"})).unwrap();
        assert_eq!(plain.blocked_on, harness_state::BlockedOn::None);
        assert_eq!(plain.ask, harness_state::Ask::None);
    }

    /// A pre-compaction edge creates a last-resort checkpoint only for whitespace-only state. The
    /// channel, not the TypeScript extension, resolves the durable path and performs the write.
    #[test]
    fn pre_compact_frame_writes_only_over_blank_context() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        let inbox = message::inbox_dir(agent_dir);
        std::fs::create_dir_all(&inbox).unwrap();
        let context_dir = context::context_dir(agent_dir);
        context::write_now(&context_dir, " \n\t").unwrap();

        let run_frame = || {
            let mut writer =
                harness_state::Writer::new(agent_dir, "h.worker", "omp", Some("h.worker".into()));
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(r#"{"type":"pre_compact"}"#.to_string()))
                .unwrap();
            drop(tx);
            channel_loop(
                &rx,
                &mut Vec::new(),
                &inbox,
                agent_dir,
                &mut writer,
                None,
                "h.worker",
                &OMP_KIND,
                Duration::from_millis(1),
                Duration::from_secs(60),
            )
            .unwrap();
        };

        run_frame();
        assert_eq!(
            context::read(&context_dir, context::View::Now),
            PRE_COMPACT_STUB
        );

        let authored = "Investigating scheduler race; next run the focused repro.\n";
        context::write_now(&context_dir, authored).unwrap();
        run_frame();
        assert_eq!(
            context::read(&context_dir, context::View::Now),
            authored,
            "the recovery edge must never replace authored state"
        );

        std::fs::remove_file(context_dir.join("now.md")).unwrap();
        std::fs::write(context_dir.join("now.md"), [0xff]).unwrap();
        run_frame();
        assert_eq!(
            std::fs::read(context_dir.join("now.md")).unwrap(),
            [0xff],
            "undecodable state must not be replaced"
        );
        let raw: Value = serde_json::from_slice(
            &std::fs::read(harness_state::harness_state_path(agent_dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(raw["state"], "active");
        assert_eq!(raw["reason"], PRE_COMPACT_ERROR_REASON);
    }

    /// The stdio connection is the evidence. While it lives, the record's heartbeat advances
    /// without a new observation; when it ends, the loop returns having written nothing more, so
    /// the last state is left to age to `unknown` instead of being asserted or terminated — the
    /// terminal record belongs to the session wrapper, which sees the provider die.
    #[test]
    fn heartbeats_while_connected_then_leaves_the_record_to_age_on_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        std::fs::create_dir_all(message::inbox_dir(agent_dir)).unwrap();
        let record = harness_state::harness_state_path(agent_dir);
        let mut writer =
            harness_state::Writer::new(agent_dir, "h.worker", "pi", Some("h.worker".into()));
        let (tx, rx) = mpsc::channel();

        tx.send(Ok(r#"{"type":"state","state":"active"}"#.to_string()))
            .unwrap();
        let disconnect = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            drop(tx);
        });
        let mut out = Vec::new();
        channel_loop(
            &rx,
            &mut out,
            &message::inbox_dir(agent_dir),
            agent_dir,
            &mut writer,
            None,
            "h.worker",
            &PI_KIND,
            Duration::from_millis(2),
            Duration::from_millis(5),
        )
        .unwrap();

        disconnect.join().unwrap();

        let raw: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(raw["state"], "active", "EOF must not rewrite the state");
        assert!(
            raw["writtenAtMs"].as_u64().unwrap() > raw["sinceMs"].as_u64().unwrap(),
            "a heartbeat re-stamped the record while the connection lived: {raw}"
        );
        let after_eof = std::fs::read(&record).unwrap();
        thread::sleep(Duration::from_millis(15));
        assert_eq!(
            std::fs::read(&record).unwrap(),
            after_eof,
            "nothing may write after the connection is gone"
        );
    }

    /// The wrapper's terminal record is the incarnation's last word: a live frame the extension
    /// queued before dying must not resurrect the session after the wrapper reaped it.
    #[test]
    fn a_queued_live_frame_never_overwrites_the_wrappers_terminal_record() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        std::fs::create_dir_all(message::inbox_dir(agent_dir)).unwrap();
        let record = harness_state::harness_state_path(agent_dir);
        // The wrapper mints the session token and the channel adopts it — that sharing is what
        // makes the wrapper's terminal record this session's last word.
        let session = harness_state::session_token();
        let mut channel_writer =
            harness_state::Writer::new(agent_dir, "h.worker", "pi", Some("h.worker".into()))
                .with_session(session.clone());
        let mut wrapper_writer =
            harness_state::Writer::new(agent_dir, "h.worker", "pi", Some("h.worker".into()))
                .with_session(session);
        wrapper_writer.ended("signal 9").unwrap();
        let terminal = std::fs::read(&record).unwrap();

        let (tx, rx) = mpsc::channel();
        tx.send(Ok(r#"{"type":"state","state":"idle"}"#.to_string()))
            .unwrap();
        drop(tx);
        let mut out = Vec::new();
        channel_loop(
            &rx,
            &mut out,
            &message::inbox_dir(agent_dir),
            agent_dir,
            &mut channel_writer,
            None,
            "h.worker",
            &PI_KIND,
            Duration::from_millis(2),
            Duration::from_millis(5),
        )
        .unwrap();

        assert_eq!(std::fs::read(&record).unwrap(), terminal);
    }

    /// Land one context frame through a real writer and read the record back.
    fn record_after(
        frames: &[Value],
        harness: harness_context::Harness,
    ) -> harness_context::Observed {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agents").join("h").join("h.worker");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let mut writer = harness_context::Writer::new(&agent_dir, "h.worker", harness).unwrap();
        for frame in frames {
            if let Some(context) = context_frame(frame) {
                write_context(&mut writer, context).unwrap();
            }
        }
        harness_context::read(&harness_context::harness_context_path(&agent_dir))
            .expect("a record must have been written")
    }

    /// HC-R13, pinned to pi 0.84.2. The payload is verbatim from the credential-free pi lab: one
    /// `message_end` for an assistant message, with `getContextUsage()` and the message's own
    /// `usage` side by side.
    ///
    /// The teeth are the numerator's MEANING, not merely its value. pi's `tokens` is the last
    /// assistant message's `totalTokens` — input + output + cacheRead + cacheWrite — and the
    /// fixture carries `input` too, so a producer that started publishing the prompt figure (which
    /// is what omp's identically-shaped call returns) fails here rather than silently publishing a
    /// differently-meaning number under the same field name. The percent is carried raw at 585.625:
    /// pi reports a float that runs far above 100 when a turn overruns the window, and a producer
    /// or reader that clamped it would hide exactly the saturation this record exists to show.
    #[test]
    fn the_pi_0_84_2_fixture_pins_total_tokens_as_the_numerator() {
        const MEASURED: &str = crate::pi_session::MEASURED_CONTEXT_VERSION;
        assert_eq!(
            MEASURED, "0.84.2",
            "the fixture below was captured on this build"
        );
        // Verbatim `event.message.usage` from the same event, carried so the assertion below can
        // name the number this producer must NOT publish.
        let message_usage = json!({
            "input": 23300, "output": 25, "cacheRead": 100, "cacheWrite": 0, "reasoning": 0,
            "totalTokens": 23425,
            "cost": {"input": 0.0699, "output": 0.000375, "cacheRead": 0.00003,
                     "cacheWrite": 0.0, "total": 0.070305}
        });
        // Verbatim `ctx.getContextUsage()` on that event, as the extension forwards it.
        let frame = json!({"type": "context", "reading": {
            "usedTokens": 23425, "windowTokens": 4000, "usedPercent": 585.625,
            "model": "fake-1", "costUsd": 0.070305
        }});

        let record = record_after(&[frame], harness_context::Harness::Pi);

        assert_eq!(record.harness, harness_context::Harness::Pi);
        assert_eq!(
            record.used_tokens,
            message_usage["totalTokens"].as_u64(),
            "pi {MEASURED}: the numerator is the assistant message's totalTokens"
        );
        assert_ne!(
            record.used_tokens,
            message_usage["input"].as_u64(),
            "pi {MEASURED}: publishing the prompt figure would be omp's arithmetic under pi's tag"
        );
        assert_eq!(record.window_tokens, Some(4000));
        assert_eq!(
            record.used_percent,
            Some(585.625),
            "carried raw, never clamped"
        );
        assert_eq!(record.model.as_deref(), Some("fake-1"));
        assert_eq!(record.cost_usd, message_usage["cost"]["total"].as_f64());
        // Neither pi nor omp carries these in v1: a lifetime sum would need a producer-side
        // accumulator whose correctness depends on having seen every message, and neither reports
        // account-scoped rate limits at all.
        assert_eq!(record.session_total_tokens, None);
        assert_eq!(record.rate_limits, harness_context::RateLimits::default());
    }

    /// HC-R13 and HC-T03's second version-coupled constant, pinned to omp 18.0.9 (the same probe
    /// run reproduces it on 18.0.3, which is why the launch gate can admit the whole 18.0 minor).
    ///
    /// omp's call has pi's exact shape and a DIFFERENT meaning: `tokens` settles to the last
    /// assistant message's prompt figure — `input` plus what pi's decomposition calls `cacheRead`
    /// — never to `totalTokens`. Measured in a controlled lab whose fake provider reported prompt
    /// tokens of 900, 9,900 and 22,500, and again on a real-credential run where `tokens` read
    /// 2,065 against that message's `totalTokens` of 2,071. So omp under-reports relative to pi by
    /// output plus cache write, and this test fails if `tokens` ever stops meaning prompt-only.
    #[test]
    fn the_omp_18_0_9_fixture_pins_prompt_input_as_the_numerator() {
        const MEASURED: [&str; 2] = crate::omp_session::MEASURED_CONTEXT_VERSIONS;
        assert_eq!(
            MEASURED,
            ["18.0.9", "18.0.3"],
            "the fixture below was captured on these builds"
        );
        let message_usage = json!({
            "input": 22400, "cacheRead": 100, "cacheWrite": 0, "output": 25,
            "totalTokens": 22525,
            "cost": {"total": 0.067605}
        });
        let frame = json!({"type": "context", "reading": {
            "usedTokens": 22500, "windowTokens": 4000, "usedPercent": 562.5,
            "model": "fake-1", "costUsd": 0.067605
        }});

        let record = record_after(&[frame], harness_context::Harness::Omp);

        assert_eq!(record.harness, harness_context::Harness::Omp);
        let prompt =
            message_usage["input"].as_u64().unwrap() + message_usage["cacheRead"].as_u64().unwrap();
        assert_eq!(
            record.used_tokens,
            Some(prompt),
            "omp {MEASURED:?}: the numerator is the assistant message's prompt tokens"
        );
        assert_ne!(
            record.used_tokens,
            message_usage["totalTokens"].as_u64(),
            "omp {MEASURED:?}: publishing totalTokens would be pi's arithmetic under omp's tag, \
             over-reporting the window by output plus cache write"
        );
        assert_eq!(record.window_tokens, Some(4000));
        assert_eq!(
            record.used_percent,
            Some(562.5),
            "carried raw, never clamped"
        );
        assert_eq!(record.cost_usd, Some(0.067605));
        assert_eq!(record.session_total_tokens, None);
    }

    /// pi's honest unknown, end to end (HC-R03). Measured inside pi's own `session_compact`
    /// handler: `getContextUsage()` already reports `{tokens: null, percent: null}` there while
    /// `contextWindow` stays populated, and `getEntries()` already counts the new entry.
    ///
    /// Two things must hold and both are silent failures. The nulls must REPLACE the previous
    /// reading rather than being carried forward — an agent whose window was just emptied must not
    /// still read 90% full. And they must land in the SAME write as the edge: a compaction edge
    /// always writes while a withheld percent has no bucket, so an edge written on its own would
    /// publish the stale pre-compaction numbers beside it and the truth would wait for the
    /// heartbeat.
    #[test]
    fn a_pi_compaction_withholds_the_reading_it_emptied_in_the_same_write() {
        let before = json!({"type": "context", "reading": {
            "usedTokens": 3625, "windowTokens": 4000, "usedPercent": 90.625,
            "model": "fake-1", "costUsd": 0.010905
        }});
        // Verbatim: reason "overflow", and `getEntries()` filtered to compactions already reads 3.
        let compacted = json!({"type": "context",
            "reading": {"usedTokens": null, "windowTokens": 4000, "usedPercent": null,
                        "model": "fake-1", "costUsd": 0.010905},
            "compaction": {"trigger": "overflow", "count": 3}});

        let full = record_after(&[before.clone()], harness_context::Harness::Pi);
        assert_eq!(full.used_percent, Some(90.625));
        assert_eq!(full.compactions, 0);

        let record = record_after(&[before, compacted], harness_context::Harness::Pi);

        assert_eq!(
            record.used_tokens, None,
            "a withheld count is never the previous one"
        );
        assert_eq!(record.used_percent, None, "nor is a withheld percent");
        assert_eq!(
            record.window_tokens,
            Some(4000),
            "pi still knows its denominator"
        );
        assert_eq!(
            record.last_compaction_trigger,
            Some(harness_context::CompactionTrigger::Overflow),
            "pi is the only v1 producer that names its trigger"
        );
        assert_eq!(
            record.compactions, 3,
            "the count is the harness's own durable one, not st2 counting edges"
        );
        assert!(record.last_compaction_ms.is_some());
        // Withholding occupancy says nothing about cost, which the harness did not retract.
        assert_eq!(record.cost_usd, Some(0.010905));
    }

    /// omp's compaction edge carries no `reason` and no `willRetry` — pi 0.84.2 has both — so the
    /// trigger is `unknown`, a legitimate v1 value for three of the five harnesses. omp does name
    /// its auto-compaction "idle" and "threshold" internally, but those words are not projected
    /// onto the event and inventing one would be a claim no capture supports. Unlike pi, omp's
    /// `getContextUsage()` still answers inside the handler, so a real reading rides along.
    #[test]
    fn an_omp_compaction_yields_unknown_because_the_event_names_no_reason() {
        let compacted = json!({"type": "context",
            "reading": {"usedTokens": 8100, "windowTokens": 4000, "usedPercent": 202.5,
                        "model": "fake-1", "costUsd": null},
            "compaction": {"trigger": null, "count": 1}});

        let record = record_after(&[compacted], harness_context::Harness::Omp);

        assert_eq!(
            record.last_compaction_trigger,
            Some(harness_context::CompactionTrigger::Unknown)
        );
        assert_eq!(record.compactions, 1);
        assert_eq!(record.used_tokens, Some(8100));
        assert_eq!(record.cost_usd, None);
    }

    /// A count the extension could not read leaves st2 counting edges itself. That is a weaker
    /// answer — incarnation-scoped rather than harness-durable — and the point is that it is
    /// weaker rather than wrong: the edge still lands, with a trigger.
    #[test]
    fn an_unreadable_durable_count_degrades_to_counting_edges_not_to_losing_them() {
        let edge = json!({"type": "context", "compaction": {"trigger": "manual"}});

        let record = record_after(&[edge.clone(), edge], harness_context::Harness::Pi);

        assert_eq!(record.compactions, 2);
        assert_eq!(
            record.last_compaction_trigger,
            Some(harness_context::CompactionTrigger::Manual)
        );
    }

    /// The decoder's own fail-closed rules. A trigger word this version does not know decodes as
    /// `unknown` and never as a definite one; a frame with neither half is not a frame; and every
    /// other frame type is left to the other decoders.
    #[test]
    fn context_frames_decode_conservatively_or_not_at_all() {
        assert_eq!(
            compaction_trigger(Some("sacrifice")),
            harness_context::CompactionTrigger::Unknown
        );
        assert_eq!(
            compaction_trigger(None),
            harness_context::CompactionTrigger::Unknown
        );
        for word in ["manual", "auto", "threshold", "overflow", "idle"] {
            assert_eq!(compaction_trigger(Some(word)).as_str(), word);
        }

        for frame in [
            json!({"type": "context"}),
            json!({"type": "context", "reading": "nonsense"}),
            json!({"type": "state", "state": "idle"}),
            json!({"reading": {"usedTokens": 1}}),
        ] {
            assert_eq!(context_frame(&frame), None, "frame: {frame}");
        }

        // A reading whose every leg is withheld is still a reading: "the harness told us it does
        // not know" is an observation, and dropping it would leave a stale number looking current.
        let withheld = context_frame(&json!({"type": "context", "reading": {}})).unwrap();
        assert_eq!(withheld.0, Some(harness_context::Reading::default()));
        assert_eq!(withheld.1, None);
    }

    /// Withholding must mean "the harness said it does not know", never "the number arrived in a
    /// JSON shape this decoder was strict about". Both harnesses round today — pi's fallback
    /// estimator is `Math.ceil(chars / 4)` — so a fractional count is not something measured; the
    /// point is that if one ever stops rounding, a strict integer parse would silently discard a
    /// real reading and the record would be indistinguishable from an honest withheld one.
    #[test]
    fn a_fractional_token_count_is_a_reading_not_a_withheld_value() {
        assert_eq!(token_count(&json!(23425)), Some(23425));
        assert_eq!(token_count(&json!(1234.75)), Some(1235));
        assert_eq!(token_count(&json!(0)), Some(0));
        // Not counts: a negative, a non-finite, and a non-number are withheld.
        assert_eq!(token_count(&json!(-1)), None);
        assert_eq!(token_count(&json!("23425")), None);
        assert_eq!(token_count(&Value::Null), None);

        let (reading, _) = context_frame(&json!({"type": "context", "reading": {
            "usedTokens": 1234.75, "windowTokens": 4000, "usedPercent": 30.86
        }}))
        .unwrap();
        assert_eq!(reading.unwrap().used_tokens, Some(1235));
    }

    /// A context frame must never be able to take the channel down or stall the inbox, and the two
    /// axes must stay independent: the numeric record is not consulted by the categorical one and
    /// does not consult it.
    #[test]
    fn context_frames_and_state_frames_are_independent_axes_on_one_wire() {
        let context = json!({"type": "context", "reading": {"usedTokens": 10, "usedPercent": 1.0}});
        let state = json!({"type": "state", "state": "idle"});

        assert!(
            state_observation(&context).is_none(),
            "a context frame is not an observation"
        );
        assert!(
            context_frame(&state).is_none(),
            "a state frame is not a reading"
        );
    }

    /// HC-R13's version pin for pi. pi ships no runtime gate, so the only thing coupling this
    /// repository to a pi build is the flake check that type-checks and runtime-smokes the shipped
    /// asset. If that tarball moves without the fixture moving, the fixture would keep claiming a
    /// measurement of a build nothing in the tree uses any more — the exact silent drift HC-T03
    /// asks a fixture to bound.
    #[test]
    fn the_measured_pi_release_is_the_one_the_extension_gate_pins() {
        let flake = include_str!("../flake.nix");
        let pin = format!(
            "piVersion = \"{}\";",
            crate::pi_session::MEASURED_CONTEXT_VERSION
        );
        assert!(
            flake.contains(&pin),
            "flake.nix must pin the pi release the harness-context fixture measured ({pin})"
        );
    }

    #[test]
    fn channel_content_reuses_the_claude_channel_envelope() {
        assert_eq!(
            channel_content(Some("subject"), "body"),
            "Subject: subject\n\nbody"
        );
        assert_eq!(channel_content(None, "body"), "body");
        assert_eq!(channel_content(Some(""), "body"), "body");
    }

    /// A restarting pi agent has to be told the same three things the Codex and Claude session-start
    /// hooks tell theirs, in the same order — otherwise "restart" means something different per
    /// harness.
    #[test]
    fn session_context_restores_state_ritual_and_unread_work_in_hook_order() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        context::write_now(
            &context::context_dir(agent_dir),
            "Mid-migration on shard 3.",
        )
        .unwrap();
        let inbox = message::inbox_dir(agent_dir);
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("1787042542238-xex2t4.md"),
            "---\nfrom: h.supervisor\nsubject: deploy check\n---\nVerify staging.\n",
        )
        .unwrap();

        let restored = session_context(agent_dir, "h.worker");

        let state = restored
            .find("Mid-migration on shard 3.")
            .expect("state restored");
        let ritual = restored
            .find("Run the st2 boot ritual")
            .expect("ritual present");
        let unread = restored
            .find("## st2 inbox (1 unread)")
            .expect("unread listed");
        assert!(state < ritual && ritual < unread, "{restored}");
        assert!(
            restored.contains("<context source=\"st2/context/now.md\" agent=\"h.worker\">"),
            "{restored}"
        );
        assert!(restored.contains("Subject: deploy check"), "{restored}");
    }

    /// A fresh agent has no durable state and no mail. It must still get its ritual, and must not be
    /// handed an empty `<context>` envelope describing nothing.
    #[test]
    fn a_fresh_agent_is_told_only_its_ritual() {
        let tmp = tempfile::tempdir().unwrap();

        let restored = session_context(tmp.path(), "h.worker");

        assert!(
            restored.starts_with("Run the st2 boot ritual"),
            "{restored}"
        );
        assert!(!restored.contains("<context"), "{restored}");
        assert!(!restored.contains("st2 inbox"), "{restored}");
    }

    /// The extension is not allowed to choose how a message lands, so the mode has to be on the
    /// frame st2 writes. Pinning it here is what makes #277 a one-place change.
    #[test]
    fn st2_owns_the_delivery_mode_on_the_wire() {
        let frame = message_frame(
            message::Message {
                filename: "1787042542238-xex2t4.md".into(),
                ts_ms: 1_787_042_542_238,
                from: Some("h.supervisor".into()),
                subject: Some("deploy check".into()),
                in_reply_to: None,
                tags: Vec::new(),
                priority: None,
                idempotency_key: None,
                stream: None,
                event_id: None,
                event_key: None,
                body: "Please verify the staging deploy.".into(),
            },
            "h.worker",
        );

        assert_eq!(frame["type"], "message");
        assert_eq!(frame["deliverAs"], "steer");
        assert_eq!(
            frame["content"],
            "Subject: deploy check\n\nPlease verify the staging deploy."
        );
        assert_eq!(frame["meta"]["identity"], "h.worker");
        // An unthreaded message threads on itself, matching the Claude channel.
        assert_eq!(
            frame["meta"]["threadFilename"],
            frame["meta"]["messageFilename"]
        );
    }

    /// The measured omp 18.1.7 classifications, one row per case in
    /// `docs/vrs/06-omp-driver/.experiments/2026-09-05-omp-provider-credential-rejection.md`.
    ///
    /// This is the oracle that keeps st2 out of provider prose. Every 4xx here reaches `AuthFailed`
    /// — omp sets it from the words `401`, `403`, and `forbidden` as readily as from a status — so
    /// a classifier that stopped at that flag would report the exhausted-credits seat that
    /// motivated this work as a refused credential and send its operator to re-login.
    #[test]
    fn only_omps_own_credential_class_becomes_provider_auth() {
        // (case, errorId, is a rejected credential)
        let cases = [
            ("401 invalid x-api-key", 0x100_1000_u64, true),
            ("401 OAuth invalid_grant", 0x100_1000, true),
            ("403 key lacks permission", 0x100_1000, true),
            ("403 run out of credits", 0x108_1000, false),
            ("403 cyber_policy", 0x100_d000, false),
            ("403 CONCURRENT_LIMIT", 0x102_1000, false),
            ("402 insufficient balance", 0x08_1000, false),
            ("429 rate limit", 0x02_1000, false),
        ];

        for (case, error_id, rejected) in cases {
            let frame = json!({
                "type": "turn",
                "error": {"reason": case, "errorId": error_id},
            });
            let result = turn_result(&frame).expect("a turn frame decodes");
            let observed = turn_observation(&result).expect("a failed turn is an observation");
            assert_eq!(
                observed.state,
                harness_state::Activity::Active,
                "a turn that died on the provider needs an operator, not an idle settle: {case}"
            );
            assert_eq!(observed.blocked_on, harness_state::BlockedOn::None);
            if rejected {
                assert_eq!(
                    observed.reason.as_deref(),
                    Some(PROVIDER_AUTH_REASON),
                    "{case}"
                );
                assert_eq!(
                    provider_auth_edge(&result),
                    Some(ProviderAuthEdge::Rejected),
                    "{case}"
                );
            } else {
                assert_eq!(
                    observed.reason.as_deref(),
                    Some(case),
                    "omp's own prose is the only place a reader learns WHICH 4xx this was: {case}"
                );
                assert_eq!(
                    provider_auth_edge(&result),
                    None,
                    "capacity, policy, and concurrency prove nothing about the credential: {case}"
                );
            }
        }

        // A turn that reached its ordinary end: omp leaves `errorId` at 0 and emits no error at
        // all, which is the only positive proof the provider accepted the credential.
        let ordinary_frame = json!({"type": "turn"});
        let ordinary = turn_result(&ordinary_frame).expect("an ordinary end decodes");
        assert!(
            turn_observation(&ordinary).is_none(),
            "the sampled idle poll still owns the settle edge"
        );
        assert_eq!(
            provider_auth_edge(&ordinary),
            Some(ProviderAuthEdge::Accepted)
        );

        // Silence beats a guess: a bare HTTP status (no `qe.Class` bit) and a missing field are
        // both "this reader cannot classify it", never "the credential is fine".
        for unclassified in [json!(403), json!(0), Value::Null] {
            let frame =
                json!({"type": "turn", "error": {"reason": "403 …", "errorId": unclassified}});
            let result = turn_result(&frame).unwrap();
            assert_eq!(provider_auth_edge(&result), None, "{unclassified}");
            assert_eq!(
                turn_observation(&result).unwrap().reason.as_deref(),
                Some("403 …")
            );
        }

        assert!(
            turn_result(&json!({"type": "state", "state": "idle"})).is_none(),
            "the categorical axis is not a turn result"
        );
    }

    /// The whole point of the record: a refused credential is durable, survives the channel process
    /// that saw it, outranks the delivery failures it causes, and is cleared by exactly one thing —
    /// a turn that reached its ordinary end.
    #[test]
    fn a_rejected_omp_credential_stands_until_a_turn_reaches_its_ordinary_end() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        let inbox = message::inbox_dir(agent_dir);
        std::fs::create_dir_all(&inbox).unwrap();
        let record = driver_diagnostic::path(agent_dir);

        let run = |frames: &[&str]| {
            let mut writer =
                harness_state::Writer::new(agent_dir, "h.worker", "omp", Some("h.worker".into()));
            let (tx, rx) = mpsc::channel();
            for frame in frames {
                tx.send(Ok((*frame).to_string())).unwrap();
            }
            drop(tx);
            channel_loop(
                &rx,
                &mut Vec::new(),
                &inbox,
                agent_dir,
                &mut writer,
                None,
                "h.worker",
                &OMP_KIND,
                Duration::from_millis(1),
                Duration::from_secs(60),
            )
            .unwrap();
        };

        run(&[r#"{"type":"turn","error":{"reason":"401 invalid x-api-key","errorId":16781312}}"#]);
        let driver_diagnostic::Observed::Failure(failure) = driver_diagnostic::read(&record) else {
            panic!(
                "a refused omp credential must be a failure: {:?}",
                driver_diagnostic::read(&record)
            )
        };
        assert_eq!(failure.driver, driver_diagnostic::Driver::Omp);
        assert_eq!(failure.stage, driver_diagnostic::Stage::ProviderAuth);
        assert_eq!(
            failure.reason,
            driver_diagnostic::Reason::ProviderAuthRejected
        );
        assert_eq!(failure.source, driver_diagnostic::Source::TurnResult);
        assert!(failure.producer_version.is_none());
        let state: Value = serde_json::from_slice(
            &std::fs::read(harness_state::harness_state_path(agent_dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(state["state"], "active");
        assert_eq!(state["reason"], PROVIDER_AUTH_REASON);

        // A different class in a NEW channel process must not clear it, and neither must the
        // ordinary live traffic that keeps flowing while the seat is wedged.
        run(&[
            r#"{"type":"state","state":"active"}"#,
            r#"{"type":"turn","error":{"reason":"429 rate limit","errorId":135168}}"#,
            r#"{"type":"state","state":"idle"}"#,
        ]);
        assert!(
            matches!(
                driver_diagnostic::read(&record),
                driver_diagnostic::Observed::Failure(_)
            ),
            "only a turn that reached its ordinary end retires this record"
        );

        run(&[r#"{"type":"turn"}"#]);
        assert_eq!(
            driver_diagnostic::read(&record),
            driver_diagnostic::Observed::Absent,
            "the last stage recovering removes the record entirely"
        );
    }

    /// pi's extension has no error-classification field to forward, so this channel has nothing but
    /// provider prose for it — and refuses to publish a credential verdict from prose.
    #[test]
    fn the_pi_channel_publishes_no_credential_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        std::fs::create_dir_all(message::inbox_dir(agent_dir)).unwrap();
        let mut writer =
            harness_state::Writer::new(agent_dir, "h.worker", "pi", Some("h.worker".into()));
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(
            r#"{"type":"turn","error":{"reason":"401 invalid x-api-key","errorId":16781312}}"#
                .to_string(),
        ))
        .unwrap();
        drop(tx);
        channel_loop(
            &rx,
            &mut Vec::new(),
            &message::inbox_dir(agent_dir),
            agent_dir,
            &mut writer,
            None,
            "h.worker",
            &PI_KIND,
            Duration::from_millis(1),
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(PI_KIND.diagnostic_driver, None);
        assert_eq!(
            driver_diagnostic::read(&driver_diagnostic::path(agent_dir)),
            driver_diagnostic::Observed::Absent
        );
    }
}
