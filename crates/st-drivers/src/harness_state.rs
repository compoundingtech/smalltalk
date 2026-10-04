//! Observed harness state: the driver-owned record of what a harness is seen doing.
//!
//! A `harness-state` file (sibling of `status` in the agent's dir) carries the latest observation a
//! session wrapper made of its provider: whether the harness is working, blocked on a human, or
//! ended, plus what its input buffer holds. This is the *observed* axis; `status` remains the
//! *declared* one, and neither speaks for the other. The record is written only by the owning
//! session's driver processes — the wrapper, its channel, or its hooks; one logical owner per
//! record, and nothing outside the driver writes it. Writes happen on state transitions plus a
//! slow heartbeat and follow the presence record's transport discipline: an embedded origin
//! timestamp (never file mtime), atomic tmp+rename writes serialized by a cross-process lock,
//! byte-distinct content on every write that lands, and a derived-only `unknown` — a writer that
//! loses sight of its harness stops heartbeating and lets the record age out rather than
//! refreshing a state it can no longer prove. Restating an unchanged state is free: it touches
//! the record only when the refresh cadence is due, so a chatty producer cannot flood the
//! transport.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// A valid observation at least this old reads as `unknown`. Deliberately its own constant rather
/// than an alias of [`crate::status::STATUS_STALE`]: retuning presence must not silently retune
/// observed harness state.
pub const HARNESS_STATE_STALE: Duration = Duration::from_secs(15 * 60);
/// How often a live writer re-stamps a record it still has evidence for — the presence cadence, so
/// wrappers piggyback on the wakeup they already own.
pub const HARNESS_STATE_REFRESH: Duration = Duration::from_secs(5 * 60);
/// Maximum accepted positive difference between the writer's UTC clock and the reader's clock.
pub const HARNESS_STATE_FUTURE_SKEW: Duration = Duration::from_secs(60);

const SCHEMA: &str = "st.harness-state.v1";
/// The claim-sequence floor sidecar, beside the record: claims stay monotonic even across a
/// record this version cannot parse.
const SEQ_FLOOR_NAME: &str = ".harness-state.seq";
const LOCK_NAME: &str = ".harness-state.lock";

/// What the harness is observed doing. `Child` is reserved: it is part of the contract so a v1
/// reader decodes it, but no producer emits it yet (the screen observer that would have was cut).
/// `Unknown` is DERIVED — staleness, malformation, or a dead session — and is never written; there
/// is no constructor path from missing evidence to `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Activity {
    /// The provider child was successfully spawned, but no harness-specific observer has yet
    /// reported whether it is idle or active. Unlike `Unknown`, this is positive live evidence.
    Ready,
    Idle,
    Active,
    Child,
    /// The session ended or reached a terminal error; nothing further will be observed from this
    /// incarnation without intervention. Unlike the live states, a fresh `Ended` survives the
    /// session-liveness cross-check: a terminal record is *supposed* to outlive its writer.
    Ended,
    #[serde(other)]
    Unknown,
}

impl Activity {
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Ready => "ready",
            Activity::Idle => "idle",
            Activity::Active => "active",
            Activity::Child => "child",
            Activity::Ended => "ended",
            Activity::Unknown => "unknown",
        }
    }
}

/// Who the harness is waiting on. `Human` means the model is stopped and a person is the thing
/// that restarts it (a permission prompt, a review, a question) — neither working nor merely idle.
/// Unrecognized future values decode as `Unknown` (indeterminate), never as `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockedOn {
    None,
    Human,
    #[serde(other)]
    Unknown,
}

impl BlockedOn {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedOn::None => "none",
            BlockedOn::Human => "human",
            BlockedOn::Unknown => "unknown",
        }
    }
}

/// What the harness's composer holds. `Unknown` is writable on this axis: "I cannot see the
/// composer" is itself the observation most producers make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputBuffer {
    Empty,
    Nonempty,
    #[serde(other)]
    Unknown,
}

impl InputBuffer {
    pub fn as_str(self) -> &'static str {
        match self {
            InputBuffer::Empty => "empty",
            InputBuffer::Nonempty => "nonempty",
            InputBuffer::Unknown => "unknown",
        }
    }
}

/// What kind of human ask holds the harness, machine-readably — consumers filter on this axis
/// (`reason` stays diagnostic-only). Meaningful only while `blockedOn` is `human`; writers set
/// `none` otherwise. `Unknown` decodes future words and is never written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Ask {
    #[default]
    None,
    Permission,
    Question,
    Review,
    #[serde(other)]
    Unknown,
}

impl Ask {
    pub fn as_str(self) -> &'static str {
        match self {
            Ask::None => "none",
            Ask::Permission => "permission",
            Ask::Question => "question",
            Ask::Review => "review",
            Ask::Unknown => "unknown",
        }
    }
}

/// The durable record. Additive-tolerant on read (no `deny_unknown_fields`): a reader pinned to an
/// older crate may be older than the writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    schema: String,
    agent: String,
    harness: String,
    state: Activity,
    blocked_on: BlockedOn,
    input_buffer: InputBuffer,
    /// The machine-readable kind of human ask while `blockedOn` is `human`; `none` otherwise.
    /// Absent in records from writers predating the axis, which defaults to `none`.
    #[serde(default)]
    ask: Ask,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background_jobs: Option<u64>,
    /// Diagnostic only. No consumer branches on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// `Ended` only: the exit outcome, e.g. `exit 0` or `signal 9`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit: Option<String>,
    /// The pty session whose liveness vouches for the live states. Same-host readers cross-check
    /// it; a record whose session is provably dead reads `unknown` even while fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pty_session: Option<String>,
    /// The writing session's incarnation token. Ownership is token equality, never a timestamp
    /// comparison: same-millisecond takeovers and lingering predecessor writers are both real.
    /// Empty in records from writers predating the field, which no session owns.
    #[serde(default)]
    incarnation: String,
    /// The monotonic ownership sequence. Only a session claim (a wrapper or session-boundary
    /// writer starting up) advances it, to the on-disk value plus one; every writer refuses to
    /// touch a record whose sequence is beyond its own claim, which is what gives ownership a
    /// DIRECTION — a lingering predecessor's late write cannot replace its successor's record,
    /// while the successor's claim replaces the predecessor's.
    #[serde(default)]
    seq: u64,
    /// When the current state was entered. Survives heartbeat re-stamps.
    since_ms: u64,
    /// The heartbeat: when the writer last held evidence for this state.
    written_at_ms: u64,
    /// Monotonic transition counter. Keeps every write byte-distinct and leaves room for a
    /// compatible transition history later.
    transitions: u64,
}

/// The record's file name inside an agent directory. Named rather than inlined because the
/// replication transport's include list carries it literally: see
/// [`crate::harness_context::REPLICATED_DRIVER_RECORDS`].
pub const RECORD_NAME: &str = "harness-state";

/// The observed-state file: `<agent_dir>/harness-state`.
pub fn harness_state_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(RECORD_NAME)
}

/// One observation as a producer states it: everything except the derived pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub state: Activity,
    pub blocked_on: BlockedOn,
    pub input_buffer: InputBuffer,
    pub ask: Ask,
    pub background_jobs: Option<u64>,
    pub reason: Option<String>,
    pub exit: Option<String>,
}

impl Observation {
    pub fn new(state: Activity, blocked_on: BlockedOn, input_buffer: InputBuffer) -> Self {
        Self {
            state,
            blocked_on,
            input_buffer,
            ask: Ask::None,
            background_jobs: None,
            reason: None,
            exit: None,
        }
    }

    pub fn with_ask(mut self, ask: Ask) -> Self {
        self.ask = ask;
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn with_exit(mut self, exit: impl Into<String>) -> Self {
        self.exit = Some(exit.into());
        self
    }
}

/// The writer a driver process owns over one agent's record. Several driver processes may
/// legitimately hold writers over the same record — a wrapper heartbeat beside hook-process
/// transitions — so every operation takes the record's cross-process lock and treats the on-disk
/// record as the authoritative current state: rename alone is atomic but not isolated, and
/// without the re-read a stale process could resurrect the state it saw before a peer's write.
/// The caller's rule for indeterminacy: when evidence is lost (the observer no longer sees its
/// harness), call nothing — never heartbeat a state you cannot see, and never write `unknown`.
pub struct Writer {
    path: PathBuf,
    lock_path: PathBuf,
    agent: String,
    harness: &'static str,
    pty_session: Option<String>,
    interrupted: bool,
    session: String,
    /// The ownership sequence this writer acts under. `None` = a claiming writer: it resolves at
    /// the first write — adopting the on-disk sequence when the record already carries this
    /// session's token, else claiming on-disk + 1. `Some` = adopted ownership handed down by the
    /// session's claimer (env-exported beside the token).
    claimed_seq: Option<u64>,
}

impl Writer {
    /// The transition counter continues from any readable record already on disk, so restarts and
    /// sibling writers keep writes byte-distinct; an unreadable predecessor starts the counter
    /// fresh.
    pub fn new(
        agent_dir: &Path,
        agent: impl Into<String>,
        harness: &'static str,
        pty_session: Option<String>,
    ) -> Self {
        Self {
            path: harness_state_path(agent_dir),
            lock_path: agent_dir.join(LOCK_NAME),
            agent: agent.into(),
            harness,
            pty_session,
            interrupted: false,
            session: session_token(),
            claimed_seq: None,
        }
    }

    /// Adopt an explicit session incarnation token. Sibling writer processes of one session —
    /// a wrapper beside its hook subprocesses, a channel beside its wrapper — must share one
    /// token (typically minted by the wrapper and exported through the session environment), or
    /// each writes as its own session: restatements open transitions and the wrapper can neither
    /// re-stamp nor terminally fence its siblings' records.
    pub fn with_session(mut self, token: impl Into<String>) -> Self {
        self.session = token.into();
        self
    }

    /// Adopt the full ownership a session's claimer exported — token and claimed sequence
    /// together. A writer holding adopted ownership never claims: it writes only while the
    /// on-disk record's sequence is at or below its claim, so a straggler from a superseded
    /// session is refused in both the live and the terminal path.
    pub fn with_ownership(mut self, token: impl Into<String>, seq: u64) -> Self {
        self.session = token.into();
        self.claimed_seq = Some(seq);
        self
    }

    /// Mark this writer's observation stream discontinuous: its evidence was lost and has since
    /// returned. The next observation opens a fresh transition even if it restates the
    /// pre-interruption tuple — continuity (`sinceMs`, the counter) must never be claimed across
    /// an interval the observer did not see.
    pub fn interrupt(&mut self) {
        self.interrupted = true;
    }

    /// Hold the record's exclusive cross-process lock for one read→decide→rename cycle. The lock
    /// file is a permanent sibling; the guard releases on drop (close).
    fn locked(&self) -> anyhow::Result<crate::flock::FileLock> {
        lock_exclusive(&self.lock_path)
    }

    /// Record an observation. A genuine change writes a new transition with a fresh `since`. An
    /// observation identical to the on-disk record is a no-op while that record is fresh —
    /// producers may restate their state arbitrarily often (an SSE stream restates several times
    /// per second, measured) and only the refresh cadence may reach the transport — and becomes a
    /// heartbeat-equivalent re-stamp once the record is older than [`HARNESS_STATE_REFRESH`].
    /// `Unknown` state is derived and cannot be written.
    pub fn observe(&mut self, observation: Observation) -> anyhow::Result<()> {
        self.observe_inner(observation, false).map(|_wrote| ())
    }

    /// [`Writer::observe`], except a live-state frame is dropped (returning `false`) when the
    /// on-disk record is already terminal. Not a general rule — a harness may legally report
    /// activity after a terminal error, and Codex does — but a producer whose live frames and
    /// terminal record come from different processes opts in so a queued live frame can never
    /// overwrite the incarnation's last word.
    pub fn observe_unless_ended(&mut self, observation: Observation) -> anyhow::Result<bool> {
        self.observe_inner(observation, true)
    }

    fn observe_inner(
        &mut self,
        observation: Observation,
        skip_if_ended: bool,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            observation.state != Activity::Unknown,
            "unknown is derived and cannot be written"
        );
        anyhow::ensure!(
            observation.state == Activity::Ended || self.pty_session.is_some(),
            "live observations require a pty session to vouch for them"
        );
        anyhow::ensure!(
            observation.ask != Ask::Unknown,
            "unknown is derived and cannot be written"
        );
        anyhow::ensure!(
            observation.ask == Ask::None || observation.blocked_on == BlockedOn::Human,
            "an ask kind is meaningful only while blocked on a human"
        );
        let _lock = self.locked()?;
        let on_disk = match read_stored(&self.path) {
            StoredRecord::Parsed(record) => Some(record),
            StoredRecord::Absent => None,
            // Bytes this version cannot parse are somebody's record, not a virgin seat: a
            // non-claiming writer refuses rather than restarting the sequence and counter over
            // foreign state. Only the explicit written claim supersedes.
            StoredRecord::Unreadable => return Ok(false),
        };
        // Resolve this writer's ownership sequence, then enforce its direction. A claiming
        // writer adopts the on-disk sequence when the record already carries its token (a
        // sibling wrote first) and claims on-disk + 1 otherwise; an adopted-ownership writer
        // holds whatever its session's claimer exported. Either way, a record whose sequence is
        // beyond the claim belongs to a LATER session: this writer is the straggler, and its
        // write — live or terminal — is refused rather than replacing its successor's record.
        let seq = match self.claimed_seq {
            Some(seq) => seq,
            // A token-only writer NEVER claims: it adopts the on-disk sequence when the record
            // already carries its token, starts a virgin record at one, and is refused outright
            // against a foreign token — a mixed-version straggler minting claims would fence
            // the true successor out permanently. New sequences are minted only by [`claim`],
            // the written act, and adopted from it.
            None => match on_disk.as_ref() {
                // A virgin seat's first write mints sequence one — initial ownership,
                // exactly what [`claim_locked`] establishes — so it persists the floor
                // sidecar too: if this record later goes unreadable, a replacement claim
                // must continue past it instead of colliding with this lingering writer.
                None => {
                    persist_floor(&self.path, 1);
                    1
                }
                Some(current) if current.incarnation == self.session => current.seq,
                Some(_) => return Ok(false),
            },
        };
        if on_disk.as_ref().is_some_and(|current| current.seq > seq) {
            return Ok(false);
        }
        // A foreign schema's `seq` decodes as serde-default zero, which every claim exceeds — a
        // v1 straggler would otherwise replace a v2 record it cannot even read. Non-claiming
        // writers refuse foreign schemas outright; only the explicit written [`claim`]
        // supersedes an unsupported schema.
        if on_disk
            .as_ref()
            .is_some_and(|current| !crate::contracts::schema_matches(&current.schema, SCHEMA))
        {
            return Ok(false);
        }
        self.claimed_seq = Some(seq);
        // Ownership is token equality: a record is this writer's only when it carries both this
        // version's schema and this session's incarnation. Anything else — a foreign schema, a
        // predecessor's or successor's token, the empty pre-token form — is never coalesced
        // against and never treated as this session's terminal word; a genuine observation
        // replaces it wholesale (one logical owner per record), continuing the counter for
        // byte-distinctness. Timestamps deliberately play no part: a same-millisecond takeover
        // and a lingering predecessor writer are both real and both ambiguous by clock.
        let own_record = on_disk.as_ref().filter(|current| {
            crate::contracts::schema_matches(&current.schema, SCHEMA)
                && current.incarnation == self.session
        });
        if skip_if_ended
            && own_record.is_some_and(|current| {
                // Only a REAL terminal record from this session suppresses queued live frames:
                // one carrying an exit, which every wrapper's `ended` does. The claim record —
                // this session's own `ended (superseded)` placeholder, deliberately exitless —
                // must not suppress the session's first frames, and a predecessor incarnation's
                // `ended` is history rather than this session's last word.
                current.state == Activity::Ended && current.exit.is_some()
            })
        {
            return Ok(false);
        }
        let now_ms = crate::message::now_ms();
        let unchanged = !self.interrupted
            && own_record.is_some_and(|current| {
                current.state == observation.state
                    && current.blocked_on == observation.blocked_on
                    && current.input_buffer == observation.input_buffer
                    && current.ask == observation.ask
                    && current.background_jobs == observation.background_jobs
                    && current.reason == observation.reason
                    && current.exit == observation.exit
            });
        if unchanged
            && let Some(current) = own_record
            // A restatement is a no-op only against a record this session already wrote (the
            // token filter above) whose stamp a reader would trust: a stamp beyond the
            // future-skew bound — a backward clock correction's leftover — would otherwise
            // read "fresh" here forever while every reader derives future-skew unknown, so it
            // falls through to the write below, whose next_stamp resets to the writer's clock.
            && current.written_at_ms <= now_ms.saturating_add(duration_ms(HARNESS_STATE_FUTURE_SKEW))
            && now_ms.saturating_sub(current.written_at_ms) < duration_ms(HARNESS_STATE_REFRESH)
        {
            return Ok(true);
        }
        // A landed write is byte-distinct even against a same-millisecond predecessor: the stamp
        // is strictly monotonic per record, at the cost of a bounded forward skew of at most one
        // millisecond per write (writes are transition-scale, so the skew never accumulates
        // meaningfully against the staleness horizon). A stamp is only ever inherited from a
        // record a reader would trust: one already past the future-skew bound is somebody's
        // garbage (or an overflow probe), and inheriting it would poison every later write —
        // the writer's own clock wins instead.
        let written_at_ms = next_stamp(on_disk.as_deref(), now_ms);
        let (since_ms, transitions) = match (own_record, unchanged) {
            (Some(current), true) => (current.since_ms, current.transitions),
            (Some(current), false) => (written_at_ms, current.transitions.saturating_add(1)),
            (None, _) => (
                written_at_ms,
                on_disk
                    .as_ref()
                    .map_or(0, |current| current.transitions.saturating_add(1)),
            ),
        };
        let record = Record {
            schema: own_record.map_or_else(|| SCHEMA.to_owned(), |current| current.schema.clone()),
            agent: self.agent.clone(),
            harness: self.harness.to_string(),
            state: observation.state,
            blocked_on: observation.blocked_on,
            input_buffer: observation.input_buffer,
            ask: observation.ask,
            background_jobs: observation.background_jobs,
            reason: observation.reason,
            exit: observation.exit,
            pty_session: self.pty_session.clone(),
            incarnation: self.session.clone(),
            seq,
            since_ms,
            written_at_ms,
            transitions,
        };
        write_record(&self.path, &record)?;
        self.interrupted = false;
        Ok(true)
    }

    /// Re-stamp the heartbeat for whatever live state is on disk. Nothing on disk means nothing
    /// to keep fresh, and a terminal record is never re-stamped. The on-disk record is
    /// authoritative: a wrapper heartbeat re-stamps the newest state, including one a hook or
    /// channel process wrote after this writer's last observation. A predecessor session's record
    /// — one written before this session started — is preserved for counter continuity but is
    /// never heartbeat-eligible: re-stamping it would keep a dead session's state fresh forever.
    /// It becomes eligible once any writer of this session observes something.
    pub fn heartbeat(&mut self) -> anyhow::Result<()> {
        let _lock = self.locked()?;
        let Some(mut current) = read_record(&self.path) else {
            return Ok(());
        };
        // A schema this writer does not own must not be round-tripped through this version's
        // record type, and a record this *session* does not own must never be kept fresh: a
        // lingering predecessor re-stamping its successor's record would keep a dead seat's
        // state alive for cross-host readers, and a successor re-stamping a predecessor's would
        // resurrect history. Token equality decides, in both directions.
        if !crate::contracts::schema_matches(&current.schema, SCHEMA)
            || current.state == Activity::Ended
            || current.incarnation != self.session
        {
            return Ok(());
        }
        let now_ms = crate::message::now_ms();
        current.written_at_ms = if current.written_at_ms
            <= now_ms.saturating_add(duration_ms(HARNESS_STATE_FUTURE_SKEW))
        {
            now_ms.max(current.written_at_ms.saturating_add(1))
        } else {
            // Never inherit an untrusted future stamp — reset to this writer's clock.
            now_ms
        };
        write_record(&self.path, &current)
    }

    /// Write the terminal record for this session. Idempotent-shaped: callers on racing teardown
    /// paths may both call it.
    pub fn ended(&mut self, exit: impl Into<String>) -> anyhow::Result<()> {
        self.observe(
            Observation::new(Activity::Ended, BlockedOn::None, InputBuffer::Unknown)
                .with_exit(exit),
        )
    }
}

/// The derived view a consumer reads. `state` already folds in staleness, future skew,
/// malformation, and (when a probe is supplied) session liveness; `reason` names which derivation
/// produced an `unknown`, so no absence is silent.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub state: Activity,
    pub blocked_on: BlockedOn,
    pub input_buffer: InputBuffer,
    pub ask: Ask,
    pub background_jobs: Option<u64>,
    pub harness: Option<String>,
    pub since_ms: Option<u64>,
    /// When the owning driver last refreshed this evidence.
    pub observed_at_ms: Option<u64>,
    /// Monotonic ownership fence from the driver record.
    pub ownership_sequence: Option<u64>,
    /// Monotonic state-transition counter from the driver record.
    pub transition_sequence: Option<u64>,
    /// Driver-session identity that owns the source record.
    pub evidence_incarnation: Option<String>,
    pub exit: Option<String>,
    pub reason: Option<String>,
}

impl Observed {
    fn indeterminate(reason: &str, harness: Option<String>) -> Self {
        // The single constructor for an indeterminate observation: every absence routes here, so
        // no path can derive `idle` — or anything else — from missing evidence.
        Self {
            state: Activity::Unknown,
            blocked_on: BlockedOn::Unknown,
            input_buffer: InputBuffer::Unknown,
            ask: Ask::Unknown,
            background_jobs: None,
            harness,
            since_ms: None,
            observed_at_ms: None,
            ownership_sequence: None,
            transition_sequence: None,
            evidence_incarnation: None,
            exit: None,
            reason: Some(reason.to_string()),
        }
    }

    fn with_record_evidence(mut self, record: &Record) -> Self {
        self.harness = Some(record.harness.clone());
        self.observed_at_ms = Some(record.written_at_ms);
        self.ownership_sequence = Some(record.seq);
        self.transition_sequence = Some(record.transitions);
        self.evidence_incarnation = Some(record.incarnation.clone());
        self
    }
}

/// Result of a same-host session-liveness probe. `Indeterminate` (an unreadable registry, e.g. a
/// reader without the session dir the writer used) must not downgrade anything: unprovable
/// evidence is never reported as death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLiveness {
    Alive,
    Dead,
    Indeterminate,
}

/// Read an agent's observed harness state. `None` means no record exists — no driver has ever
/// observed this agent, which is different from `unknown`. `probe` is the optional same-host
/// liveness cross-check for the record's pty session; pass `None` for cross-host reads.
pub fn read(path: &Path, probe: Option<&dyn Fn(&str) -> SessionLiveness>) -> Option<Observed> {
    let raw = match crate::harness_events::read_record(path) {
        Ok(raw) => raw,
        // Only proven absence is absence; a record that exists but cannot be read is
        // indeterminate, never silently "no observation".
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(Observed::indeterminate("unreadable-record", None)),
    };
    Some(read_raw_at(&raw, probe, crate::message::now_ms()))
}

pub fn read_raw_at(
    raw: &[u8],
    probe: Option<&dyn Fn(&str) -> SessionLiveness>,
    now_ms: u64,
) -> Observed {
    let Ok(record) = serde_json::from_slice::<Record>(raw) else {
        return Observed::indeterminate("malformed-record", None);
    };
    let harness = Some(record.harness.clone());
    // The discriminator gates interpretation: a future schema's words may be spelled like this
    // version's while meaning something else, so nothing definite may be derived from them.
    if !crate::contracts::schema_matches(&record.schema, SCHEMA) {
        return Observed::indeterminate("unsupported-schema", harness);
    }
    if record.written_at_ms > now_ms {
        if record.written_at_ms - now_ms > duration_ms(HARNESS_STATE_FUTURE_SKEW) {
            return Observed::indeterminate("future-skew", harness).with_record_evidence(&record);
        }
    } else if now_ms - record.written_at_ms >= duration_ms(HARNESS_STATE_STALE) {
        return Observed::indeterminate("stale", harness).with_record_evidence(&record);
    }
    if record.state == Activity::Unknown {
        // A literal `unknown` is never written by this crate; treat one like malformation.
        return Observed::indeterminate("literal-unknown", harness).with_record_evidence(&record);
    }
    if record.state == Activity::Ended
        && record.exit.is_none()
        && record.reason.as_deref() == Some("superseded")
    {
        // The claim placeholder is a fence, not an observation: the session wrote it at startup
        // and has observed nothing yet. Reading it as definite `ended` would flip a live seat
        // to dead for every consumer whose harness never publishes its first frame promptly —
        // indeterminate, distinctly, until the first real observation or the ordinary horizon.
        return Observed::indeterminate("claimed", harness).with_record_evidence(&record);
    }
    if record.state != Activity::Ended
        && let Some(probe) = probe
    {
        // Same-host readers cross-check live states against the session registry. A live record
        // that names no session offers nothing to check — without this rule it would stay
        // definite through an external SIGKILL for the whole staleness horizon, which is exactly
        // the window the cross-check exists to close. Writers therefore must fence live states
        // (enforced in `observe`); a fenced record whose session is provably dead is downgraded,
        // and an unreadable registry still downgrades nothing.
        let Some(session) = record.pty_session.as_deref() else {
            return Observed::indeterminate("unfenced-record", harness)
                .with_record_evidence(&record);
        };
        if probe(session) == SessionLiveness::Dead {
            return Observed::indeterminate("session-dead", harness).with_record_evidence(&record);
        }
    }
    Observed {
        state: record.state,
        blocked_on: record.blocked_on,
        input_buffer: record.input_buffer,
        ask: record.ask,
        background_jobs: record.background_jobs,
        harness,
        since_ms: Some(record.since_ms),
        observed_at_ms: Some(record.written_at_ms),
        ownership_sequence: Some(record.seq),
        transition_sequence: Some(record.transitions),
        evidence_incarnation: Some(record.incarnation),
        exit: record.exit,
        reason: record.reason,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What the record file holds, tri-state: absence, bytes this version cannot parse, or a parsed
/// record. Collapsing `Unreadable` into `Absent` would let a writer treat an undeserializable
/// v2 record as a virgin seat — restarting the sequence and counter over live foreign state.
enum StoredRecord {
    Absent,
    Unreadable,
    Parsed(Box<Record>),
}

fn read_stored(path: &Path) -> StoredRecord {
    match crate::harness_events::read_record(path) {
        // Only proven absence is absence: a file that exists but cannot be read (permissions,
        // IO) is somebody's record — treating it as a virgin seat would let a token-only write
        // or a wrapperless claim rename over live state it never saw.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredRecord::Absent,
        Err(_) => StoredRecord::Unreadable,
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(record) => StoredRecord::Parsed(Box::new(record)),
            Err(_) => StoredRecord::Unreadable,
        },
    }
}

fn read_record(path: &Path) -> Option<Record> {
    match read_stored(path) {
        StoredRecord::Parsed(record) => Some(*record),
        StoredRecord::Absent | StoredRecord::Unreadable => None,
    }
}

fn write_record(path: &Path, record: &Record) -> anyhow::Result<()> {
    // This record stages beside itself, unchanged: the sibling driver record
    // ([`crate::harness_context`]) stages outside the agent subtree because a replicated
    // temporary name becomes a durable key, and moving this one's staging is a separate change.
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    write_json_atomic(path, record, &dir, ".harness-state")
}

/// Take one driver record's exclusive cross-process lock, held for a read→decide→rename cycle.
/// The lock file is a permanent sibling of the record and the guard releases on drop (close).
/// Shared with [`crate::harness_context`], which owns a sibling record with its own lock file:
/// the transport is common, the ownership protocol above it is not.
pub(crate) fn lock_exclusive(lock_path: &Path) -> anyhow::Result<crate::flock::FileLock> {
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let lock = crate::flock::open(lock_path, crate::flock::Open::Create)?;
    crate::flock::FileLock::hold_blocking(lock, crate::flock::Mode::Exclusive)
        .with_context(|| format!("locking {} failed", lock_path.display()))
}

/// Stage-and-rename one newline-terminated JSON record. Atomic when `staging_dir` is on the
/// record's filesystem, which every caller must ensure — `staging_dir` is explicit precisely
/// because the two driver records answer "where may a temporary name live" differently.
pub(crate) fn write_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    staging_dir: &Path,
    tmp_prefix: &str,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if let Some(dir) = path.parent()
        && crate::harness_events::enabled(dir)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "harness-state" | "harness-context"))
    {
        let kind = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("record name")?;
        return crate::harness_events::write_snapshot(dir, kind, &bytes);
    }
    bytes.push(b'\n');
    crate::fsatomic::replace(
        path,
        &bytes,
        crate::fsatomic::Staging::new(tmp_prefix).in_dir(staging_dir),
        crate::fsatomic::Durability::Rename,
    )?;
    Ok(())
}

/// The per-record monotonic stamp: strictly beyond the on-disk stamp when that stamp is inside
/// the future-skew trust bound (a stamp beyond it is somebody's garbage or an overflow probe,
/// and inheriting it would poison every later write), and the writer's own clock otherwise.
fn next_stamp(on_disk: Option<&Record>, now_ms: u64) -> u64 {
    on_disk
        .map(|current| current.written_at_ms)
        .filter(|&previous| {
            previous <= now_ms.saturating_add(duration_ms(HARNESS_STATE_FUTURE_SKEW))
        })
        .map_or(now_ms, |previous| now_ms.max(previous.saturating_add(1)))
}

/// Claim session ownership of an agent's record, as a WRITTEN act under the record's lock: the
/// takeover record supersedes whatever is on disk — `ended` with reason `superseded`, no exit,
/// the new session's token, and the next ownership sequence — and the claimed sequence is
/// returned for the wrapper to adopt and export beside its token. Writing the claim makes it
/// atomic: racing claimers serialize on the lock and mint DISTINCT sequences, and a
/// predecessor's fresh live record is superseded at relaunch even though the pty-name-based
/// probe cannot tell the sessions apart. Readers derive indeterminate (`claimed`) from the
/// fresh placeholder — a fence, not an observation — until the session's first real
/// observation replaces it.
pub fn claim(
    agent_dir: &Path,
    agent: impl Into<String>,
    harness: &'static str,
    token: &str,
) -> anyhow::Result<u64> {
    let writer = Writer::new(agent_dir, agent, harness, None);
    let _lock = writer.locked()?;
    claim_locked(&writer, token)
}

/// Run `action` only while `incarnation` and `seq` are the exact current owner of an agent's
/// harness record. The record lock stays held through the action so a successor cannot claim
/// between the ownership check and the caller's related publication.
pub(crate) fn with_current_ownership<T>(
    agent_dir: &Path,
    incarnation: &str,
    seq: u64,
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = harness_state_path(agent_dir);
    let _lock = lock_exclusive(&agent_dir.join(LOCK_NAME))?;
    let current = read_record(&path).context("current harness ownership record is unavailable")?;
    anyhow::ensure!(
        crate::contracts::schema_matches(&current.schema, SCHEMA)
            && current.incarnation == incarnation
            && current.seq == seq,
        "harness ownership was superseded"
    );
    action()
}

/// The claim's body, under an already-held record lock.
fn claim_locked(writer: &Writer, token: &str) -> anyhow::Result<u64> {
    // Unreadable bytes are superseded like anything else — that is exactly what the claim is
    // for — but their CONTENT cannot be continued (the counter restarts). The SEQUENCE must
    // survive them regardless: a claim restarting at one would sit below a lingering
    // predecessor's claim, whose next write would replace the new claim and then permanently
    // fence the new session out. The floor sidecar, written under this same lock on every
    // claim, preserves monotonicity across records this version cannot parse; only both files
    // being damaged loses the floor, and that residual is documented.
    let on_disk = match read_stored(&writer.path) {
        StoredRecord::Parsed(record) => Some(record),
        StoredRecord::Absent | StoredRecord::Unreadable => None,
    };
    let floor_path = writer.path.with_file_name(SEQ_FLOOR_NAME);
    let floor = fs::read_to_string(&floor_path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    let highest = on_disk.as_ref().map(|record| record.seq).max(floor);
    // A saturated sequence would mint SHARED ownership forever after: every later claim would
    // return the same MAX, and two sessions holding equal claims are exactly the ambiguity the
    // sequence exists to remove. Fail loudly; producers degrade to token-only and stay alive.
    anyhow::ensure!(
        highest.is_none_or(|seq| seq < u64::MAX),
        "ownership sequence exhausted; refusing a shared claim"
    );
    let seq = highest.map_or(1, |seq| seq.saturating_add(1));
    let now_ms = crate::message::now_ms();
    let written_at_ms = next_stamp(on_disk.as_deref(), now_ms);
    let record = Record {
        schema: on_disk
            .as_ref()
            .filter(|record| {
                crate::contracts::schema_matches(&record.schema, SCHEMA)
                    && record.incarnation == token
            })
            .map_or_else(|| SCHEMA.to_owned(), |record| record.schema.clone()),
        agent: writer.agent.clone(),
        harness: writer.harness.to_string(),
        state: Activity::Ended,
        blocked_on: BlockedOn::None,
        input_buffer: InputBuffer::Unknown,
        ask: Ask::None,
        background_jobs: None,
        reason: Some("superseded".to_string()),
        exit: None,
        pty_session: None,
        incarnation: token.to_string(),
        seq,
        since_ms: written_at_ms,
        written_at_ms,
        transitions: on_disk
            .as_ref()
            .map_or(0, |record| record.transitions.saturating_add(1)),
    };
    write_record(&writer.path, &record)?;
    // The floor accompanies every act that establishes ownership; its own failure
    // modes must never be quiet ones.
    persist_floor(&writer.path, seq);
    // A session boundary empties the window, so the numeric sibling is removed with the same
    // act that supersedes this record (HC-R15): the new incarnation reads "no context yet"
    // rather than the previous one's 190k, which is what a crash-looping seat would otherwise
    // show for the whole hour of that record's horizon. This runs while THIS record's lock is
    // held and takes the sibling's lock inside it, so the order is state → context. That is
    // the only place the two are ever held together and `harness_context` never takes this
    // one, so the ordering is acyclic and no writer can deadlock against it. The claim stands
    // whether or not the removal succeeds, but never silently.
    if let Some(agent_dir) = writer.path.parent()
        && let Err(error) = crate::harness_context::remove(agent_dir)
    {
        tracing::warn!(
            "st harness-state: clearing the harness-context record for {} failed: {error}",
            agent_dir.display()
        );
    }
    Ok(seq)
}

/// Persist the sequence-floor sidecar for `seq`. The floor is the safety net for the record
/// itself going unreadable, so its own failure modes must not be quiet ones: stage-and-rename
/// keeps a torn write from corrupting the current floor, and a failed write is logged — the
/// ownership still stands (losing the floor only matters if the record later becomes
/// unreadable), but never silently.
///
/// The staging name carries the writer's pid and a counter for the same reason every other
/// publication's does. It used to be the fixed literal `.harness-state.seq.tmp`, written with a
/// truncating, symlink-following `fs::write`: two writers persisting a floor for one agent shared
/// that one path, so each could truncate and rename the other's half-written bytes — a torn floor,
/// which defeats exactly the failure the floor exists for. One caller holds the record's lock; the
/// other is the token-only virgin-record path, whose lock coverage is not established here, so the
/// staging name must not depend on it.
fn persist_floor(record_path: &Path, seq: u64) {
    let floor_path = record_path.with_file_name(SEQ_FLOOR_NAME);
    if let Err(error) = crate::fsatomic::replace(
        &floor_path,
        format!("{seq}\n").as_bytes(),
        crate::fsatomic::Staging::new(SEQ_FLOOR_NAME),
        crate::fsatomic::Durability::Rename,
    ) {
        tracing::warn!(
            "st harness-state: writing the sequence floor {} failed: {error}",
            floor_path.display()
        );
    }
}

/// The token prefix wrapperless Claude sessions derive from Claude's own session id.
pub const WRAPPERLESS_PREFIX: &str = "claude-session-";

/// A WRAPPERLESS session boundary's claim — eligibility and the written takeover as ONE act
/// under the record lock, because check-then-act across two acquisitions is a race: a
/// hooks-only SessionStart landing between a wrapper's startup reads could otherwise steal the
/// sequence the wrapper was about to export. A wrapper's claim is always legitimate — it owns
/// the seat's lifecycle — but a wrapperless claimer (a hook fired by any interactive session
/// that inherited the project-scoped registration) must not supersede live wrapper state. It
/// claims over nothing, over records no wrapper minted (fellow wrapperless tokens), over REAL
/// terminal records (exit-bearing), and over staleness — never over a live wrapper record, and
/// never over a wrapper's FRESH claim placeholder (`ended (superseded)`, exitless,
/// wrapper-shaped token): that placeholder is a session mid-startup, not an ended one, though
/// an abandoned placeholder past the staleness horizon is claimable like any orphan.
/// `Ok(None)` = ineligible; unreadable bytes are also ineligible for this cautious path.
pub fn claim_wrapperless(
    agent_dir: &Path,
    agent: impl Into<String>,
    harness: &'static str,
    token: &str,
) -> anyhow::Result<Option<u64>> {
    let writer = Writer::new(agent_dir, agent, harness, None);
    let _lock = writer.locked()?;
    let eligible = match read_stored(&writer.path) {
        StoredRecord::Absent => true,
        StoredRecord::Unreadable => false,
        StoredRecord::Parsed(record) => {
            let now_ms = crate::message::now_ms();
            let stale =
                now_ms.saturating_sub(record.written_at_ms) >= duration_ms(HARNESS_STATE_STALE);
            let wrapperless_owner =
                record.incarnation.is_empty() || record.incarnation.starts_with(WRAPPERLESS_PREFIX);
            let real_terminal = record.state == Activity::Ended && record.exit.is_some();
            wrapperless_owner || real_terminal || stale
        }
    };
    if !eligible {
        return Ok(None);
    }
    claim_locked(&writer, token).map(Some)
}

/// A process-unique session incarnation token: pid, wall-clock, and a process-local counter.
/// Uniqueness across the writers that can actually race on one record (processes on one host)
/// is what matters; no cryptographic strength is implied or needed.
pub fn session_token() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        crate::message::now_ms(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    /// [`write_json_atomic`]'s contract: one newline-terminated JSON record, replaced whole,
    /// staged in the directory the CALLER named, and owner-only. The staging directory is not a
    /// detail — the harness-context record stages in the catalog control plane precisely because a
    /// staged name inside the replicated `agents` namespace becomes a durable replicated key
    /// (INVARIANTS row 29, HC-R05) — so an unusable staging directory must fail the publication
    /// instead of quietly staging beside the record. Proven with a staging path that is a regular
    /// file, which no uid can turn into a directory.
    ///
    /// The mode is the deliberate change of the fold onto `fsatomic`: this pair used to be
    /// published at whatever an ordinary write produces (`0644` under the fleet's umask). These
    /// are the two records a replication transport's include list names (HC-R05), and no such
    /// transport runs on the fleet today (`DQ-C1`/`DQ-H2`), so nothing reads them as another uid;
    /// the tightening is recorded against HC-T08 for whoever adopts one.
    #[test]
    fn a_record_is_one_json_line_staged_in_the_directory_the_caller_named() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agents/example-linux/worker");
        let path = harness_state_path(&agent_dir);
        let staging = tmp.path().join("staging");
        let record = serde_json::json!({"schema": "test"});

        write_json_atomic(&path, &record, &staging, ".harness-state").unwrap();
        write_json_atomic(&path, &record, &staging, ".harness-state").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"schema\":\"test\"}\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the driver record is published owner-only"
        );

        for dir in [&agent_dir, &staging] {
            let residue = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".harness-state.tmp-"))
                .collect::<Vec<_>>();
            assert!(
                residue.is_empty(),
                "staging residue in {dir:?}: {residue:?}"
            );
        }

        let blocked = tmp.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        assert!(
            write_json_atomic(&path, &record, &blocked, ".harness-state").is_err(),
            "an unusable staging directory must fail the publication, not fall back"
        );
    }

    fn writer(dir: &Path) -> Writer {
        Writer::new(dir, "example-linux.worker", "codex", Some("worker".to_string()))
    }

    /// A new session arriving the way real wrappers do: a written claim, then adoption.
    fn takeover(dir: &Path, harness: &'static str) -> Writer {
        let token = session_token();
        let seq = claim(dir, "example-linux.worker", harness, &token).unwrap();
        Writer::new(dir, "example-linux.worker", harness, Some("worker".to_string()))
            .with_ownership(token, seq)
    }

    fn active() -> Observation {
        Observation::new(Activity::Active, BlockedOn::None, InputBuffer::Unknown)
    }

    #[test]
    fn missing_record_reads_as_none_not_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(&harness_state_path(tmp.path()), None), None);
    }

    #[test]
    fn observe_then_read_roundtrips_every_writable_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = writer(tmp.path());
        for (state, blocked, buffer) in [
            (Activity::Idle, BlockedOn::None, InputBuffer::Empty),
            (Activity::Active, BlockedOn::Human, InputBuffer::Unknown),
            (Activity::Child, BlockedOn::None, InputBuffer::Nonempty),
            (Activity::Ended, BlockedOn::None, InputBuffer::Unknown),
        ] {
            writer
                .observe(Observation::new(state, blocked, buffer))
                .unwrap();
            let observed = read(&harness_state_path(tmp.path()), None).unwrap();
            assert_eq!(observed.state, state);
            assert_eq!(observed.blocked_on, blocked);
            assert_eq!(observed.input_buffer, buffer);
            assert_eq!(observed.harness.as_deref(), Some("codex"));
            assert!(observed.since_ms.is_some());
        }
    }

    #[test]
    fn unknown_state_is_derived_and_cannot_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = writer(tmp.path());
        assert!(
            writer
                .observe(Observation::new(
                    Activity::Unknown,
                    BlockedOn::None,
                    InputBuffer::Unknown,
                ))
                .is_err()
        );
        assert_eq!(read(&harness_state_path(tmp.path()), None), None);
    }

    #[test]
    fn every_landed_write_is_byte_distinct_and_fresh_restatements_do_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());

        writer.observe(active()).unwrap();
        let first = fs::read(&path).unwrap();

        // Restating an unchanged state against a fresh record must not touch it…
        std::thread::sleep(Duration::from_millis(2));
        writer.observe(active()).unwrap();
        assert_eq!(
            first,
            fs::read(&path).unwrap(),
            "fresh identical observation must not write"
        );

        // …while an explicit heartbeat and a genuine transition each land distinct bytes.
        std::thread::sleep(Duration::from_millis(2));
        writer.heartbeat().unwrap();
        let second = fs::read(&path).unwrap();
        assert_ne!(first, second, "heartbeat must change bytes");

        std::thread::sleep(Duration::from_millis(2));
        writer
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        let third = fs::read(&path).unwrap();
        assert_ne!(second, third, "transition must change bytes");
    }

    /// The measured failure mode this guards: an SSE-fed producer restating its state ~3×/second
    /// turned into 679 byte-distinct replicated writes in 221 s. Restatements are free.
    #[test]
    fn a_chatty_producer_restating_its_state_causes_zero_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());

        writer.observe(active()).unwrap();
        let bytes = fs::read(&path).unwrap();
        for _ in 0..300 {
            writer.observe(active()).unwrap();
        }
        assert_eq!(bytes, fs::read(&path).unwrap());
        let record: Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.transitions, 0);
    }

    #[test]
    fn an_unchanged_observation_re_stamps_only_a_record_older_than_the_refresh_cadence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());
        writer.observe(active()).unwrap();

        // Age this session's own record past the refresh cadence without changing its owner.
        let mut aged: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        aged.written_at_ms = crate::message::now_ms() - duration_ms(HARNESS_STATE_REFRESH) - 1;
        aged.since_ms = aged.written_at_ms;
        write_record(&path, &aged).unwrap();

        // The unchanged restatement now lands as a heartbeat-equivalent re-stamp: same state,
        // same transition, fresh stamp.
        writer.observe(active()).unwrap();
        let restamped: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(restamped.transitions, aged.transitions);
        assert_eq!(restamped.since_ms, aged.since_ms);
        assert!(restamped.written_at_ms > aged.written_at_ms);
    }

    #[test]
    fn concurrent_writers_defer_to_the_on_disk_record_not_their_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut a = writer(tmp.path());
        a.observe(active()).unwrap();
        let mut b = takeover(tmp.path(), "codex");
        b.observe(Observation::new(
            Activity::Idle,
            BlockedOn::None,
            InputBuffer::Unknown,
        ))
        .unwrap();

        // A's heartbeat re-stamps the newest state on disk — it must not resurrect `active`.
        a.heartbeat().unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.state, Activity::Idle);
        assert_eq!(record.transitions, 2);

        // B's write was a later session's claim, so A is now the straggler: its re-observation
        // is refused rather than treated as a fresh takeover of its successor's record.
        a.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.state, Activity::Idle);
        assert_eq!(record.transitions, 2);
    }

    #[test]
    fn a_heartbeat_never_resurrects_a_peer_processes_terminal_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut a = writer(tmp.path());
        a.observe(active()).unwrap();
        let mut b = takeover(tmp.path(), "codex");
        b.ended("signal 9").unwrap();
        let terminal = fs::read(&path).unwrap();

        a.heartbeat().unwrap();
        assert_eq!(fs::read(&path).unwrap(), terminal);
        let observed = read(&path, None).unwrap();
        assert_eq!(observed.state, Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }

    #[test]
    fn identical_observations_coalesce_without_a_new_transition() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());

        writer.observe(active()).unwrap();
        let entered: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        writer.observe(active()).unwrap();
        let restated: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            restated.since_ms, entered.since_ms,
            "since survives restating"
        );
        assert_eq!(restated.transitions, entered.transitions);

        writer
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        let transitioned: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(transitioned.transitions, entered.transitions + 1);
        assert!(transitioned.since_ms >= restated.since_ms);
    }

    #[test]
    fn restart_continues_the_transition_counter() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut first = writer(tmp.path());
        first.observe(active()).unwrap();
        first
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        drop(first);

        let mut second = takeover(tmp.path(), "codex");
        second.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            record.transitions, 3,
            "claim and first observation continue the counter"
        );
    }

    #[test]
    fn staleness_and_future_skew_derive_unknown_with_distinct_reasons() {
        let now_ms = 2_000_000_000_u64;
        let stale_ms = duration_ms(HARNESS_STATE_STALE);
        let skew_ms = duration_ms(HARNESS_STATE_FUTURE_SKEW);
        let raw = |written_at_ms: u64| {
            serde_json::to_vec(&Record {
                schema: SCHEMA.to_string(),
                agent: "example-linux.worker".to_string(),
                harness: "codex".to_string(),
                state: Activity::Active,
                blocked_on: BlockedOn::None,
                input_buffer: InputBuffer::Unknown,
                ask: Ask::None,
                background_jobs: None,
                reason: None,
                exit: None,
                pty_session: None,
                incarnation: String::new(),
                seq: 0,
                since_ms: written_at_ms,
                written_at_ms,
                transitions: 0,
            })
            .unwrap()
        };

        let fresh = read_raw_at(&raw(now_ms - stale_ms + 1), None, now_ms);
        assert_eq!(fresh.state, Activity::Active);

        let stale = read_raw_at(&raw(now_ms - stale_ms), None, now_ms);
        assert_eq!(stale.state, Activity::Unknown);
        assert_eq!(stale.reason.as_deref(), Some("stale"));
        assert_eq!(stale.blocked_on, BlockedOn::Unknown);

        let bounded_future = read_raw_at(&raw(now_ms + skew_ms), None, now_ms);
        assert_eq!(bounded_future.state, Activity::Active);

        let excessive_future = read_raw_at(&raw(now_ms + skew_ms + 1), None, now_ms);
        assert_eq!(excessive_future.state, Activity::Unknown);
        assert_eq!(excessive_future.reason.as_deref(), Some("future-skew"));
    }

    #[test]
    fn malformed_record_is_unknown_without_mtime_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        for raw in [
            &b"garbage"[..],
            b"{}",
            b"{\"schema\":\"st2.harness-state.v1\"}",
        ] {
            fs::write(&path, raw).unwrap();
            let observed = read(&path, None).unwrap();
            assert_eq!(observed.state, Activity::Unknown);
            assert_eq!(observed.reason.as_deref(), Some("malformed-record"));
        }
    }

    #[test]
    fn a_dead_session_reads_unknown_even_while_fresh_but_ended_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());
        let dead: &dyn Fn(&str) -> SessionLiveness = &|_| SessionLiveness::Dead;
        let indeterminate: &dyn Fn(&str) -> SessionLiveness = &|_| SessionLiveness::Indeterminate;

        writer.observe(active()).unwrap();
        let observed = read(&path, Some(dead)).unwrap();
        assert_eq!(observed.state, Activity::Unknown);
        assert_eq!(observed.reason.as_deref(), Some("session-dead"));
        // An unreadable registry proves nothing and downgrades nothing.
        assert_eq!(
            read(&path, Some(indeterminate)).unwrap().state,
            Activity::Active
        );

        writer.ended("signal 9").unwrap();
        let observed = read(&path, Some(dead)).unwrap();
        assert_eq!(observed.state, Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }

    #[test]
    fn heartbeat_re_stamps_only_live_states_and_never_resurrects_ended() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());

        // Nothing observed yet: heartbeat is a no-op, not a default write.
        writer.heartbeat().unwrap();
        assert_eq!(read(&path, None), None);

        writer.ended("exit 0").unwrap();
        let terminal = fs::read(&path).unwrap();
        writer.heartbeat().unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            terminal,
            "ended is never re-stamped"
        );
    }

    #[test]
    fn future_vocabulary_degrades_to_indeterminate_not_none() {
        // A future schema gates interpretation entirely — even words spelled exactly like this
        // version's must not decode as anything definite, because v2 may have changed what the
        // same spelling means.
        let raw = br#"{"schema":"st2.harness-state.v2","agent":"example-linux.worker","harness":"codex","state":"active","blockedOn":"none","inputBuffer":"empty","sinceMs":1,"writtenAtMs":9999999999999,"transitions":3,"novelField":true}"#;
        let observed = read_raw_at(raw, None, 9_999_999_999_999);
        assert_eq!(observed.state, Activity::Unknown);
        assert_eq!(observed.reason.as_deref(), Some("unsupported-schema"));
        assert_eq!(observed.blocked_on, BlockedOn::Unknown);

        // And on a v1 record with a known state, unknown axis words stay indeterminate.
        let raw = br#"{"schema":"st2.harness-state.v1","agent":"example-linux.worker","harness":"codex","state":"active","blockedOn":"robot","inputBuffer":"overflowing","sinceMs":1,"writtenAtMs":9999999999999,"transitions":3}"#;
        let observed = read_raw_at(raw, None, 9_999_999_999_999);
        assert_eq!(observed.state, Activity::Active);
        assert_eq!(observed.blocked_on, BlockedOn::Unknown);
        assert_eq!(observed.input_buffer, InputBuffer::Unknown);
    }

    #[test]
    fn interrupt_forces_a_fresh_transition_even_for_a_restated_fresh_tuple() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());

        writer.observe(active()).unwrap();
        let entered: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        // Evidence lost and returned: the restated tuple must not claim continuity, overriding
        // both the coalesce branch and the fresh-restatement no-op guard.
        writer.interrupt();
        writer.observe(active()).unwrap();
        let resumed: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(resumed.transitions, entered.transitions + 1);
        assert!(resumed.since_ms >= entered.since_ms);

        // The flag clears on the write: the next restatement coalesces again.
        let bytes = fs::read(&path).unwrap();
        writer.observe(active()).unwrap();
        assert_eq!(bytes, fs::read(&path).unwrap());
    }

    /// HC-R15: the written claim that supersedes this record also removes the numeric sibling, so
    /// a new incarnation reads "no context yet" rather than the previous incarnation's fill. The
    /// wrapperless claim path shares the same body and therefore the same behaviour.
    #[test]
    fn the_relaunch_claim_removes_the_harness_context_record() {
        use crate::harness_context::{self, Harness, Reading, harness_context_path};

        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agents").join("example-linux").join("worker");
        fs::create_dir_all(&agent_dir).unwrap();
        let context_path = harness_context_path(&agent_dir);

        let fill = || Reading {
            used_tokens: Some(190_000),
            window_tokens: Some(200_000),
            used_percent: Some(95.0),
            ..Reading::default()
        };
        harness_context::Writer::new(&agent_dir, "example-linux.worker", Harness::Claude)
            .unwrap()
            .observe(fill())
            .unwrap();
        assert!(harness_context::read(&context_path).is_some());

        // A wrapper relaunch: claim the state record, and the sibling goes with it.
        let token = session_token();
        claim(&agent_dir, "example-linux.worker", "claude", &token).unwrap();
        assert!(
            harness_context::read(&context_path).is_none(),
            "the new incarnation must read `no context yet`"
        );
        assert!(!context_path.exists());
        // The state record's own claim placeholder is untouched by the removal: it still reads
        // indeterminate-because-`claimed`, the fence the claim just wrote.
        assert_eq!(
            read(&harness_state_path(&agent_dir), None)
                .unwrap()
                .reason
                .as_deref(),
            Some("claimed")
        );

        // Claiming a seat that never had a context record is not an error.
        claim(&agent_dir, "example-linux.worker", "claude", &session_token()).unwrap();

        // …and the wrapperless boundary, which routes through the same body. It is eligible only
        // over a seat no wrapper holds, so it gets its own.
        let hooks_dir = tmp.path().join("agents").join("example-linux").join("hooked");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hooks_context = harness_context_path(&hooks_dir);
        harness_context::Writer::new(&hooks_dir, "example-linux.hooked", Harness::Claude)
            .unwrap()
            .observe(fill())
            .unwrap();
        let wrapperless = format!("{WRAPPERLESS_PREFIX}abc");
        assert!(
            claim_wrapperless(&hooks_dir, "example-linux.hooked", "claude", &wrapperless)
                .unwrap()
                .is_some(),
            "the claim must actually have happened"
        );
        assert!(harness_context::read(&hooks_context).is_none());
    }

    #[test]
    fn a_predecessor_sessions_record_is_never_heartbeat_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let predecessor = Record {
            schema: SCHEMA.to_string(),
            agent: "example-linux.worker".to_string(),
            harness: "codex".to_string(),
            state: Activity::Active,
            blocked_on: BlockedOn::None,
            input_buffer: InputBuffer::Unknown,
            ask: Ask::None,
            background_jobs: None,
            reason: None,
            exit: None,
            pty_session: Some("worker".to_string()),
            incarnation: String::new(),
            seq: 0,
            since_ms: 5,
            written_at_ms: 5,
            transitions: 3,
        };
        write_record(&path, &predecessor).unwrap();
        let stale_bytes = fs::read(&path).unwrap();

        // A restarted wrapper must not keep a dead session's state fresh forever.
        let mut writer = takeover(tmp.path(), "codex");
        writer.heartbeat().unwrap();
        assert_ne!(
            fs::read(&path).unwrap(),
            stale_bytes,
            "the written claim itself supersedes the predecessor"
        );
        assert_eq!(
            read(&path, None).unwrap().reason.as_deref(),
            Some("claimed"),
            "the fresh placeholder reads indeterminate, never definite ended"
        );

        // Once this session observes something, heartbeats re-stamp again.
        writer.observe(active()).unwrap();
        let observed_bytes = fs::read(&path).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        writer.heartbeat().unwrap();
        assert_ne!(fs::read(&path).unwrap(), observed_bytes);
    }

    #[test]
    fn an_unreadable_record_is_indeterminate_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        fs::create_dir(&path).unwrap();
        let observed = read(&path, None).unwrap();
        assert_eq!(observed.state, Activity::Unknown);
        assert_eq!(observed.reason.as_deref(), Some("unreadable-record"));
    }

    #[test]
    fn observe_unless_ended_drops_live_frames_after_a_peer_terminal_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        // The channel and wrapper are sibling processes of ONE session and share its token —
        // that sharing is what makes the wrapper's terminal record the session's last word.
        let token = session_token();
        let mut channel = Writer::new(tmp.path(), "example-linux.worker", "pi", Some("worker".into()))
            .with_session(token.clone());
        let mut wrapper =
            Writer::new(tmp.path(), "example-linux.worker", "pi", Some("worker".into())).with_session(token);

        assert!(channel.observe_unless_ended(active()).unwrap());
        wrapper.ended("signal 9").unwrap();
        let terminal = fs::read(&path).unwrap();

        // A queued live frame arriving after the terminal record must not resurrect the session.
        assert!(
            !channel
                .observe_unless_ended(Observation::new(
                    Activity::Idle,
                    BlockedOn::None,
                    InputBuffer::Unknown,
                ))
                .unwrap()
        );
        assert_eq!(fs::read(&path).unwrap(), terminal);
    }

    #[test]
    fn a_new_sessions_first_observation_writes_through_a_matching_fresh_predecessor() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut predecessor = writer(tmp.path());
        predecessor.observe(active()).unwrap();
        let before: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        // The successor is a different incarnation — token inequality alone forces the
        // write-through, even inside the same millisecond. Without it, its matching first
        // observation would be a no-op and the ownership gate would then reject every heartbeat
        // while the record quietly aged out.
        let mut successor = takeover(tmp.path(), "codex");
        successor.observe(active()).unwrap();
        let claimed: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(
            claimed.written_at_ms >= before.written_at_ms,
            "takeover must write"
        );

        std::thread::sleep(Duration::from_millis(2));
        successor.heartbeat().unwrap();
        let stamped: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(
            stamped.written_at_ms > claimed.written_at_ms,
            "heartbeat must be eligible after the takeover write"
        );
    }

    #[test]
    fn a_session_writer_marked_interrupted_opens_a_fresh_transition_on_takeover() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        writer(tmp.path()).observe(active()).unwrap();
        let before: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        let mut successor = takeover(tmp.path(), "codex");
        successor.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            record.transitions,
            before.transitions + 2,
            "the written claim and the first observation each transition"
        );
        assert!(
            record.since_ms > before.since_ms,
            "sinceMs must never span a session boundary"
        );
    }

    #[test]
    fn foreign_schemas_are_never_coalesced_restamped_or_treated_as_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let foreign = br#"{"schema":"st2.harness-state.v2","agent":"example-linux.worker","harness":"codex","state":"ended","blockedOn":"none","inputBuffer":"unknown","sinceMs":5,"writtenAtMs":99999999999999,"transitions":7,"novel":true}"#;
        fs::write(&path, foreign).unwrap();

        // Heartbeat leaves a foreign record byte-identical rather than stripping its fields —
        // and a token-only writer cannot replace it either: supersession is a claim's job.
        let mut unclaimed = writer(tmp.path());
        unclaimed.heartbeat().unwrap();
        assert_eq!(fs::read(&path).unwrap(), foreign.to_vec());
        assert!(!unclaimed.observe_unless_ended(active()).unwrap());
        assert_eq!(fs::read(&path).unwrap(), foreign.to_vec());

        // A claiming session replaces it wholesale, continuing the counter for byte-distinctness.
        let mut writer = takeover(tmp.path(), "codex");
        assert!(writer.observe_unless_ended(active()).unwrap());
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.schema, SCHEMA);
        assert_eq!(record.state, Activity::Active);
        assert_eq!(record.transitions, 9);
    }

    #[test]
    fn a_predecessor_terminal_record_does_not_suppress_a_new_sessions_live_frames() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        writer(tmp.path()).ended("exit 0").unwrap();
        let terminal: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        // Deliberately no delay: a same-millisecond takeover is the ambiguous case a timestamp
        // boundary got wrong, and token inequality decides it.
        let _ = terminal;
        let mut successor = takeover(tmp.path(), "pi");
        assert!(
            successor.observe_unless_ended(active()).unwrap(),
            "a restarted seat must replace its predecessor's terminal record"
        );
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.state, Activity::Active);
    }

    #[test]
    fn live_observations_require_a_pty_session_and_unfenced_live_records_read_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut unfenced = Writer::new(tmp.path(), "example-linux.worker", "codex", None);
        assert!(
            unfenced.observe(active()).is_err(),
            "live states need a fence"
        );
        unfenced.ended("exit 0").unwrap();

        // A live record that names no session offers the probe nothing to check: with a probe
        // available it is indeterminate, while the terminal record stays definite.
        let alive: &dyn Fn(&str) -> SessionLiveness = &|_| SessionLiveness::Alive;
        assert_eq!(read(&path, Some(alive)).unwrap().state, Activity::Ended);
        let live_unfenced = format!(
            r#"{{"schema":"st2.harness-state.v1","agent":"example-linux.worker","harness":"codex","state":"active","blockedOn":"none","inputBuffer":"unknown","sinceMs":1,"writtenAtMs":{},"transitions":1}}"#,
            crate::message::now_ms()
        );
        fs::write(&path, live_unfenced).unwrap();
        let observed = read(&path, Some(alive)).unwrap();
        assert_eq!(observed.state, Activity::Unknown);
        assert_eq!(observed.reason.as_deref(), Some("unfenced-record"));
        // Without a probe (a cross-host reader) the record keeps its staleness-only semantics.
        assert_eq!(read(&path, None).unwrap().state, Activity::Active);
    }

    #[test]
    fn ask_kind_roundtrips_and_unknown_words_decode_indeterminate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());
        writer
            .observe(
                Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
                    .with_ask(Ask::Question)
                    .with_reason("question"),
            )
            .unwrap();
        let observed = read(&path, None).unwrap();
        assert_eq!(observed.ask, Ask::Question);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"ask\":\"question\"")
        );

        // A record predating the axis defaults to `none`; a future word decodes indeterminate.
        let raw: String = fs::read_to_string(&path)
            .unwrap()
            .replace("\"ask\":\"question\",", "");
        fs::write(&path, &raw).unwrap();
        assert_eq!(read(&path, None).unwrap().ask, Ask::None);
        fs::write(
            &path,
            raw.replace(
                "\"blockedOn\":\"human\"",
                "\"blockedOn\":\"human\",\"ask\":\"telepathy\"",
            ),
        )
        .unwrap();
        assert_eq!(read(&path, None).unwrap().ask, Ask::Unknown);
    }

    #[test]
    fn a_lingering_predecessor_cannot_heartbeat_its_successors_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut old = writer(tmp.path());
        old.observe(active()).unwrap();

        let mut successor = takeover(tmp.path(), "codex");
        successor
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        let bytes = fs::read(&path).unwrap();

        // The old wrapper outlives its replacement briefly; its heartbeat must not keep the
        // successor's record fresh — the successor's death would otherwise stay invisible to
        // cross-host readers for as long as the straggler lives.
        old.heartbeat().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn landed_heartbeats_are_byte_distinct_and_strictly_monotonic_even_same_millisecond() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());
        writer.observe(active()).unwrap();

        let mut previous: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for _ in 0..5 {
            let before_bytes = fs::read(&path).unwrap();
            writer.heartbeat().unwrap();
            let after_bytes = fs::read(&path).unwrap();
            assert_ne!(
                after_bytes, before_bytes,
                "every landed heartbeat re-stamps bytes"
            );
            let current: Record = serde_json::from_slice(&after_bytes).unwrap();
            assert!(
                current.written_at_ms > previous.written_at_ms,
                "stamps are strictly monotonic per record"
            );
            previous = current;
        }
    }

    #[test]
    fn sibling_writers_sharing_a_session_token_coalesce_and_heartbeat_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let token = session_token();
        let mut wrapper = Writer::new(tmp.path(), "example-linux.worker", "claude", Some("worker".into()))
            .with_session(token.clone());
        let mut hook = Writer::new(tmp.path(), "example-linux.worker", "claude", Some("worker".into()))
            .with_session(token);

        hook.observe(active()).unwrap();
        let entered: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        // A sibling's restatement coalesces (no transition churn across hook processes)…
        let mut hook2 = Writer::new(tmp.path(), "example-linux.worker", "claude", Some("worker".into()))
            .with_session(entered.incarnation.clone());
        hook2.observe(active()).unwrap();
        let restated: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(restated.transitions, entered.transitions);
        assert_eq!(restated.since_ms, entered.since_ms);

        // …and the wrapper's heartbeat re-stamps the sibling-written state.
        wrapper.heartbeat().unwrap();
        let stamped: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(stamped.written_at_ms > restated.written_at_ms);
        assert_eq!(stamped.state, Activity::Active);
    }

    /// Cluster A: ownership has a direction. A successor's claim replaces the predecessor's
    /// record, and the predecessor's late writes — live AND terminal — are refused, not treated
    /// as a fresh takeover.
    #[test]
    fn a_lingering_predecessors_late_writes_are_refused_after_the_successors_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut predecessor = writer(tmp.path());
        predecessor.observe(active()).unwrap();

        let mut successor = takeover(tmp.path(), "codex");
        successor
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        let claimed = fs::read(&path).unwrap();

        // The straggler's live frame, its queued terminal write, and its unless-ended variant
        // all land nothing.
        predecessor.observe(active()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), claimed);
        predecessor.ended("signal 9").unwrap();
        assert_eq!(fs::read(&path).unwrap(), claimed);
        assert!(!predecessor.observe_unless_ended(active()).unwrap());
        assert_eq!(fs::read(&path).unwrap(), claimed);
        assert_eq!(read(&path, None).unwrap().state, Activity::Idle);
    }

    /// Cluster A: adopted ownership (the env-exported token+seq) writes while the disk is at or
    /// below its claim — including performing the session's first write — and is refused once a
    /// later session claims past it.
    #[test]
    fn adopted_ownership_writes_up_to_its_claim_and_is_refused_beyond_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        writer(tmp.path()).observe(active()).unwrap();

        // The wrapper writes the claim and exports it; the hook adopts the pair.
        let token = session_token();
        let seq = claim(tmp.path(), "example-linux.worker", "claude", &token).unwrap();
        let mut hook = Writer::new(tmp.path(), "example-linux.worker", "claude", Some("worker".into()))
            .with_ownership(token.clone(), seq);
        hook.observe(Observation::new(
            Activity::Idle,
            BlockedOn::None,
            InputBuffer::Unknown,
        ))
        .unwrap();
        assert_eq!(read(&path, None).unwrap().state, Activity::Idle);

        // A later session claims past it; the adopted writer becomes the straggler.
        let mut next = Writer::new(tmp.path(), "example-linux.worker", "claude", Some("worker".into()));
        next.observe(active()).unwrap();
        let after = fs::read(&path).unwrap();
        hook.observe(Observation::new(
            Activity::Idle,
            BlockedOn::None,
            InputBuffer::Unknown,
        ))
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), after);
    }

    /// A3: a stamp beyond the future-skew bound — garbage or an overflow probe — is never
    /// inherited; the writer's own clock wins and nothing overflows.
    #[test]
    fn untrusted_future_stamps_are_reset_not_inherited() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let poisoned = format!(
            r#"{{"schema":"st2.harness-state.v1","agent":"example-linux.worker","harness":"codex","state":"active","blockedOn":"none","inputBuffer":"unknown","ptySession":"worker","incarnation":"other","seq":3,"sinceMs":1,"writtenAtMs":{},"transitions":1}}"#,
            u64::MAX
        );
        fs::write(&path, poisoned).unwrap();

        let mut writer = takeover(tmp.path(), "codex");
        writer.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let now = crate::message::now_ms();
        assert!(record.written_at_ms <= now.saturating_add(duration_ms(HARNESS_STATE_FUTURE_SKEW)));
        assert_eq!(
            record.seq, 4,
            "the claim still advances past the poisoned record"
        );
        assert_eq!(read(&path, None).unwrap().state, Activity::Active);
    }

    /// T2: the claim is a WRITTEN act under the record lock — racing claimers mint DISTINCT
    /// sequences instead of tying, which dissolved the former dual-claim residual.
    #[test]
    fn racing_claims_serialize_on_the_lock_and_mint_distinct_sequences() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    claim(&dir, "example-linux.worker", "codex", &session_token()).unwrap()
                })
            })
            .collect();
        let mut seqs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 4, "every racing claim minted its own sequence");
    }

    /// T3: at relaunch the claim supersedes the predecessor's still-fresh live record — the
    /// pty-name-based probe cannot tell the sessions apart, so the record itself must — and the
    /// seat reads indeterminate (`claimed`) until the new session's first real observation.
    #[test]
    fn a_relaunch_claim_supersedes_a_fresh_live_predecessor_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        writer(tmp.path()).observe(active()).unwrap();

        let token = session_token();
        let seq = claim(tmp.path(), "example-linux.worker", "codex", &token).unwrap();
        let observed = read(&path, None).unwrap();
        assert_eq!(
            observed.state,
            Activity::Unknown,
            "the fresh placeholder is a fence, not a definite ended"
        );
        assert_eq!(observed.reason.as_deref(), Some("claimed"));
        assert_eq!(observed.exit, None);

        let mut successor = Writer::new(tmp.path(), "example-linux.worker", "codex", Some("worker".into()))
            .with_ownership(token, seq);
        successor.observe(active()).unwrap();
        assert_eq!(read(&path, None).unwrap().state, Activity::Active);
    }

    /// W8-6: a v2 record's serde-default sequence of zero is below every claim — but a v1
    /// straggler must not replace a record it cannot read. Only the written claim supersedes.
    #[test]
    fn non_claiming_writers_refuse_foreign_schemas_outright() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let v2 = br#"{"schema":"st2.harness-state.v2","agent":"example-linux.worker","harness":"codex","state":"active","blockedOn":"none","inputBuffer":"unknown","incarnation":"future","sinceMs":1,"writtenAtMs":1,"transitions":1}"#;
        fs::write(&path, v2).unwrap();

        let token = session_token();
        let mut adopted = Writer::new(tmp.path(), "example-linux.worker", "codex", Some("worker".into()))
            .with_ownership(token, 5);
        adopted.observe(active()).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            v2.to_vec(),
            "adopted writer refused"
        );
        let mut token_only = writer(tmp.path());
        token_only.observe(active()).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            v2.to_vec(),
            "token-only writer refused"
        );

        let mut claimed = takeover(tmp.path(), "codex");
        claimed.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.schema, SCHEMA, "only the written claim supersedes");
    }

    /// W8-7: a beyond-skew future stamp (a backward clock correction's leftover) must not make
    /// an unchanged restatement a no-op forever — the next restatement repairs the stamp.
    #[test]
    fn a_beyond_skew_stamp_is_repaired_by_the_next_restatement() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut writer = writer(tmp.path());
        writer.observe(active()).unwrap();

        let mut poisoned: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        poisoned.written_at_ms = crate::message::now_ms()
            + duration_ms(HARNESS_STATE_FUTURE_SKEW)
            + duration_ms(HARNESS_STATE_REFRESH);
        write_record(&path, &poisoned).unwrap();
        assert_eq!(
            read(&path, None).unwrap().reason.as_deref(),
            Some("future-skew")
        );

        writer.observe(active()).unwrap();
        let repaired = read(&path, None).unwrap();
        assert_eq!(
            repaired.state,
            Activity::Active,
            "stamp repaired to the writer's clock"
        );
    }

    /// W8-8: the ask axis is validated at the write boundary.
    #[test]
    fn wrapperless_claims_are_atomic_and_never_supersede_a_live_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let wl =
            |token: &str| claim_wrapperless(tmp.path(), "example-linux.worker", "claude", token).unwrap();
        assert!(wl("claude-session-a").is_some(), "virgin dir");

        // A wrapper's FRESH claim placeholder is a session mid-startup, not an ended one: the
        // check-and-write is one act under the lock, so the racing hooks-only SessionStart
        // cannot steal the sequence between the wrapper's read and its write.
        let wrapper_token = session_token();
        let wrapper_seq = claim(tmp.path(), "example-linux.worker", "claude", &wrapper_token).unwrap();
        assert!(
            wl("claude-session-b").is_none(),
            "fresh placeholder is owned"
        );

        // A live wrapper record stays off limits; a REAL terminal record is claimable.
        let mut wrapper = Writer::new(
            tmp.path(),
            "example-linux.worker",
            "claude",
            Some("worker".to_string()),
        )
        .with_ownership(wrapper_token.clone(), wrapper_seq);
        wrapper.observe(active()).unwrap();
        assert!(
            wl("claude-session-c").is_none(),
            "live wrapper record is off limits"
        );
        wrapper.ended("exit 0").unwrap();
        assert!(
            wl("claude-session-d").is_some(),
            "real terminal records may be claimed"
        );
    }

    /// An abandoned placeholder — a wrapper that claimed and then died before observing —
    /// ages past the staleness horizon and becomes claimable like any orphan.
    #[test]
    fn an_abandoned_wrapper_placeholder_is_claimable_once_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        claim(tmp.path(), "example-linux.worker", "claude", &session_token()).unwrap();
        assert!(
            claim_wrapperless(tmp.path(), "example-linux.worker", "claude", "claude-session-x")
                .unwrap()
                .is_none()
        );

        let mut aged: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        aged.written_at_ms = crate::message::now_ms() - duration_ms(HARNESS_STATE_STALE) - 1;
        write_record(&path, &aged).unwrap();
        assert!(
            claim_wrapperless(tmp.path(), "example-linux.worker", "claude", "claude-session-x")
                .unwrap()
                .is_some()
        );
    }

    /// n2: a saturated on-disk sequence would mint shared ownership; the claim fails loudly
    /// instead, and unreadable bytes are never a virgin seat for non-claiming writers.
    #[test]
    fn saturated_sequences_refuse_claims_and_unreadable_bytes_refuse_writers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let saturated = format!(
            r#"{{"schema":"st2.harness-state.v1","agent":"example-linux.worker","harness":"codex","state":"idle","blockedOn":"none","inputBuffer":"unknown","ptySession":"worker","incarnation":"other","seq":{},"sinceMs":1,"writtenAtMs":1,"transitions":1}}"#,
            u64::MAX
        );
        fs::write(&path, saturated).unwrap();
        assert!(claim(tmp.path(), "example-linux.worker", "codex", "t").is_err());

        fs::write(&path, b"{not json").unwrap();
        let before = fs::read(&path).unwrap();
        let mut writer = writer(tmp.path());
        writer.observe(active()).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "non-claiming writers refuse"
        );
        let mut adopted = Writer::new(tmp.path(), "example-linux.worker", "codex", Some("worker".into()))
            .with_ownership(session_token(), 7);
        adopted.observe(active()).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "adopted writers refuse too"
        );

        // The written claim supersedes even bytes it cannot parse; sequence and counter restart.
        let token = session_token();
        let seq = claim(tmp.path(), "example-linux.worker", "codex", &token).unwrap();
        assert_eq!(seq, 1);
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.transitions, 0);
    }

    /// jUUo: the sequence floor survives a record this version cannot parse — a claim after
    /// damage continues past the damaged sequence instead of restarting below a lingering
    /// predecessor, who stays fenced out.
    #[test]
    fn the_sequence_floor_keeps_claims_monotonic_across_unreadable_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut predecessor = takeover(tmp.path(), "codex");
        predecessor.observe(active()).unwrap();
        let damaged_seq: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        fs::write(&path, b"{corrupted").unwrap();
        let token = session_token();
        let seq = claim(tmp.path(), "example-linux.worker", "codex", &token).unwrap();
        assert!(
            seq > damaged_seq.seq,
            "the floor carries the sequence past the damage ({seq} vs {})",
            damaged_seq.seq
        );

        // The lingering predecessor is below the new claim and stays refused.
        predecessor.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.incarnation, token);
        assert_eq!(record.reason.as_deref(), Some("superseded"));
    }

    /// The floor is staged under a per-writer name, created exclusively, so a path an agent
    /// planted is refused instead of followed and two writers cannot tear each other's bytes.
    ///
    /// Both are the same defect: the staging name used to be the fixed literal
    /// `.harness-state.seq.tmp` written with a truncating, symlink-following `fs::write`. A
    /// symlink there aimed st2's write at a file of the agent's choosing, and two writers for one
    /// agent could rename each other's half-written floor into place — a torn floor, which is
    /// exactly the failure the floor exists to survive.
    #[test]
    fn the_sequence_floor_refuses_a_planted_staging_path() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agents/example-linux/worker");
        fs::create_dir_all(&agent_dir).unwrap();
        let victim = tmp.path().join("authored");
        fs::write(&victim, b"authored bytes").unwrap();
        symlink(&victim, agent_dir.join(".harness-state.seq.tmp")).unwrap();

        persist_floor(&harness_state_path(&agent_dir), 7);

        let floor = agent_dir.join(SEQ_FLOOR_NAME);
        assert_eq!(
            fs::read_to_string(&floor).unwrap().trim(),
            "7",
            "the floor landed despite the planted staging path"
        );
        assert_eq!(
            fs::metadata(&floor).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"authored bytes",
            "the planted symlink was followed and its target was truncated"
        );
    }

    /// The virgin token-only path also establishes initial ownership (sequence one), so it
    /// persists the floor sidecar too: if that first record later goes unreadable, a
    /// replacement claim continues PAST the lingering writer instead of colliding with it.
    #[test]
    fn a_virgin_token_only_write_persists_the_sequence_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        let mut predecessor = writer(tmp.path());
        predecessor.observe(active()).unwrap();
        let record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.seq, 1);
        let floor = fs::read_to_string(tmp.path().join(SEQ_FLOOR_NAME)).unwrap();
        assert_eq!(
            floor.trim(),
            "1",
            "the virgin write persists the floor beside its sequence"
        );

        // The record goes unreadable; the claim must continue past sequence one, and the
        // lingering token-only predecessor stays fenced out.
        fs::write(&path, b"{corrupted").unwrap();
        let token = session_token();
        let seq = claim(tmp.path(), "example-linux.worker", "codex", &token).unwrap();
        assert!(
            seq > 1,
            "the persisted floor carries the claim past sequence one ({seq})"
        );
        predecessor.observe(active()).unwrap();
        let after: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after.incarnation, token);
    }

    /// jUUq: an unreadable (not merely absent) record refuses token-only writes and wrapperless
    /// claims — a permissions failure is never a virgin seat.
    #[test]
    fn io_failures_are_unreadable_not_absent() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_state_path(tmp.path());
        writer(tmp.path()).observe(active()).unwrap();
        let live = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let mut token_only = writer(tmp.path());
        token_only
            .observe(Observation::new(
                Activity::Idle,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        assert!(
            claim_wrapperless(tmp.path(), "example-linux.worker", "claude", "claude-session-x")
                .unwrap()
                .is_none(),
            "wrapperless claims refuse unreadable records"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            live,
            "nothing renamed over the live record"
        );
    }

    /// Both driver records and `context`'s `now.md` share this one acquisition, so this is the
    /// only place the newly hardened `O_NOFOLLOW` needs pinning. None of the three had a lock
    /// test before.
    #[test]
    fn a_symlinked_record_lock_is_refused_instead_of_locking_its_target() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::write(&outside, "unchanged").unwrap();
        let lock_path = tmp.path().join("agent/.harness-state.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &lock_path).unwrap();

        let error = lock_exclusive(&lock_path).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(libc::ELOOP),
            "a symlinked record lock must be refused, got {error:#}"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged");

        let fresh = tmp.path().join("agent/.harness-context.lock");
        let held = lock_exclusive(&fresh).unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600,
            "a freshly created record lock must be private"
        );
        // `flock` locks the open file description, so a second open in this process contends.
        let probe = || {
            let file = crate::flock::open(&fresh, crate::flock::Open::Existing).unwrap();
            crate::flock::FileLock::hold(
                file,
                crate::flock::Mode::Exclusive,
                crate::flock::Wait::Now,
            )
            .unwrap()
            .is_some()
        };
        assert!(
            !probe(),
            "a live record-lock holder must exclude a second writer"
        );
        drop(held);
        assert!(probe(), "dropping the guard must release the record lock");
    }
}

#[cfg(test)]
mod background_job_tests {
    use super::*;

    #[test]
    fn background_job_count_changes_survive_the_owned_record_and_heartbeat() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(root.path(), "example.worker", "omp", Some("fixture".into()));
        for jobs in [Some(2), Some(0), None] {
            let mut observation = Observation::new(Activity::Idle, BlockedOn::None, InputBuffer::Empty);
            observation.background_jobs = jobs;
            writer.observe(observation).unwrap();
            writer.heartbeat().unwrap();
            assert_eq!(read(&harness_state_path(root.path()), None).unwrap().background_jobs, jobs);
        }
    }
}
