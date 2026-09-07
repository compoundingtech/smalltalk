//! The agent roster (M2.3): the data behind `st2 agents`. Enumerates the catalog's agents with their
//! presence status and retirement state, and optionally last-activity + inbox count. The JSON field
//! names, order, and null handling are a stable machine-readable contract.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::message;
use crate::status::{self, State};
use crate::{AgentSpec, Discovered, Resource, driver_diagnostic, harness_context, harness_state};

/// One roster row: everything `st2 agents [--enrich]` can report about an agent.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// The bus id — `<host>.<identity>`.
    pub identity: String,
    /// Declaration source used to attribute this runtime observation.
    pub source_path: PathBuf,
    /// Effective presence (derived: stale → `unknown`, etc.).
    pub status: State,
    /// Optional display name from the Agent Spec declaration.
    pub name: Option<String>,
    /// Optional enduring responsibility boundary from the Agent Spec declaration.
    pub description: Option<String>,
    /// Whether the declaration is explicitly retired. Presence remains a separate runtime signal.
    pub retired: bool,
    /// Declarative whole-agent lifecycle intent.
    pub desired_state: String,
    /// Human-facing rationale for suspended/retired new-style declarations.
    pub desired_state_reason: Option<String>,
    /// Typed Resource bindings declared directly by the agent.
    pub resources: Vec<Resource>,
    /// Resync coverage derived from the declaration directory and each Resource URI.
    pub resource_resync: Vec<crate::resync::ResyncCoverage>,
    /// Newest activity time across inbox, archive, and status. Version 1 status uses its embedded
    /// writer timestamp; message files and legacy status use local mtime. `--enrich` only.
    pub last_activity_ms: Option<f64>,
    /// Count of canonical message files in the agent's inbox. `--enrich` only.
    pub inbox: usize,
    /// Observed harness state — the driver-owned signal of what the harness is seen doing, a third
    /// axis independent from declared presence and from desired lifecycle. `None` means no driver
    /// has ever published a record for this agent, which is different from a derived `unknown`.
    pub observed: Option<harness_state::Observed>,
    /// Current native-driver diagnostic. Absence and unreadable records remain explicit states;
    /// neither is projected as healthy.
    pub driver_diagnostic: driver_diagnostic::Observed,
    /// Harness context — how full the harness's window is, a fourth axis independent of the other
    /// three. `None` means no record exists; a record past its horizon is still reported, marked
    /// stale and carrying its age, so it survives every `observedState: unknown` derivation.
    pub context: Option<harness_context::Observed>,
    /// The immutable catalog-global agent ID (R24) — the declaration's explicit `id`, else the
    /// legacy `<host>.<identity>` bus identity that migration freezes as this subject's ID.
    /// `identity` above stays the positional declaration key and legacy address fallback;
    /// consumers must not read it as the agent ID.
    pub id: String,
    /// The effective mutable agent address — declared `address`, else positional `identity`.
    pub address: String,
    /// `<host>.<effective address>`. `None` for a retired subject, which is non-routable and
    /// releases its address; that null does not make the row incomplete.
    pub bus_address: Option<String>,
}

/// Every agent in the catalog, sorted by bus id, with presence + enrich data computed. Read-only:
/// walks discovered specs and each agent's resources, mutating nothing.
pub fn roster(catalog_root: &Path, this_host: &str) -> Vec<AgentRow> {
    let found = crate::discover(catalog_root);
    roster_from_discovered(&found, catalog_root, this_host)
}

/// Project a roster from one immutable discovery result. Exact selectors use this after proving
/// discovery complete so the uniqueness check and returned metadata describe the same snapshot.
pub fn roster_from_discovered(
    found: &Discovered,
    catalog_root: &Path,
    this_host: &str,
) -> Vec<AgentRow> {
    let pty_root = probe_pty_root(catalog_root);
    let profiles = crate::catalog::declared_profiles(catalog_root).unwrap_or_default();
    let profile_refresh = profiles.begin_refresh();
    let mut rows: Vec<AgentRow> = found
        .specs
        .iter()
        .filter_map(|s| {
            let agent_dir = s.path.parent()?;
            Some(AgentRow {
                identity: s.bus_id(this_host),
                source_path: s.path.clone(),
                status: status::read_state(&status::status_path(agent_dir)),
                name: s.name.clone(),
                description: s.description.clone(),
                retired: s.desired_state.is_retired(),
                desired_state: s.desired_state.as_str().to_owned(),
                desired_state_reason: s.desired_state.reason().map(str::to_owned),
                resources: s.resources.clone(),
                resource_resync: s
                    .resources
                    .iter()
                    .map(|resource| {
                        crate::resync::resource_coverage_with_profiles(
                            agent_dir,
                            resource,
                            &profile_refresh,
                        )
                    })
                    .collect(),
                last_activity_ms: newest_activity_ms(agent_dir),
                inbox: inbox_count(agent_dir),
                observed: observed_state(s, agent_dir, &pty_root, this_host),
                driver_diagnostic: driver_diagnostic::read(&driver_diagnostic::path(agent_dir)),
                // Read independently of the state record above: the wedge case this exists for is
                // an agent whose state has gone indeterminate at 190k of a 200k window.
                context: harness_context::read(&harness_context::harness_context_path(agent_dir)),
                id: s.effective_id(this_host),
                address: s.effective_address().to_owned(),
                // A retired subject does not resolve and does not occupy the address namespace.
                bus_address: (!s.desired_state.is_retired())
                    .then(|| s.bus_address(this_host)),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.identity.cmp(&b.identity));
    rows
}

/// Read the observed-harness-state record beside `status`. The session-liveness cross-check is
/// host-local by construction: it applies only to agents this host runs, and an unreadable
/// registry downgrades nothing.
fn observed_state(
    spec: &AgentSpec,
    agent_dir: &Path,
    pty_root: &Path,
    this_host: &str,
) -> Option<harness_state::Observed> {
    let path = harness_state::harness_state_path(agent_dir);
    if spec.resolved_host(this_host) == this_host {
        let probe = |session: &str| crate::ding::session_liveness_in(pty_root, session);
        harness_state::read(&path, Some(&probe))
    } else {
        harness_state::read(&path, None)
    }
}

/// The pty registry root the probe reads: exactly the runner's own resolution, so the reader and
/// the sessions it probes can never disagree. The runner honors `PTY_ROOT` and nothing else — a
/// legacy `PTY_SESSION_DIR` here would point the probe at a directory st2-managed sessions never
/// use, turning provable deaths into indeterminate reads.
pub fn probe_pty_root(catalog_root: &Path) -> PathBuf {
    crate::run::effective_pty_root(catalog_root)
}

/// The `observedState` object inside a roster row. Vocabulary words are the record's own
/// (`as_str`), never Rust identifier spellings.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservedJson<'a> {
    state: &'a str,
    blocked_on: &'a str,
    input_buffer: &'a str,
    ask: &'a str,
    harness: Option<&'a str>,
    since: Option<u64>,
    reason: Option<&'a str>,
    exit: Option<&'a str>,
}

impl<'a> ObservedJson<'a> {
    fn from_row(observed: Option<&'a harness_state::Observed>) -> Option<Self> {
        observed.map(|observed| ObservedJson {
            state: observed.state.as_str(),
            blocked_on: observed.blocked_on.as_str(),
            input_buffer: observed.input_buffer.as_str(),
            ask: observed.ask.as_str(),
            harness: observed.harness.as_deref(),
            since: observed.since_ms,
            reason: observed.reason.as_deref(),
            exit: observed.exit.as_deref(),
        })
    }
}

/// The closed `driverDiagnostic` object. Every state uses the same field set so a malformed,
/// unsupported, or absent record remains machine-visible rather than disappearing as a healthy
/// null/default.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DriverDiagnosticJson<'a> {
    status: &'static str,
    driver: Option<&'a str>,
    stage: Option<&'static str>,
    reason: Option<&'static str>,
    source: Option<&'static str>,
    producer_version: Option<&'a str>,
    support: &'static str,
    observed_at: Option<u64>,
    evidence_age_ms: Option<u64>,
    recovery: &'static str,
}

impl<'a> DriverDiagnosticJson<'a> {
    fn from_row(observed: &'a driver_diagnostic::Observed) -> Self {
        match observed {
            driver_diagnostic::Observed::Absent => Self {
                status: observed.status(),
                driver: None,
                stage: None,
                reason: None,
                source: None,
                producer_version: None,
                support: "unknown",
                observed_at: None,
                evidence_age_ms: None,
                recovery: "publishFailureOrClearOnStageRecovery",
            },
            driver_diagnostic::Observed::Indeterminate(reason) => Self {
                status: observed.status(),
                driver: None,
                stage: None,
                reason: Some(reason.as_str()),
                source: None,
                producer_version: None,
                support: "unknown",
                observed_at: None,
                evidence_age_ms: None,
                recovery: "replaceWithValidRecordOrClearOnStageRecovery",
            },
            driver_diagnostic::Observed::Failure(failure) => Self {
                status: observed.status(),
                driver: Some(failure.driver.as_str()),
                stage: Some(failure.stage.as_str()),
                reason: Some(failure.reason.as_str()),
                source: Some(failure.source.as_str()),
                producer_version: failure.producer_version.as_deref(),
                support: failure.support.as_str(),
                observed_at: Some(failure.observed_at),
                evidence_age_ms: Some(failure.evidence_age_ms),
                recovery: "clearsOnStageRecovery",
            },
        }
    }
}

/// The `context` object inside a roster row — the fourth top-level axis. The reading projection
/// is already applied, so a consumer never re-implements staleness and never sees a record it
/// would have to age itself. Vocabulary words are the record's own (`as_str`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextJson<'a> {
    harness: &'static str,
    used_tokens: Option<u64>,
    window_tokens: Option<u64>,
    used_percent: Option<f64>,
    model: Option<&'a str>,
    cost_usd: Option<f64>,
    session_total_tokens: Option<u64>,
    rate_limits: harness_context::RateLimits,
    rate_limited: bool,
    compactions: u64,
    last_compaction_ms: Option<u64>,
    last_compaction_trigger: Option<&'static str>,
    /// Spelled `observedAtMs` on the wire, matching the record's own field and the
    /// `sinceMs`/`writtenAtMs` convention the driver records already use. `driverDiagnostic`'s
    /// unsuffixed `observedAt` is a different record's shipped name and is not touched here.
    observed_at_ms: u64,
    age_ms: u64,
    stale: bool,
}

impl<'a> ContextJson<'a> {
    fn from_row(context: Option<&'a harness_context::Observed>) -> Option<Self> {
        context.map(|context| ContextJson {
            harness: context.harness.as_str(),
            used_tokens: context.used_tokens,
            window_tokens: context.window_tokens,
            // Carried exactly as the harness published it: never clamped here, and never divided
            // out of the operands beside it.
            used_percent: context.used_percent,
            model: context.model.as_deref(),
            cost_usd: context.cost_usd,
            session_total_tokens: context.session_total_tokens,
            rate_limits: context.rate_limits,
            rate_limited: context.is_rate_limited(),
            compactions: context.compactions,
            last_compaction_ms: context.last_compaction_ms,
            last_compaction_trigger: context
                .last_compaction_trigger
                .map(harness_context::CompactionTrigger::as_str),
            observed_at_ms: context.observed_at_ms,
            age_ms: context.age_ms,
            stale: context.stale,
        })
    }
}

#[derive(Serialize)]
struct ResourceJson<'a> {
    name: &'a str,
    uri: &'a str,
    reason: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    inactive_reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selector: Option<&'a serde_json::Value>,
    resync: &'static str,
}

fn resource_json(row: &AgentRow) -> Vec<ResourceJson<'_>> {
    row.resources
        .iter()
        .zip(&row.resource_resync)
        .map(|(resource, coverage)| ResourceJson {
            name: resource.name(),
            uri: resource.uri(),
            reason: resource.reason(),
            inactive_reason: resource.inactive_reason(),
            selector: resource.selector(),
            resync: coverage.as_str(),
        })
        .collect()
}

/// `st2 agents --json` row. Field order and names are the stable wire contract.
#[derive(Serialize)]
struct SummaryJson<'a> {
    identity: &'a str,
    status: &'a str,
    name: Option<&'a str>,
    description: Option<&'a str>,
    retired: bool,
    resources: Vec<ResourceJson<'a>>,
    #[serde(rename = "desiredState")]
    desired_state: &'a str,
    #[serde(rename = "desiredStateReason")]
    desired_state_reason: Option<&'a str>,
    #[serde(rename = "observedState")]
    observed_state: Option<ObservedJson<'a>>,
    #[serde(rename = "driverDiagnostic")]
    driver_diagnostic: DriverDiagnosticJson<'a>,
    context: Option<ContextJson<'a>>,
    /// Appended (R24): the immutable agent ID. `identity` above keeps its existing meaning as the
    /// positional declaration key and legacy address fallback — never read it as the agent ID.
    id: &'a str,
    /// Appended (R24): the effective mutable address.
    address: &'a str,
    /// Appended (R24): `<host>.<address>`; `null` for a retired, non-routable subject.
    #[serde(rename = "busAddress")]
    bus_address: Option<&'a str>,
}

/// `st2 agents --json --enrich` row (adds `lastActivity` and `inbox`).
#[derive(Serialize)]
struct EnrichedJson<'a> {
    identity: &'a str,
    status: &'a str,
    name: Option<&'a str>,
    description: Option<&'a str>,
    retired: bool,
    resources: Vec<ResourceJson<'a>>,
    #[serde(rename = "lastActivity")]
    last_activity: Option<f64>,
    inbox: usize,
    #[serde(rename = "desiredState")]
    desired_state: &'a str,
    #[serde(rename = "desiredStateReason")]
    desired_state_reason: Option<&'a str>,
    #[serde(rename = "observedState")]
    observed_state: Option<ObservedJson<'a>>,
    #[serde(rename = "driverDiagnostic")]
    driver_diagnostic: DriverDiagnosticJson<'a>,
    context: Option<ContextJson<'a>>,
    /// Appended (R24): the immutable agent ID. `identity` above keeps its existing meaning as the
    /// positional declaration key and legacy address fallback — never read it as the agent ID.
    id: &'a str,
    /// Appended (R24): the effective mutable address.
    address: &'a str,
    /// Appended (R24): `<host>.<address>`; `null` for a retired, non-routable subject.
    #[serde(rename = "busAddress")]
    bus_address: Option<&'a str>,
}

/// Serialize a roster to the stable JSON emitted by `st2 agents --json [--enrich]`.
pub fn to_json(rows: &[AgentRow], enrich: bool) -> String {
    if enrich {
        let out: Vec<EnrichedJson> = rows
            .iter()
            .map(|r| EnrichedJson {
                identity: &r.identity,
                status: r.status.as_str(),
                name: r.name.as_deref(),
                description: r.description.as_deref(),
                retired: r.retired,
                resources: resource_json(r),
                desired_state: &r.desired_state,
                desired_state_reason: r.desired_state_reason.as_deref(),
                last_activity: r.last_activity_ms,
                inbox: r.inbox,
                observed_state: ObservedJson::from_row(r.observed.as_ref()),
                driver_diagnostic: DriverDiagnosticJson::from_row(&r.driver_diagnostic),
                context: ContextJson::from_row(r.context.as_ref()),
                id: &r.id,
                address: &r.address,
                bus_address: r.bus_address.as_deref(),
            })
            .collect();
        serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_string())
    } else {
        let out: Vec<SummaryJson> = rows
            .iter()
            .map(|r| SummaryJson {
                identity: &r.identity,
                status: r.status.as_str(),
                name: r.name.as_deref(),
                description: r.description.as_deref(),
                retired: r.retired,
                resources: resource_json(r),
                desired_state: &r.desired_state,
                desired_state_reason: r.desired_state_reason.as_deref(),
                observed_state: ObservedJson::from_row(r.observed.as_ref()),
                driver_diagnostic: DriverDiagnosticJson::from_row(&r.driver_diagnostic),
                context: ContextJson::from_row(r.context.as_ref()),
                id: &r.id,
                address: &r.address,
                bus_address: r.bus_address.as_deref(),
            })
            .collect();
        serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_string())
    }
}
/// Runtime-only projection used by the versioned catalog graph. Declaration identity, lifecycle,
/// and topology stay on the graph row rather than being inferred from this observation.
pub(crate) fn graph_runtime_value(row: &AgentRow) -> serde_json::Value {
    serde_json::json!({
        "presence": row.status.as_str(),
        "lastActivityMs": row.last_activity_ms,
        "inbox": row.inbox,
        "observedState": ObservedJson::from_row(row.observed.as_ref()),
        "driverDiagnostic": DriverDiagnosticJson::from_row(&row.driver_diagnostic),
        "context": ContextJson::from_row(row.context.as_ref()),
    })
}

/// Count logically unread messages in the agent's `resources/inbox`. A same-filename archive receipt
/// suppresses and cleans a raw inbox duplicate restored by eventually-consistent sync.
fn inbox_count(agent_dir: &Path) -> usize {
    message::list_inbox(&message::inbox_dir(agent_dir))
        .map(|msgs| msgs.len())
        .unwrap_or(0)
}

/// Newest activity time across status and message state. A version 1 status contributes its origin
/// timestamp. Inbox, archive, and legacy status retain their local-mtime behavior.
fn newest_activity_ms(agent_dir: &Path) -> Option<f64> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in [
        message::inbox_dir(agent_dir),
        message::archive_dir(agent_dir),
    ] {
        if let Ok(rd) = fs::read_dir(&dir) {
            candidates.extend(rd.flatten().map(|e| e.path()));
        }
    }
    let mut newest = status::activity_time_ms(&status::status_path(agent_dir));
    for p in candidates {
        if let Ok(m) = fs::metadata(&p)
            && let Ok(t) = m.modified()
            && let Ok(duration) = t.duration_since(UNIX_EPOCH)
        {
            let timestamp = duration.as_secs_f64() * 1000.0;
            if newest.is_none_or(|current| timestamp > current) {
                newest = Some(timestamp);
            }
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        identity: &str,
        status: State,
        name: Option<&str>,
        retired: bool,
        last: Option<f64>,
        inbox: usize,
    ) -> AgentRow {
        AgentRow {
            identity: identity.to_string(),
            source_path: PathBuf::new(),
            status,
            name: name.map(str::to_string),
            description: None,
            retired,
            desired_state: if retired { "retired" } else { "running" }.to_owned(),
            desired_state_reason: None,
            resources: Vec::new(),
            resource_resync: Vec::new(),
            last_activity_ms: last,
            inbox,
            observed: None,
            driver_diagnostic: driver_diagnostic::Observed::Absent,
            context: None,
            // A legacy declaration: the frozen ID is its bus identity, the effective address is
            // its positional identity, and a retired subject releases its bus address.
            id: identity.to_string(),
            address: identity
                .split_once('.')
                .map(|(_, positional)| positional.to_string())
                .unwrap_or_else(|| identity.to_string()),
            bus_address: (!retired).then(|| identity.to_string()),
        }
    }

    /// Field names, order, and null handling are stable (see INVARIANTS.md).
    #[test]
    fn agents_json_has_stable_wire_shape() {
        let rows = [
            row(
                "hetz.cos-claude",
                State::Available,
                None,
                false,
                Some(1784653027733.6138),
                1,
            ),
            row("hetz.st2-claude", State::Busy, Some("owner"), true, None, 0),
        ];

        assert_eq!(
            to_json(&rows, false),
            r#"[{"identity":"hetz.cos-claude","status":"available","name":null,"description":null,"retired":false,"resources":[],"desiredState":"running","desiredStateReason":null,"observedState":null,"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.cos-claude","address":"cos-claude","busAddress":"hetz.cos-claude"},{"identity":"hetz.st2-claude","status":"busy","name":"owner","description":null,"retired":true,"resources":[],"desiredState":"retired","desiredStateReason":null,"observedState":null,"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.st2-claude","address":"st2-claude","busAddress":null}]"#
        );
        assert_eq!(
            to_json(&rows, true),
            r#"[{"identity":"hetz.cos-claude","status":"available","name":null,"description":null,"retired":false,"resources":[],"lastActivity":1784653027733.6138,"inbox":1,"desiredState":"running","desiredStateReason":null,"observedState":null,"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.cos-claude","address":"cos-claude","busAddress":"hetz.cos-claude"},{"identity":"hetz.st2-claude","status":"busy","name":"owner","description":null,"retired":true,"resources":[],"lastActivity":null,"inbox":0,"desiredState":"retired","desiredStateReason":null,"observedState":null,"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.st2-claude","address":"st2-claude","busAddress":null}]"#
        );
        // Empty roster is `[]`, not `null`.
        assert_eq!(to_json(&[], true), "[]");
    }

    #[test]
    fn agents_json_preserves_opaque_declared_resource_descriptors() {
        let mut resource_row = row("hetz.worker", State::Available, None, false, None, 0);
        resource_row.resources.push(
            Resource::new(
                "work".into(),
                "vendor+thing://authority/exact%20identity".into(),
                "Current implementation task.".into(),
            )
            .unwrap(),
        );
        resource_row
            .resource_resync
            .push(crate::resync::ResyncCoverage::Unsupported);

        assert_eq!(
            to_json(&[resource_row], false),
            r#"[{"identity":"hetz.worker","status":"available","name":null,"description":null,"retired":false,"resources":[{"name":"work","uri":"vendor+thing://authority/exact%20identity","reason":"Current implementation task.","resync":"unsupported"}],"desiredState":"running","desiredStateReason":null,"observedState":null,"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.worker","address":"worker","busAddress":"hetz.worker"}]"#
        );
    }

    /// Declared presence and observed harness state are independent axes in one payload: a
    /// declared `busy` sits beside an observed `idle` (the wedged-agent signal), and
    /// `lastActivity` keeps its existing meaning untouched by the new field.
    #[test]
    fn observed_state_joins_declared_presence_without_touching_either() {
        let mut wedged = row(
            "hetz.worker",
            State::Busy,
            None,
            false,
            Some(1784653027733.6138),
            0,
        );
        wedged.observed = Some(harness_state::Observed {
            state: harness_state::Activity::Idle,
            blocked_on: harness_state::BlockedOn::None,
            input_buffer: harness_state::InputBuffer::Empty,
            ask: harness_state::Ask::None,
            harness: Some("codex".to_string()),
            since_ms: Some(1784653000000),
            exit: None,
            reason: None,
        });

        assert_eq!(
            to_json(&[wedged.clone()], false),
            r#"[{"identity":"hetz.worker","status":"busy","name":null,"description":null,"retired":false,"resources":[],"desiredState":"running","desiredStateReason":null,"observedState":{"state":"idle","blockedOn":"none","inputBuffer":"empty","ask":"none","harness":"codex","since":1784653000000,"reason":null,"exit":null},"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.worker","address":"worker","busAddress":"hetz.worker"}]"#
        );
        assert_eq!(
            to_json(&[wedged], true),
            r#"[{"identity":"hetz.worker","status":"busy","name":null,"description":null,"retired":false,"resources":[],"lastActivity":1784653027733.6138,"inbox":0,"desiredState":"running","desiredStateReason":null,"observedState":{"state":"idle","blockedOn":"none","inputBuffer":"empty","ask":"none","harness":"codex","since":1784653000000,"reason":null,"exit":null},"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.worker","address":"worker","busAddress":"hetz.worker"}]"#
        );

        let mut derived = row("hetz.worker", State::Available, None, false, None, 0);
        derived.observed = Some(harness_state::Observed {
            state: harness_state::Activity::Unknown,
            blocked_on: harness_state::BlockedOn::Unknown,
            input_buffer: harness_state::InputBuffer::Unknown,
            ask: harness_state::Ask::Unknown,
            harness: Some("codex".to_string()),
            since_ms: None,
            exit: None,
            reason: Some("session-dead".to_string()),
        });
        assert_eq!(
            to_json(&[derived], false),
            r#"[{"identity":"hetz.worker","status":"available","name":null,"description":null,"retired":false,"resources":[],"desiredState":"running","desiredStateReason":null,"observedState":{"state":"unknown","blockedOn":"unknown","inputBuffer":"unknown","ask":"unknown","harness":"codex","since":null,"reason":"session-dead","exit":null},"driverDiagnostic":{"status":"absent","driver":null,"stage":null,"reason":null,"source":null,"producerVersion":null,"support":"unknown","observedAt":null,"evidenceAgeMs":null,"recovery":"publishFailureOrClearOnStageRecovery"},"context":null,"id":"hetz.worker","address":"worker","busAddress":"hetz.worker"}]"#
        );
    }

    /// HC-R14/HC-R07: `context` is a fourth top-level axis, always emitted and `null` when no
    /// record exists — and it survives an `observedState` of `unknown`, which is precisely the
    /// wedge case it exists for: a runtime at 190k of a 200k window whose state has gone
    /// indeterminate. Nothing here derives one axis from another.
    #[test]
    fn context_is_a_fourth_axis_that_survives_an_indeterminate_observed_state() {
        let mut wedged = row("hetz.worker", State::Busy, None, false, None, 0);
        wedged.observed = Some(harness_state::Observed {
            state: harness_state::Activity::Unknown,
            blocked_on: harness_state::BlockedOn::Unknown,
            input_buffer: harness_state::InputBuffer::Unknown,
            ask: harness_state::Ask::Unknown,
            harness: Some("codex".to_string()),
            since_ms: None,
            exit: None,
            reason: Some("session-dead".to_string()),
        });
        wedged.context = Some(harness_context::Observed {
            harness: harness_context::Harness::Codex,
            used_tokens: Some(92283),
            window_tokens: Some(258400),
            used_percent: Some(33.0),
            model: None,
            cost_usd: None,
            session_total_tokens: Some(2235329),
            rate_limits: harness_context::RateLimits {
                five_hour: Some(31.0),
                seven_day: Some(55.0),
            },
            compactions: 3,
            last_compaction_ms: Some(1788000097290),
            last_compaction_trigger: Some(harness_context::CompactionTrigger::Unknown),
            observed_at_ms: 1788000100000,
            age_ms: 4210,
            stale: false,
        });

        let wire: serde_json::Value =
            serde_json::from_str(&to_json(&[wedged.clone()], false)).unwrap();
        assert_eq!(
            wire[0]["context"],
            serde_json::json!({
                "harness": "codex",
                "usedTokens": 92283,
                "windowTokens": 258400,
                "usedPercent": 33.0,
                "model": null,
                "costUsd": null,
                "sessionTotalTokens": 2235329,
                "rateLimits": {"fiveHour": 31.0, "sevenDay": 55.0},
                "rateLimited": false,
                "compactions": 3,
                "lastCompactionMs": 1788000097290u64,
                "lastCompactionTrigger": "unknown",
                "observedAtMs": 1788000100000u64,
                "ageMs": 4210,
                "stale": false
            })
        );
        // The other three axes are untouched by it, and it by them.
        assert_eq!(wire[0]["status"], "busy");
        assert_eq!(wire[0]["observedState"]["state"], "unknown");
        assert_eq!(wire[0]["driverDiagnostic"]["status"], "absent");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&to_json(&[wedged], true)).unwrap()[0]["context"]
                ["usedPercent"],
            33.0
        );

        // A percent above the window rides the wire raw, and a stale reading keeps its age.
        let mut overrun = row("hetz.pi", State::Available, None, false, None, 0);
        overrun.context = Some(harness_context::Observed {
            harness: harness_context::Harness::Pi,
            used_tokens: Some(23424),
            window_tokens: Some(4000),
            used_percent: Some(585.6),
            model: Some("pi-model".to_string()),
            cost_usd: Some(0.42),
            session_total_tokens: None,
            rate_limits: harness_context::RateLimits::default(),
            compactions: 0,
            last_compaction_ms: None,
            last_compaction_trigger: None,
            observed_at_ms: 1788000100000,
            age_ms: 7_200_000,
            stale: true,
        });
        let wire: serde_json::Value = serde_json::from_str(&to_json(&[overrun], false)).unwrap();
        assert_eq!(wire[0]["context"]["usedPercent"], 585.6);
        assert_eq!(wire[0]["context"]["stale"], true);
        assert_eq!(wire[0]["context"]["ageMs"], 7_200_000);
        assert_eq!(
            wire[0]["context"]["lastCompactionTrigger"],
            serde_json::Value::Null
        );
        assert_eq!(
            wire[0]["context"]["rateLimits"]["fiveHour"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn exhausted_claude_rate_limit_is_explicit_beside_active_state() {
        let mut limited = row("hetz.worker", State::Available, None, false, None, 0);
        limited.observed = Some(harness_state::Observed {
            state: harness_state::Activity::Active,
            blocked_on: harness_state::BlockedOn::None,
            input_buffer: harness_state::InputBuffer::Unknown,
            ask: harness_state::Ask::None,
            harness: Some("claude".to_string()),
            since_ms: Some(1788000100000),
            exit: None,
            reason: None,
        });
        limited.context = Some(harness_context::Observed {
            harness: harness_context::Harness::Claude,
            used_tokens: Some(194_763),
            window_tokens: Some(1_000_000),
            used_percent: Some(19.0),
            model: Some("claude-opus-5".to_string()),
            cost_usd: Some(4.7312),
            session_total_tokens: None,
            rate_limits: harness_context::RateLimits {
                five_hour: Some(100.0),
                seven_day: Some(55.0),
            },
            compactions: 0,
            last_compaction_ms: None,
            last_compaction_trigger: None,
            observed_at_ms: 1788000100000,
            age_ms: 4210,
            stale: false,
        });

        let wire: serde_json::Value =
            serde_json::from_str(&to_json(&[limited.clone()], false)).unwrap();
        assert_eq!(wire[0]["observedState"]["state"], "active");
        assert_eq!(wire[0]["context"]["rateLimits"]["fiveHour"], 100.0);
        assert_eq!(wire[0]["context"]["rateLimited"], true);

        limited.context.as_mut().unwrap().stale = true;
        let stale: serde_json::Value = serde_json::from_str(&to_json(&[limited], false)).unwrap();
        assert_eq!(stale[0]["context"]["rateLimited"], false);
    }

    #[test]
    fn driver_diagnostic_wire_exposes_failure_and_evidence_age_without_identity_payloads() {
        let mut diagnosed = row("hetz.worker", State::Available, None, false, None, 0);
        diagnosed.driver_diagnostic =
            driver_diagnostic::Observed::Failure(driver_diagnostic::Failure {
                driver: driver_diagnostic::Driver::OpenCode,
                stage: driver_diagnostic::Stage::ReadBack,
                reason: driver_diagnostic::Reason::NotDurable,
                source: driver_diagnostic::Source::MessageReadBack,
                producer_version: Some("1.18.19".to_string()),
                support: driver_diagnostic::Support::Supported,
                observed_at: 100,
                evidence_age_ms: 25,
            });
        let wire: serde_json::Value = serde_json::from_str(&to_json(&[diagnosed], false)).unwrap();
        assert_eq!(
            wire[0]["driverDiagnostic"],
            serde_json::json!({
                "status": "failure",
                "driver": "opencode",
                "stage": "readBack",
                "reason": "notDurable",
                "source": "messageReadBack",
                "producerVersion": "1.18.19",
                "support": "supported",
                "observedAt": 100,
                "evidenceAgeMs": 25,
                "recovery": "clearsOnStageRecovery"
            })
        );
        let rendered = wire[0]["driverDiagnostic"].to_string();
        for forbidden in ["prompt", "body", "filename", "sessionId", "messageId"] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
    }
}
