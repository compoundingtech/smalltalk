//! Harness context: the driver-owned numeric record of how full an agent's context window is.
//!
//! A `harness-context` file sits beside `harness-state` at the agent-directory root and carries
//! the fill triple (`usedTokens`, `windowTokens`, `usedPercent`), the compaction triple
//! (`compactions`, `lastCompactionMs`, `lastCompactionTrigger`), and the adjacent facts the same
//! producer channel supplies for free (`model`, `costUsd`, `rateLimits`, `sessionTotalTokens`).
//! It is the *numeric* axis; [`crate::harness_state`] owns the *categorical* one and reads none of
//! this. Both are independent of declared presence.
//!
//! It borrows that module's transport — a cross-process lock, stage-and-rename, an embedded origin
//! timestamp never file mtime, byte-distinct content on every landed write — and deliberately does
//! **not** borrow its ownership machinery: no claim sequence, no written claim, no floor sidecar,
//! no terminal record, no derived `unknown`. Those exist because a *state* record can lie in a
//! dangerous direction — a straggler resurrecting `active` makes a reader believe a dead seat is
//! working. A straggler writing a stale token count says only "this number is older than you
//! think", which the record's own timestamp already says. `incarnation` is carried as PROVENANCE
//! and never consulted as a fence (HC-T04, HC-R15).
//!
//! Two properties follow from the axis rather than from the transport:
//!
//! - **A stale reading is returned, not derived away** (HC-R06). There is no `unknown` vocabulary
//!   here: past the horizon a reader gets the numbers it found, marked `stale`, with their age.
//!   "190k of 200k, twelve minutes ago" is useful; "active, twelve minutes ago" is not, which is
//!   why the sibling record derives and this one does not.
//! - **The write guard is quantization, not equality** (HC-R09, HC-R10). A reading lands when it
//!   enters a different 1% bucket of the window, crosses Claude account-window exhaustion, when
//!   a compaction happens, or when the record is older than the heartbeat. Writes per window fill
//!   are therefore capped at
//!   `100 / HARNESS_CONTEXT_BUCKET_PERCENT` however chatty the producer is, which is what lets one
//!   constant serve all five harnesses; the bounded exhaustion and reset edges bypass that cap.
//!
//! The only st2 classification derived from these numbers is the positive fresh Claude rate-limit
//! signal used by roster consumers; the raw readings remain advisory for a human, a roster, and
//! Doctor.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::harness_state::{lock_exclusive, write_json_atomic};

const SCHEMA: &str = "st2.harness-context.v1";
const LOCK_NAME: &str = ".harness-context.lock";
const TMP_PREFIX: &str = ".harness-context";
const STAGING_DIR_NAME: &str = "harness-context-staging";

/// The record's file name inside an agent directory.
pub const RECORD_NAME: &str = "harness-context";

/// The exact driver-record names st2 publishes for the replication transport to carry, and the
/// only names its readers derive their paths from.
///
/// The transport's include list lives in the fleet's own configuration, not in this repository
/// (HC-T08), so a rename here would otherwise stop replication in silence: no error, just a
/// record no remote reader ever sees. Pinning the names is st2's half of that agreement — it does
/// not prove the other side carries them, it makes st2's half explicit and breakable.
pub const REPLICATED_DRIVER_RECORDS: [&str; 2] = [crate::harness_state::RECORD_NAME, RECORD_NAME];

/// Past this age a reading is returned marked `stale`, with its age (HC-R06). Deliberately its own
/// constant and four times [`crate::harness_state::HARNESS_STATE_STALE`]: a categorical state an
/// hour old is a dangerous claim about what an agent is doing right now, while a token count an
/// hour old is a still-useful lower bound on how full a window has become.
pub const HARNESS_CONTEXT_STALE: Duration = Duration::from_secs(60 * 60);
/// Beyond this the writer's clock is untrusted and the record reads absent: a derived age
/// computed against it would be meaningless.
pub const HARNESS_CONTEXT_FUTURE_SKEW: Duration = Duration::from_secs(60);
/// The window fraction, in percent, that defines a write bucket (HC-R09). One constant for all
/// five harnesses: quantization ties write cost to information rather than to producer
/// chattiness, so no per-harness tuning exists or is needed.
pub const HARNESS_CONTEXT_BUCKET_PERCENT: f64 = 1.0;
/// At or above this reading Doctor emits an advisory (HC-R17). st2's own "worth a human's
/// attention" number, explicitly **not** a prediction of where a harness will compact — that
/// point is harness-, model-, and setting-specific.
pub const HARNESS_CONTEXT_WARN_PERCENT: f64 = 80.0;
/// The maximum interval between writes *while a reading is available* (HC-R09). Equal to
/// [`crate::harness_state::HARNESS_STATE_REFRESH`] on purpose, so this record never re-stamps more
/// often than the state record beside it. A producer holding no fresh reading writes nothing and
/// the record ages visibly instead.
pub const HARNESS_CONTEXT_HEARTBEAT: Duration = crate::harness_state::HARNESS_STATE_REFRESH;

/// The record file: `<agent_dir>/harness-context`.
pub fn harness_context_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(RECORD_NAME)
}

/// Which harness's arithmetic produced the numbers — the discriminator, and deliberately the only
/// one: a reader that knows the harness knows which producer rule made the number, so there is no
/// second `semantics` field. A word this version does not recognize makes the numbers
/// uninterpretable, so such a record reads as absent rather than as a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Harness {
    Claude,
    Codex,
    Pi,
    Omp,
    #[serde(rename = "opencode")]
    OpenCode,
    /// Decodes a future producer's word. Never written, and a record carrying one reads absent.
    #[serde(other)]
    Unrecognized,
}

impl Harness {
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Omp => "omp",
            Harness::OpenCode => "opencode",
            Harness::Unrecognized => "unrecognized",
        }
    }
}

/// What caused the last compaction, over a closed vocabulary (HC-R12). Unlike the state record's
/// `unknown`, this one is WRITABLE and legitimately common: three of the five harnesses publish a
/// compaction edge carrying no reason at all. `idle` is in v1 because omp's internal auto-compaction
/// names it; no v1 producer emits it, since omp does not project its reason onto the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionTrigger {
    Manual,
    Auto,
    Threshold,
    Overflow,
    Idle,
    /// Also the landing place for a future producer's word: additive-tolerant on read, so an
    /// unrecognized trigger decodes as `unknown` and never as a definite one.
    #[serde(other)]
    Unknown,
}

impl CompactionTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionTrigger::Manual => "manual",
            CompactionTrigger::Auto => "auto",
            CompactionTrigger::Threshold => "threshold",
            CompactionTrigger::Overflow => "overflow",
            CompactionTrigger::Idle => "idle",
            CompactionTrigger::Unknown => "unknown",
        }
    }
}

/// Harness-reported, account-scoped rate-limit occupancy, as percentages the harness itself
/// published. Account-scoped means it repeats across every runtime sharing an account; nothing
/// here reconciles it with the fleet's quota authority. Absent windows are `null`, never zero.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimits {
    #[serde(default)]
    pub five_hour: Option<f64>,
    #[serde(default)]
    pub seven_day: Option<f64>,
}

impl RateLimits {
    /// Whether any reported account window reached 100%. Absent windows remain unknown, and the
    /// percentage alone does not classify provider availability.
    fn is_exhausted(&self) -> bool {
        [self.five_hour, self.seven_day]
            .into_iter()
            .flatten()
            .any(|percent| percent.is_finite() && percent >= 100.0)
    }
}

/// Whether the retained account-window evidence proves that this harness cannot progress. Codex
/// can continue through credits after its included allowance is exhausted, and the context record
/// does not carry the credit metadata needed to classify that state.
fn proves_rate_limited(harness: Harness, rate_limits: RateLimits) -> bool {
    harness == Harness::Claude && rate_limits.is_exhausted()
}

/// The durable record. Additive-tolerant on read (no `deny_unknown_fields`): a reader pinned to an
/// older crate may be older than its writer, and an unknown future enum word decodes as
/// indeterminate rather than as a definite value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    schema: String,
    agent: String,
    harness: Harness,
    /// The fill triple. Each leg is independently optional and `null` when the harness withheld
    /// it (HC-R02, HC-R03): a producer that cannot obtain a window withholds the percent rather
    /// than dividing by a table, and no path substitutes zero, the previous reading, or an
    /// estimate for a harness-declared null.
    #[serde(default)]
    used_tokens: Option<u64>,
    #[serde(default)]
    window_tokens: Option<u64>,
    /// The number the harness itself displays, carried RAW and never clamped by a producer or a
    /// reader: occupancy above 100% of the window is observed in practice and is precisely the
    /// condition worth surfacing. Clamping is a display concern.
    #[serde(default)]
    used_percent: Option<f64>,
    /// Adjacent facts, each carried as what the harness reported and nothing more (HC-R16).
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cost_usd: Option<f64>,
    /// Cumulative lifetime spend for the session, and **never** occupancy — named for exactly that
    /// distinction. It is never the numerator of any percent.
    #[serde(default)]
    session_total_tokens: Option<u64>,
    /// Always emitted, its own windows `null` when absent — one absence convention on this
    /// record, not two (HC-R16). A record predating the field decodes through `default`.
    #[serde(default)]
    rate_limits: RateLimits,
    #[serde(default)]
    compactions: u64,
    #[serde(default)]
    last_compaction_ms: Option<u64>,
    #[serde(default)]
    last_compaction_trigger: Option<CompactionTrigger>,
    /// The writing session's token, provenance only (HC-R15). Nothing refuses a write on it and no
    /// sequence accompanies it; a reader may use it to tell "this number came from the session
    /// currently running" from "this number predates it", and that is its whole purpose.
    #[serde(default)]
    incarnation: String,
    /// When the READING was taken. Never re-stamped without a new reading behind it, so a record
    /// ages honestly rather than looking refreshed.
    observed_at_ms: u64,
    /// When the record was WRITTEN. Distinct from `observedAtMs` because the heartbeat clause of
    /// the write policy is about how old the *record* is, while `ageMs` is about how old the
    /// *reading* is; collapsing them would make a producer publishing a four-minute-old reading
    /// look immediately due for a heartbeat. Strictly monotonic per record, which is also what
    /// keeps every landed write byte-distinct for a transport that compares content (HC-R04).
    #[serde(default)]
    written_at_ms: u64,
}

/// One producer's reading of its harness's window, stated in that harness's own arithmetic
/// (HC-R02). Every field is independently optional: a producer states what its channel gave it and
/// withholds the rest.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reading {
    pub used_tokens: Option<u64>,
    pub window_tokens: Option<u64>,
    pub used_percent: Option<f64>,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub session_total_tokens: Option<u64>,
    pub rate_limits: RateLimits,
}

/// One compaction edge as a producer observed it (HC-R12). `count` is `None` where st2 does the
/// counting (the harness publishes an edge and nothing else, so the counter is incarnation-scoped)
/// and `Some` where the harness's own session store answers the question and the count is durable
/// across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    pub trigger: CompactionTrigger,
    pub count: Option<u64>,
}

impl Compaction {
    pub fn new(trigger: CompactionTrigger) -> Self {
        Self {
            trigger,
            count: None,
        }
    }

    /// A harness-durable count read from the harness's own session store.
    pub fn with_count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }
}

/// Return the dedicated control-plane directory where harness-context writes may stage (HC-R05).
///
/// The writer is handed an agent directory, not a catalog root, so it derives the root only from
/// the exact canonical `<catalog>/agents/<host>/<identity>` shape and validates every catalog
/// component as a real directory. This is intentionally not an upward search: a nearby unrelated
/// `.st2` must never capture a write. The staging directory lives below this catalog's `.st2`, out
/// of the identity namespace and the replicated `agents` subtree.
///
/// The device check makes the atomic-rename precondition explicit. A catalog with `.st2` mounted
/// separately is rejected rather than degrading publication to a copy or a non-atomic fallback.
fn staging_dir(agent_dir: &Path) -> anyhow::Result<PathBuf> {
    let host_dir = agent_dir
        .parent()
        .context("canonical agent directory has no host parent")?;
    let agents_dir = host_dir
        .parent()
        .context("canonical host directory has no agents parent")?;
    anyhow::ensure!(
        agents_dir.file_name() == Some(OsStr::new("agents")),
        "agent directory is not under a canonical agents path: {}",
        agent_dir.display()
    );
    let catalog = agents_dir
        .parent()
        .context("canonical agents directory has no catalog parent")?;
    for (path, label) in [
        (catalog, "catalog root"),
        (agents_dir, "canonical agents directory"),
        (host_dir, "canonical host directory"),
        (agent_dir, "canonical identity directory"),
    ] {
        ensure_real_directory(path, label)?;
    }
    let canonical_catalog = catalog
        .canonicalize()
        .with_context(|| format!("canonicalize catalog root {}", catalog.display()))?;
    let canonical_agent = agent_dir
        .canonicalize()
        .with_context(|| format!("canonicalize agent directory {}", agent_dir.display()))?;
    anyhow::ensure!(
        canonical_agent
            == canonical_catalog
                .join("agents")
                .join(host_dir.file_name().context("canonical host has no name")?)
                .join(
                    agent_dir
                        .file_name()
                        .context("canonical identity has no name")?,
                ),
        "agent directory ancestry is not canonical: {}",
        agent_dir.display()
    );

    let control = canonical_catalog.join(crate::catalog_lock::CONTROL_DIR);
    create_or_validate_directory(&control, "catalog control directory")?;
    let staging = control.join(STAGING_DIR_NAME);
    create_or_validate_directory(&staging, "harness-context staging directory")?;
    let agent_device = fs::metadata(&canonical_agent)?.dev();
    let staging_device = fs::metadata(&staging)?.dev();
    anyhow::ensure!(
        agent_device == staging_device,
        "harness-context staging directory is not on the agent filesystem: {}",
        staging.display()
    );
    Ok(staging)
}

fn ensure_real_directory(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} is not a real directory: {}",
        path.display()
    );
    Ok(())
}

fn create_or_validate_directory(path: &Path, label: &str) -> anyhow::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("create {label} {}", path.display()));
        }
    }
    ensure_real_directory(path, label)
}

/// Whether a host child is the exact regular-file shape emitted by legacy harness-context writers.
///
/// Current-catalog walkers may overlook only this one compatibility residue. Exact-name
/// directories, symlinks, and special files are errors; generic dotfiles and near misses remain
/// ordinary identity candidates and therefore fail the canonical topology checks.
pub(crate) fn is_legacy_harness_context_staging_file(entry: &fs::DirEntry) -> anyhow::Result<bool> {
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(false);
    };
    if !is_legacy_staging_name(name) {
        return Ok(false);
    }
    let path = entry.path();
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect legacy staging file {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "legacy harness-context staging path is not a real regular file: {}",
        path.display()
    );
    Ok(true)
}

/// The matcher and the writer must agree on the prefix, so both read [`TMP_PREFIX`]: this name is
/// the one INVARIANTS row 29 says current-catalog identity walkers overlook, and a writer that
/// drifted from this matcher would leave a staged file the walkers no longer recognize.
fn is_legacy_staging_name(name: &str) -> bool {
    let Some(suffix) = name
        .strip_prefix(TMP_PREFIX)
        .and_then(|rest| rest.strip_prefix(".tmp-"))
    else {
        return false;
    };
    let Some((pid, counter)) = suffix.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && !counter.is_empty()
        && counter.bytes().all(|byte| byte.is_ascii_digit())
}

/// The writer a driver process owns over one agent's context record. Several driver processes of
/// one session may hold writers over the same record — a wrapper beside its hook subprocesses — so
/// every operation takes the record's own cross-process lock and treats the on-disk record as the
/// current one.
pub struct Writer {
    path: PathBuf,
    lock_path: PathBuf,
    staging_dir: PathBuf,
    agent: String,
    harness: Harness,
    session: String,
}

impl Writer {
    pub fn new(
        agent_dir: &Path,
        agent: impl Into<String>,
        harness: Harness,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            harness != Harness::Unrecognized,
            "a harness this version does not recognize cannot be written: its numbers would be uninterpretable"
        );
        Ok(Self {
            path: harness_context_path(agent_dir),
            lock_path: agent_dir.join(LOCK_NAME),
            staging_dir: staging_dir(agent_dir)?,
            agent: agent.into(),
            harness,
            session: crate::harness_state::session_token(),
        })
    }

    /// Adopt the session's incarnation token, so sibling writer processes of one session agree on
    /// the provenance they publish. Provenance only: nothing is fenced on it.
    pub fn with_session(mut self, token: impl Into<String>) -> Self {
        self.session = token.into();
        self
    }

    /// Record a fresh reading, returning whether a write landed.
    ///
    /// The guard is [`Writer::write_locked`]'s quantization: a reading inside the written bucket is
    /// skipped, a bucket or proven Claude rate-limit crossing lands, and a record older than
    /// [`HARNESS_CONTEXT_HEARTBEAT`] lands whatever the bucket. Callers must hold a reading taken
    /// since their last write — the heartbeat re-publishes a *fresh* reading whose bucket happened
    /// not to change, and never re-stamps a stale one.
    pub fn observe(&mut self, reading: Reading) -> anyhow::Result<bool> {
        self.write_locked(Some(reading), None)
    }

    /// Record a compaction edge. Always lands: it is the one thing this record cannot reconstruct
    /// from the numbers, and it is rare enough that no cadence is needed.
    pub fn compacted(&mut self, compaction: Compaction) -> anyhow::Result<bool> {
        self.write_locked(None, Some(compaction))
    }

    /// Record a compaction edge together with the reading that follows it, in one landed write.
    pub fn compacted_with(
        &mut self,
        compaction: Compaction,
        reading: Reading,
    ) -> anyhow::Result<bool> {
        self.write_locked(Some(reading), Some(compaction))
    }

    fn write_locked(
        &mut self,
        reading: Option<Reading>,
        compaction: Option<Compaction>,
    ) -> anyhow::Result<bool> {
        let _lock = lock_exclusive(&self.lock_path)?;
        // A record this version cannot interpret — unparseable bytes, another schema, a harness
        // whose arithmetic is unknown — is not coalesced against: it reads as absent to every
        // consumer, so continuing its counters would be continuing something nobody can read.
        let current = read_record(&self.path)
            .filter(|record| record.schema == SCHEMA && record.harness != Harness::Unrecognized);
        let now_ms = crate::message::now_ms();
        if compaction.is_none()
            && let (Some(current), Some(reading)) = (current.as_ref(), reading.as_ref())
            && !self.due(current, reading, now_ms)
        {
            return Ok(false);
        }
        // A reading replaces the numbers wholesale — including with nulls, since a withheld value
        // is an observation and carrying the previous one forward would fabricate exactly what
        // HC-R03 forbids. The compaction counters belong to the seat rather than to any one
        // reading, so they carry forward across a reading that says nothing about them.
        let (used_tokens, window_tokens, used_percent, model, cost_usd, session_total, limits) =
            match (&reading, current.as_ref()) {
                (Some(reading), _) => (
                    reading.used_tokens,
                    reading.window_tokens,
                    reading.used_percent,
                    reading.model.clone(),
                    reading.cost_usd,
                    reading.session_total_tokens,
                    reading.rate_limits,
                ),
                (None, Some(current)) => (
                    current.used_tokens,
                    current.window_tokens,
                    current.used_percent,
                    current.model.clone(),
                    current.cost_usd,
                    current.session_total_tokens,
                    current.rate_limits,
                ),
                (None, None) => (None, None, None, None, None, None, RateLimits::default()),
            };
        let observed_at_ms = match (&reading, current.as_ref()) {
            // A compaction edge with no reading behind it does not re-stamp the reading: the
            // numbers are as old as they were, and saying otherwise would hide that.
            (None, Some(current)) => current.observed_at_ms,
            _ => now_ms,
        };
        let record = Record {
            schema: SCHEMA.to_string(),
            agent: self.agent.clone(),
            harness: self.harness,
            used_tokens,
            window_tokens,
            used_percent,
            model,
            cost_usd,
            session_total_tokens: session_total,
            rate_limits: limits,
            compactions: match (compaction, current.as_ref()) {
                (Some(edge), current) => edge
                    .count
                    .unwrap_or_else(|| current.map_or(0, |c| c.compactions).saturating_add(1)),
                (None, current) => current.map_or(0, |c| c.compactions),
            },
            last_compaction_ms: match compaction {
                Some(_) => Some(now_ms),
                None => current.as_ref().and_then(|c| c.last_compaction_ms),
            },
            last_compaction_trigger: match compaction {
                Some(edge) => Some(edge.trigger),
                None => current.as_ref().and_then(|c| c.last_compaction_trigger),
            },
            incarnation: self.session.clone(),
            observed_at_ms,
            // Strictly monotonic per record, so a landed write is byte-distinct even against a
            // same-millisecond predecessor. A stamp already beyond the future-skew bound is
            // somebody's garbage and is never inherited: this writer's clock wins instead.
            written_at_ms: current
                .as_ref()
                .map(|c| c.written_at_ms)
                .filter(|&previous| {
                    previous <= now_ms.saturating_add(duration_ms(HARNESS_CONTEXT_FUTURE_SKEW))
                })
                .map_or(now_ms, |previous| now_ms.max(previous.saturating_add(1))),
        };
        write_json_atomic(&self.path, &record, &self.staging_dir, TMP_PREFIX)?;
        Ok(true)
    }

    /// The write policy (HC-R09, HC-R10), isolated so it is one testable place: a reading lands
    /// when it enters a different bucket of the window, crosses proven Claude account-window
    /// exhaustion, or the record is older than the heartbeat. A compaction edge bypasses this
    /// entirely and is handled by the caller.
    fn due(&self, current: &Record, reading: &Reading, now_ms: u64) -> bool {
        if bucket(current.used_percent) != bucket(reading.used_percent) {
            return true;
        }
        if proves_rate_limited(current.harness, current.rate_limits)
            != proves_rate_limited(self.harness, reading.rate_limits)
        {
            return true;
        }
        // Age is measured from the WRITE, not from the reading: the clause asks how long it has
        // been since anything reached the transport.
        let age = now_ms.saturating_sub(current.written_at_ms);
        // A stamp from the future is untrustworthy in the direction that matters here — it would
        // suppress every heartbeat until the clock caught up — so it is treated as due.
        current.written_at_ms > now_ms.saturating_add(duration_ms(HARNESS_CONTEXT_FUTURE_SKEW))
            || age >= duration_ms(HARNESS_CONTEXT_HEARTBEAT)
    }
}

/// The bucket a reading falls in. Quantization is over `usedPercent` — the number a reader alarms
/// on (HC-R17) — rather than over the raw operands: Claude clamps its own percent and Codex
/// subtracts a baseline from both operands, so `used / window` sits in a different bucket than the
/// percent for two of five harnesses, and HC-R10's "the reader always shares the truth's bucket at
/// every threshold" would quietly stop holding.
///
/// A withheld percent has no bucket, so withheld↔known is itself a bucket change; while it stays
/// withheld only a compaction edge or the heartbeat writes.
fn bucket(used_percent: Option<f64>) -> Option<i64> {
    used_percent
        .filter(|percent| percent.is_finite())
        .map(|percent| (percent / HARNESS_CONTEXT_BUCKET_PERCENT).floor() as i64)
}

/// The reading a consumer sees. Note what is not here: no `unknown` vocabulary and no derived
/// percent. A stale reading arrives with `stale` set and its age attached (HC-R06), and
/// `used_percent` is whatever the harness published — st2 never divides `used_tokens` by
/// `window_tokens` to manufacture one (HC-R02).
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub harness: Harness,
    pub used_tokens: Option<u64>,
    pub window_tokens: Option<u64>,
    pub used_percent: Option<f64>,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub session_total_tokens: Option<u64>,
    pub rate_limits: RateLimits,
    pub compactions: u64,
    pub last_compaction_ms: Option<u64>,
    pub last_compaction_trigger: Option<CompactionTrigger>,
    /// When the reading was taken, from the record's own bytes — no read path consults file mtime.
    pub observed_at_ms: u64,
    /// Derived by the reader from `observed_at_ms`.
    pub age_ms: u64,
    pub stale: bool,
}

impl Observed {
    /// Whether fresh provider evidence proves that this harness cannot currently progress.
    pub fn is_rate_limited(&self) -> bool {
        !self.stale && proves_rate_limited(self.harness, self.rate_limits)
    }
}

/// Read an agent's harness-context record.
///
/// `None` means there is nothing trustworthy to report: no record (never observed), an unreadable
/// or unparseable one, a schema this version does not own, a harness whose arithmetic is unknown,
/// or a writer clock so far ahead that a derived age would be meaningless. Every other record
/// comes back — including one past the staleness horizon, which returns marked `stale` with its
/// age rather than being derived away (HC-R06).
///
/// The projection is computed independently of [`crate::harness_state`]: an agent whose observed
/// state reads `unknown` for any of that record's reasons still reports its last context reading
/// with the age it has (HC-R07).
pub fn read(path: &Path) -> Option<Observed> {
    read_at(path, crate::message::now_ms())
}

fn read_at(path: &Path, now_ms: u64) -> Option<Observed> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                "st2 harness-context: reading {} failed: {error}",
                path.display()
            );
            return None;
        }
    };
    let Ok(record) = serde_json::from_slice::<Record>(&raw) else {
        tracing::warn!(
            "st2 harness-context: {} is not a readable record",
            path.display()
        );
        return None;
    };
    if record.schema != SCHEMA {
        tracing::warn!(
            "st2 harness-context: {} carries schema `{}`, not `{SCHEMA}`",
            path.display(),
            record.schema
        );
        return None;
    }
    if record.harness == Harness::Unrecognized {
        tracing::warn!(
            "st2 harness-context: {} was written by a harness this version cannot interpret",
            path.display()
        );
        return None;
    }
    if record.observed_at_ms > now_ms.saturating_add(duration_ms(HARNESS_CONTEXT_FUTURE_SKEW)) {
        tracing::warn!(
            "st2 harness-context: {} is stamped beyond the future-skew bound",
            path.display()
        );
        return None;
    }
    let age_ms = now_ms.saturating_sub(record.observed_at_ms);
    Some(Observed {
        harness: record.harness,
        used_tokens: record.used_tokens,
        window_tokens: record.window_tokens,
        used_percent: record.used_percent,
        model: record.model,
        cost_usd: record.cost_usd,
        session_total_tokens: record.session_total_tokens,
        rate_limits: record.rate_limits,
        compactions: record.compactions,
        last_compaction_ms: record.last_compaction_ms,
        last_compaction_trigger: record.last_compaction_trigger,
        observed_at_ms: record.observed_at_ms,
        age_ms,
        stale: age_ms >= duration_ms(HARNESS_CONTEXT_STALE),
    })
}

/// Remove an agent's harness-context record at a session boundary (HC-R15).
///
/// A new incarnation must read "no context yet" rather than the previous incarnation's fill:
/// leaving the old numbers to age out over the horizon would show a crash-looping agent the
/// previous incarnation's 190k as if it were current. Taken under this record's own lock, which is
/// never held while the state record's lock is taken, so the claim path's lock order is one-way.
/// A record that is already gone is success; any other failure is reported to the caller rather
/// than being swallowed.
pub fn remove(agent_dir: &Path) -> anyhow::Result<()> {
    let _lock = lock_exclusive(&agent_dir.join(LOCK_NAME))?;
    match fs::remove_file(harness_context_path(agent_dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_record(path: &Path) -> Option<Record> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(dir: &Path) -> Writer {
        Writer::new(dir, "hetz.worker", Harness::Codex).unwrap()
    }

    /// A Codex-shaped reading: the operands and the percent the producer computed from them.
    fn reading(used: u64, percent: f64) -> Reading {
        Reading {
            used_tokens: Some(used),
            window_tokens: Some(258_400),
            used_percent: Some(percent),
            ..Reading::default()
        }
    }

    /// A catalog-shaped tree: a control directory at the root and the agent below it.
    fn catalog(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join(crate::catalog_lock::CONTROL_DIR)).unwrap();
        let agent_dir = root.join("agents").join("hetz").join("worker");
        fs::create_dir_all(&agent_dir).unwrap();
        agent_dir
    }

    #[test]
    fn missing_record_reads_as_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(&harness_context_path(tmp.path())), None);
    }

    /// The full envelope round-trips: the fill triple, the compaction triple, every adjacent fact,
    /// and the provenance token.
    #[test]
    fn the_envelope_round_trips_every_field_it_carries() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = writer(&agent_dir).with_session("session-1");
        writer
            .observe(Reading {
                used_tokens: Some(92_283),
                window_tokens: Some(258_400),
                used_percent: Some(33.0),
                model: Some("gpt-5".to_string()),
                cost_usd: Some(1.25),
                session_total_tokens: Some(2_235_329),
                rate_limits: RateLimits {
                    five_hour: Some(31.0),
                    seven_day: Some(55.0),
                },
            })
            .unwrap();
        writer
            .compacted(Compaction::new(CompactionTrigger::Manual))
            .unwrap();

        let observed = read(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(observed.harness, Harness::Codex);
        assert_eq!(observed.used_tokens, Some(92_283));
        assert_eq!(observed.window_tokens, Some(258_400));
        assert_eq!(observed.used_percent, Some(33.0));
        assert_eq!(observed.model.as_deref(), Some("gpt-5"));
        assert_eq!(observed.cost_usd, Some(1.25));
        assert_eq!(observed.session_total_tokens, Some(2_235_329));
        assert_eq!(observed.rate_limits.five_hour, Some(31.0));
        assert_eq!(observed.rate_limits.seven_day, Some(55.0));
        assert_eq!(observed.compactions, 1);
        assert!(observed.last_compaction_ms.is_some());
        assert_eq!(
            observed.last_compaction_trigger,
            Some(CompactionTrigger::Manual)
        );
        assert!(!observed.stale);

        let record = read_record(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(record.incarnation, "session-1");
        assert_eq!(record.schema, SCHEMA);
    }

    /// HC-R02/HC-R03: a withheld value is written as `null` and read back as `null`. Nothing —
    /// not zero, not the previous reading, not a division st2 could have done itself — is
    /// substituted for what a harness declined to state.
    #[test]
    fn withheld_values_are_null_and_are_never_fabricated() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = Writer::new(&agent_dir, "hetz.worker", Harness::Claude).unwrap();
        // Claude before its first API response: the window is populated, the rest is not.
        writer
            .observe(Reading {
                window_tokens: Some(200_000),
                ..Reading::default()
            })
            .unwrap();
        let observed = read(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(observed.used_tokens, None);
        assert_eq!(observed.used_percent, None);
        assert_eq!(observed.window_tokens, Some(200_000));
        // One absence convention across the whole record: every withheld fact is an emitted
        // `null`, never an omitted key and never a zero.
        let wire: serde_json::Value =
            serde_json::from_slice(&fs::read(harness_context_path(&agent_dir)).unwrap()).unwrap();
        for withheld in [
            "usedTokens",
            "usedPercent",
            "model",
            "costUsd",
            "sessionTotalTokens",
        ] {
            assert_eq!(wire[withheld], serde_json::Value::Null, "{withheld}");
        }
        assert_eq!(wire["rateLimits"]["fiveHour"], serde_json::Value::Null);
        assert_eq!(wire["rateLimits"]["sevenDay"], serde_json::Value::Null);

        // A later reading that withholds again does not resurrect an earlier known value.
        writer
            .observe(Reading {
                used_tokens: Some(40_000),
                window_tokens: Some(200_000),
                used_percent: Some(20.0),
                ..Reading::default()
            })
            .unwrap();
        writer
            .compacted(Compaction::new(CompactionTrigger::Auto))
            .unwrap();
        writer
            .observe(Reading {
                window_tokens: Some(200_000),
                ..Reading::default()
            })
            .unwrap();
        let observed = read(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(observed.used_percent, None, "a null is an observation");
        assert_eq!(observed.used_tokens, None);
        assert_eq!(observed.compactions, 1, "the seat's counter carries across");
    }

    /// HC-R02: occupancy above the window is a real observation and is carried raw. No producer
    /// and no reader clamps it — clamping belongs to whatever renders a bar.
    #[test]
    fn a_reading_above_the_window_is_carried_unclamped() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = Writer::new(&agent_dir, "hetz.worker", Harness::Pi).unwrap();
        writer
            .observe(Reading {
                used_tokens: Some(23_424),
                window_tokens: Some(4_000),
                used_percent: Some(585.6),
                ..Reading::default()
            })
            .unwrap();
        let observed = read(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(observed.used_percent, Some(585.6));
    }

    /// HC-R09/HC-R10: a reading inside the written bucket is not written; a bucket crossing is.
    /// Quantization is over `usedPercent`, so the write rate is capped by the window fill rather
    /// than by how often the producer speaks.
    #[test]
    fn a_reading_inside_the_written_bucket_does_not_write_and_a_crossing_does() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut writer = writer(&agent_dir);

        assert!(
            writer.observe(reading(85_000, 33.0)).unwrap(),
            "first lands"
        );
        let after_first = fs::read(&path).unwrap();
        assert!(
            !writer.observe(reading(85_400, 33.4)).unwrap(),
            "same bucket is drift"
        );
        assert!(
            !writer.observe(reading(85_900, 33.9)).unwrap(),
            "still the same bucket"
        );
        assert_eq!(fs::read(&path).unwrap(), after_first, "no write landed");

        assert!(
            writer.observe(reading(86_100, 34.0)).unwrap(),
            "entering bucket 34 is news"
        );
        // A hundred restatements inside one bucket still cost one write.
        let mut landed = 0;
        for step in 0..100 {
            if writer
                .observe(reading(86_100 + step, 34.0 + step as f64 / 1000.0))
                .unwrap()
            {
                landed += 1;
            }
        }
        assert_eq!(landed, 0, "a chatty producer cannot inflate the write rate");
    }

    #[test]
    fn claude_rate_limit_exhaustion_and_reset_crossings_land_inside_one_usage_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = Writer::new(&agent_dir, "hetz.worker", Harness::Claude).unwrap();
        let at_limit = |used_tokens, used_percent, five_hour| Reading {
            rate_limits: RateLimits {
                five_hour: Some(five_hour),
                seven_day: Some(55.0),
            },
            ..reading(used_tokens, used_percent)
        };

        assert!(writer.observe(at_limit(85_000, 33.0, 99.0)).unwrap());
        assert!(
            writer.observe(at_limit(85_400, 33.4, 100.0)).unwrap(),
            "exhaustion is news inside the same usage bucket"
        );
        assert!(
            read(&harness_context_path(&agent_dir))
                .unwrap()
                .is_rate_limited()
        );
        assert!(
            writer.observe(at_limit(85_900, 33.9, 0.0)).unwrap(),
            "a Claude reset is also news inside the same usage bucket"
        );
        assert!(
            !read(&harness_context_path(&agent_dir))
                .unwrap()
                .is_rate_limited()
        );
    }

    #[test]
    fn codex_account_window_exhaustion_does_not_prove_the_runtime_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = writer(&agent_dir);
        let path = harness_context_path(&agent_dir);

        assert!(
            writer
                .observe(Reading {
                    rate_limits: RateLimits {
                        five_hour: None,
                        seven_day: Some(100.0),
                    },
                    ..reading(85_000, 33.0)
                })
                .unwrap()
        );
        let exhausted = fs::read(&path).unwrap();
        assert!(
            !read(&path).unwrap().is_rate_limited(),
            "included allowance exhaustion is not Codex availability evidence"
        );
        assert!(
            !writer
                .observe(Reading {
                    rate_limits: RateLimits {
                        five_hour: None,
                        seven_day: Some(0.0),
                    },
                    ..reading(85_400, 33.4)
                })
                .unwrap(),
            "a Codex allowance reset is not a classification edge"
        );
        assert_eq!(fs::read(path).unwrap(), exhausted, "no write landed");
    }

    /// The withheld case has its own bucket behaviour: `null` has no bucket, so withheld↔known is
    /// a crossing, and while the percent stays withheld only a compaction or the heartbeat writes.
    #[test]
    fn a_withheld_percent_has_no_bucket_and_only_a_compaction_or_heartbeat_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = Writer::new(&agent_dir, "hetz.worker", Harness::Pi).unwrap();
        let withheld = |used: Option<u64>| Reading {
            used_tokens: used,
            window_tokens: Some(200_000),
            ..Reading::default()
        };
        assert!(writer.observe(withheld(None)).unwrap(), "first lands");
        assert!(
            !writer.observe(withheld(Some(1_000))).unwrap(),
            "still no percent, so still no bucket to cross"
        );
        assert!(
            writer.observe(reading(40_000, 20.0)).unwrap(),
            "withheld to known is a crossing"
        );
        assert!(
            writer.observe(withheld(None)).unwrap(),
            "known to withheld is a crossing too"
        );
        assert!(
            writer
                .compacted(Compaction::new(CompactionTrigger::Threshold))
                .unwrap(),
            "a compaction always lands"
        );
    }

    /// HC-R09's third clause: a record older than the heartbeat is re-published even though its
    /// bucket did not change, and HC-R04's byte-distinctness holds for that write.
    #[test]
    fn a_record_older_than_the_heartbeat_is_rewritten_and_every_landed_write_is_byte_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut writer = writer(&agent_dir);
        writer.observe(reading(85_000, 33.0)).unwrap();
        let first = fs::read(&path).unwrap();

        // Age the record past the heartbeat by rewriting its stamps, exactly as the passage of
        // time would.
        let mut record = read_record(&path).unwrap();
        let aged = record
            .written_at_ms
            .saturating_sub(duration_ms(HARNESS_CONTEXT_HEARTBEAT) + 1);
        record.written_at_ms = aged;
        record.observed_at_ms = aged;
        write_json_atomic(&path, &record, tmp.path(), TMP_PREFIX).unwrap();

        assert!(
            writer.observe(reading(85_100, 33.0)).unwrap(),
            "the heartbeat is due"
        );
        let second = fs::read(&path).unwrap();
        assert_ne!(first, second);
        assert!(
            !writer.observe(reading(85_200, 33.0)).unwrap(),
            "and the cadence starts over"
        );
    }

    /// HC-R06: past the horizon the reading is RETURNED, marked stale and carrying its age. There
    /// is no `unknown` on this axis and no path from an old number to no number.
    #[test]
    fn a_stale_reading_is_returned_with_its_age_rather_than_derived_away() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut writer = writer(&agent_dir);
        writer.observe(reading(250_000, 96.7)).unwrap();

        let record = read_record(&path).unwrap();
        let later = record.observed_at_ms + duration_ms(HARNESS_CONTEXT_STALE) + 5_000;
        let observed = read_at(&path, later).unwrap();
        assert!(observed.stale);
        assert_eq!(observed.used_percent, Some(96.7));
        assert_eq!(observed.used_tokens, Some(250_000));
        assert!(observed.age_ms >= duration_ms(HARNESS_CONTEXT_STALE));

        let fresh = read_at(&path, record.observed_at_ms + 4_210).unwrap();
        assert!(!fresh.stale);
        assert_eq!(fresh.age_ms, 4_210);
    }

    /// `ageMs` is derived from the record's own `observedAtMs`, never from file mtime — a
    /// transport that rewrites a file does not make its numbers younger.
    #[test]
    fn freshness_comes_from_the_record_bytes_and_never_from_file_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut writer = writer(&agent_dir);
        writer.observe(reading(85_000, 33.0)).unwrap();

        let mut record = read_record(&path).unwrap();
        let now = crate::message::now_ms();
        record.observed_at_ms = now.saturating_sub(duration_ms(HARNESS_CONTEXT_STALE) + 1_000);
        // Rewritten right now: the file is brand new, its reading is not.
        write_json_atomic(&path, &record, tmp.path(), TMP_PREFIX).unwrap();
        let observed = read_at(&path, now).unwrap();
        assert!(observed.stale, "a fresh file with an old reading is stale");
    }

    /// HC-R12: a compaction edge always lands, carries its trigger, and — where the harness's own
    /// session store answers the question — its durable count.
    #[test]
    fn a_compaction_always_lands_with_its_trigger_and_may_carry_a_durable_count() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = writer(&agent_dir);
        writer.observe(reading(250_000, 96.7)).unwrap();
        assert!(
            writer
                .compacted(Compaction::new(CompactionTrigger::Unknown))
                .unwrap()
        );
        assert!(
            writer
                .compacted(Compaction::new(CompactionTrigger::Unknown))
                .unwrap()
        );
        let observed = read(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(observed.compactions, 2, "st2 counted the two edges");
        assert_eq!(
            observed.used_percent,
            Some(96.7),
            "no reading behind the edge, so no invented number"
        );
        assert!(
            observed.age_ms < duration_ms(HARNESS_CONTEXT_STALE),
            "the reading is still the one taken before the edge, and still fresh"
        );

        // A harness-durable count replaces st2's, so a restart does not restart the counter.
        writer
            .compacted(Compaction::new(CompactionTrigger::Overflow).with_count(9))
            .unwrap();
        let observed = read(&harness_context_path(&agent_dir)).unwrap();
        assert_eq!(observed.compactions, 9);
        assert_eq!(
            observed.last_compaction_trigger,
            Some(CompactionTrigger::Overflow)
        );
    }

    /// A compaction edge with no reading behind it leaves the reading's own age alone: the numbers
    /// are exactly as old as they were, and re-stamping them would hide that.
    #[test]
    fn a_compaction_does_not_restamp_the_reading_it_did_not_take() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut writer = writer(&agent_dir);
        writer.observe(reading(250_000, 96.7)).unwrap();
        let observed_at = read_record(&path).unwrap().observed_at_ms;
        writer
            .compacted(Compaction::new(CompactionTrigger::Auto))
            .unwrap();
        let after = read_record(&path).unwrap();
        assert_eq!(after.observed_at_ms, observed_at);
        assert!(after.written_at_ms > observed_at.saturating_sub(1));
    }

    /// Additive tolerance on read, and the three records that read as nothing: an unparseable one,
    /// a foreign schema, and a harness whose arithmetic this version cannot interpret.
    #[test]
    fn additive_fields_decode_but_foreign_schema_and_harness_read_as_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = harness_context_path(tmp.path());
        assert!(read(&path).is_none());

        fs::write(&path, b"not json\n").unwrap();
        assert!(read(&path).is_none());

        fs::write(
            &path,
            br#"{"schema":"st2.harness-context.v2","agent":"a","harness":"codex","observedAtMs":1}"#,
        )
        .unwrap();
        assert!(read(&path).is_none(), "a schema this version does not own");

        fs::write(
            &path,
            br#"{"schema":"st2.harness-context.v1","agent":"a","harness":"someFutureHarness","usedPercent":50,"observedAtMs":1}"#,
        )
        .unwrap();
        assert!(
            read(&path).is_none(),
            "unknown arithmetic makes the numbers meaningless"
        );

        // An additive field a future writer added decodes without complaint, and an unrecognized
        // trigger word decodes as `unknown` rather than as a definite trigger.
        let now = crate::message::now_ms();
        fs::write(
            &path,
            format!(
                r#"{{"schema":"st2.harness-context.v1","agent":"a","harness":"codex","usedPercent":50,"lastCompactionTrigger":"someFutureReason","tomorrowsField":7,"observedAtMs":{now}}}"#
            ),
        )
        .unwrap();
        let observed = read(&path).unwrap();
        assert_eq!(observed.used_percent, Some(50.0));
        assert_eq!(
            observed.last_compaction_trigger,
            Some(CompactionTrigger::Unknown)
        );
    }

    /// A writer clock far enough ahead makes the derived age meaningless, so the record reads as
    /// absent rather than as a value with a nonsense age.
    #[test]
    fn a_record_beyond_the_future_skew_bound_reads_as_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut writer = writer(&agent_dir);
        writer.observe(reading(85_000, 33.0)).unwrap();
        let record = read_record(&path).unwrap();
        let earlier = record
            .observed_at_ms
            .saturating_sub(duration_ms(HARNESS_CONTEXT_FUTURE_SKEW) + 1_000);
        assert!(read_at(&path, earlier).is_none());
        // Just inside the bound, the record is still readable.
        let barely = record.observed_at_ms.saturating_sub(1_000);
        assert!(read_at(&path, barely).is_some());
    }

    /// A harness this version does not recognize can be decoded but never written: its numbers
    /// would be uninterpretable to every reader, including this one.
    #[test]
    fn an_unrecognized_harness_cannot_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        assert!(Writer::new(&agent_dir, "hetz.worker", Harness::Unrecognized).is_err());
    }

    /// HC-R05: publication stages below this catalog's control directory, never in `agents`, and
    /// both successful and failed renames consume or clean their temporary file.
    #[test]
    fn writes_stage_in_catalog_control_and_clean_up_after_success_or_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let staging = staging_dir(&agent_dir).unwrap();
        assert_eq!(
            staging,
            tmp.path()
                .join(crate::catalog_lock::CONTROL_DIR)
                .join(STAGING_DIR_NAME)
        );
        assert!(
            !staging.starts_with(tmp.path().join("agents")),
            "staging must be outside the complete identity namespace"
        );
        assert_eq!(
            fs::metadata(&staging).unwrap().dev(),
            fs::metadata(&agent_dir).unwrap().dev(),
            "rename must remain on one filesystem"
        );

        let mut writer = writer(&agent_dir);
        writer.observe(reading(85_000, 33.0)).unwrap();
        assert!(harness_context_path(&agent_dir).is_file());
        assert!(
            fs::read_dir(&staging).unwrap().next().is_none(),
            "a successful rename consumes its staging file"
        );

        fs::remove_file(harness_context_path(&agent_dir)).unwrap();
        fs::create_dir(harness_context_path(&agent_dir)).unwrap();
        assert!(writer.observe(reading(90_000, 35.0)).is_err());
        assert!(
            fs::read_dir(&staging).unwrap().next().is_none(),
            "a failed rename cleans its staging file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_requires_real_canonical_catalog_ancestry() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let noncanonical = tmp.path().join("other/host/worker");
        fs::create_dir_all(&noncanonical).unwrap();
        assert!(staging_dir(&noncanonical).is_err());

        let real = tmp.path().join("real-worker");
        fs::create_dir(&real).unwrap();
        let linked = tmp.path().join("agents/host/worker");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        symlink(&real, &linked).unwrap();
        assert!(staging_dir(&linked).is_err());
        assert!(
            !tmp.path().join(crate::catalog_lock::CONTROL_DIR).exists(),
            "rejected ancestry must not create control state"
        );
    }

    #[test]
    fn only_the_exact_legacy_regular_file_shape_is_reserved() {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path();
        for (name, reserved) in [
            (".harness-context.tmp-123-0", true),
            (".harness-context.tmp-001-002", true),
            (".harness-context.tmp--1", false),
            (".harness-context.tmp-1-", false),
            (".harness-context.tmp-1-2-extra", false),
            (".harness-context.tmp-pid-2", false),
            (".other.tmp-1-2", false),
        ] {
            let path = host.join(name);
            fs::write(&path, "stale").unwrap();
            let entry = fs::read_dir(host)
                .unwrap()
                .find_map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name() == OsStr::new(name)).then_some(entry)
                })
                .unwrap();
            assert_eq!(
                is_legacy_harness_context_staging_file(&entry).unwrap(),
                reserved,
                "{name}"
            );
            fs::remove_file(path).unwrap();
        }
    }

    /// HC-R05, naming half: the exact record names st2 expects the transport's include list to
    /// carry. The list lives in another repository, so a rename here would otherwise stop
    /// replication in silence — this is the assertion that makes st2's half breakable.
    #[test]
    fn the_replicated_driver_record_names_are_pinned() {
        assert_eq!(
            REPLICATED_DRIVER_RECORDS,
            ["harness-state", "harness-context"]
        );
        let tmp = tempfile::tempdir().unwrap();
        // Both readers derive their paths from those exact names, so the pin is not decorative.
        for (name, path) in REPLICATED_DRIVER_RECORDS.iter().zip([
            crate::harness_state::harness_state_path(tmp.path()),
            harness_context_path(tmp.path()),
        ]) {
            assert_eq!(path, tmp.path().join(name));
        }
    }

    /// HC-R15: the record is removable at a session boundary, and removing one that is already
    /// gone is success rather than an error the claim path would have to special-case.
    #[test]
    fn the_record_is_removable_and_removing_an_absent_one_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let mut writer = writer(&agent_dir);
        writer.observe(reading(190_000, 73.5)).unwrap();
        assert!(read(&harness_context_path(&agent_dir)).is_some());
        remove(&agent_dir).unwrap();
        assert!(read(&harness_context_path(&agent_dir)).is_none());
        remove(&agent_dir).unwrap();
    }

    /// HC-T04: the record is unfenced. A straggler from a superseded session lands its reading and
    /// says only "this number is older than you think" — which the incarnation it carries makes
    /// visible, and which the next real reading overwrites.
    #[test]
    fn a_straggler_lands_and_is_visible_as_provenance_rather_than_being_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = catalog(tmp.path());
        let path = harness_context_path(&agent_dir);
        let mut old = writer(&agent_dir).with_session("session-old");
        let mut new = writer(&agent_dir).with_session("session-new");

        new.observe(reading(20_000, 8.0)).unwrap();
        assert!(old.observe(reading(250_000, 96.7)).unwrap(), "not refused");
        assert_eq!(read_record(&path).unwrap().incarnation, "session-old");
        assert!(new.observe(reading(21_000, 9.0)).unwrap());
        assert_eq!(read_record(&path).unwrap().incarnation, "session-new");
        assert_eq!(read(&path).unwrap().used_percent, Some(9.0));
    }
}
