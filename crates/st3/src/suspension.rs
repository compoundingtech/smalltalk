//! Suspended seats: a running seat stops at a quiet boundary and later resumes its own native
//! session on the same host.
//!
//! A suspension is a pair of fenced runtime actions on the seat, like a restart. A person or
//! agent asks with `runtime.action.requested {action: suspend | resume}`; the seat's owner records
//! each phase as a `runtime.action.succeeded` or `runtime.action.failed` keyed by the request, with
//! the requester as actor so the phase replicates. The phase is derived from those claims:
//!
//! ```text
//! suspend: quiescing -> snapshotting -> suspended        (failed: the seat keeps running)
//! resume:  restoring -> verifying   -> resumed           (failed: the seat stays suspended)
//! ```
//!
//! The snapshot is the native session the seat's driver bound for the suspended incarnation, read
//! from its `harness.session-file` claim. Resume launches the driver with that session named in
//! [`RESUME_ENV`]; the driver relaunches the harness on exactly that session or exits with a typed
//! reason, and the resume completes only when the driver binds the same session again.
//!
//! Core never reads harness state itself. Whether a harness is quiet is the driver's report
//! (`quiescent` and `blocking` on `harness.observed`); core adds only st's own blockers.

use crate::model::ClaimRecord;
use crate::store::Store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The environment variable that names the native session a resumed driver must relaunch.
pub const RESUME_ENV: &str = "ST3_NATIVE_RESUME_SESSION";
/// The diagnostic code a driver records when it cannot relaunch the named native session.
pub const RESUME_UNAVAILABLE_CODE: &str = "native-resume-unavailable";
/// The environment variable that names the native session a relaunched driver continues when
/// it can. Unlike [`RESUME_ENV`], a driver that cannot continue it starts a new session.
pub const CONTINUE_ENV: &str = "ST3_NATIVE_CONTINUE_SESSION";
/// Where the harness kept that session, when its driver reported a path.
pub const CONTINUE_PATH_ENV: &str = "ST3_NATIVE_CONTINUE_PATH";
/// The diagnostic code a driver records when it starts a new session instead of continuing.
pub const CONTINUE_UNAVAILABLE_CODE: &str = "native-continue-unavailable";
/// How long a resumed driver may take to bind its native session before the resume fails.
pub const VERIFY_TIMEOUT_MS: u128 = 180_000;

pub fn suspend_failed_key(request: &str) -> String {
    format!("agent-suspend-failed:{request}")
}
pub fn suspend_snapshot_key(request: &str) -> String {
    format!("agent-suspend-snapshot:{request}")
}
pub fn suspend_completed_key(request: &str) -> String {
    format!("agent-suspend-completed:{request}")
}
pub fn resume_failed_key(request: &str) -> String {
    format!("agent-resume-failed:{request}")
}
pub fn resume_started_key(request: &str) -> String {
    format!("agent-resume-started:{request}")
}
pub fn resume_completed_key(request: &str) -> String {
    format!("agent-resume-completed:{request}")
}

/// Where a seat's latest suspend or resume stands.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Suspension {
    /// `suspend` or `resume`: the request this phase belongs to.
    pub action: String,
    /// `quiescing`, `snapshotting`, `suspended`, `failed` (a refused suspend), `restoring`,
    /// `verifying` or `resumed`.
    pub phase: String,
    /// The request claim; clients follow one operation by it.
    pub operation_id: String,
    /// Who asked. The owner records each phase as this actor, so the phase replicates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    /// The suspend request this state descends from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// The incarnation that was suspended, or the one a resume launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_at_unix_ms: Option<u128>,
    /// Why the last suspend or resume of this seat failed, as a stable code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What kept a refused suspend from finding the seat quiet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocking: Vec<String>,
    pub updated_at_unix_ms: u128,
    /// When the request this phase belongs to was made.
    #[serde(default)]
    pub requested_at_unix_ms: u128,
}

impl Suspension {
    /// The seat has no process of its own and the reconciler does not start one.
    pub fn holds_seat(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "quiescing" | "snapshotting" | "suspended" | "restoring" | "verifying"
        )
    }
}

fn field<'a>(claim: &'a ClaimRecord, name: &str) -> Option<&'a str> {
    claim
        .body
        .get("fields")
        .unwrap_or(&claim.body)
        .get(name)
        .and_then(Value::as_str)
}

fn action(claim: &ClaimRecord) -> Option<&str> {
    field(claim, "action")
}

fn evidence(claim: &ClaimRecord, index: usize) -> Option<&str> {
    claim
        .body
        .get("evidence")
        .and_then(|evidence| evidence.get(index))
        .and_then(Value::as_str)
}

/// The seat's requester-authored suspends and resumes, oldest first.
fn requests(store: &Store, subject: &str) -> Result<Vec<ClaimRecord>> {
    let mut requests = store.claims_for(subject, Some("runtime.action.requested"))?;
    requests.retain(|claim| {
        claim.actor.is_some() && matches!(action(claim), Some("suspend" | "resume"))
    });
    Ok(requests)
}

fn apply_failure(state: &mut Suspension, claim: &ClaimRecord) {
    state.code = field(claim, "code").map(str::to_owned);
    state.reason = field(claim, "reason").map(str::to_owned);
    state.blocking = claim
        .body
        .pointer("/fields/blocking")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    state.updated_at_unix_ms = claim.accepted_at_unix_ms;
}

/// The phase of the suspend request `request`.
fn suspend_state(store: &Store, request: &ClaimRecord) -> Result<Suspension> {
    let mut state = Suspension {
        action: "suspend".into(),
        phase: "quiescing".into(),
        operation_id: request.id.clone(),
        requested_by: request.actor.clone(),
        suspend_operation_id: Some(request.id.clone()),
        incarnation_id: field(request, "incarnation_id").map(str::to_owned),
        updated_at_unix_ms: request.accepted_at_unix_ms,
        requested_at_unix_ms: request.accepted_at_unix_ms,
        ..Suspension::default()
    };
    if let Some(failed) = store.operation_claim(&suspend_failed_key(&request.id))? {
        state.phase = "failed".into();
        apply_failure(&mut state, &failed);
        return Ok(state);
    }
    let snapshot = store.operation_claim(&suspend_snapshot_key(&request.id))?;
    if let Some(snapshot) = &snapshot {
        state.phase = "snapshotting".into();
        state.harness = field(snapshot, "harness").map(str::to_owned);
        state.native_session_id = field(snapshot, "native_session_id").map(str::to_owned);
        state.updated_at_unix_ms = snapshot.accepted_at_unix_ms;
    }
    if let Some(completed) = store.operation_claim(&suspend_completed_key(&request.id))? {
        state.phase = "suspended".into();
        state.suspended_at_unix_ms = Some(completed.accepted_at_unix_ms);
        state.updated_at_unix_ms = completed.accepted_at_unix_ms;
    }
    Ok(state)
}

/// Where the seat's latest suspend or resume stands, or `None` when it has none that still
/// applies. A suspension belongs to the launch it was taken under: a stop, or a declaration that
/// changes how the seat launches, ends it, and the seat then starts by the usual rules.
pub fn current(store: &Store, subject: &str) -> Result<Option<Suspension>> {
    let requests = requests(store, subject)?;
    let Some(request) = requests.last() else {
        return Ok(None);
    };
    if store.selected_desired_kind(subject)?.as_deref() != Some("agent") {
        return Ok(None);
    }
    let lineage = store.launch_lineage(subject)?;
    if !evidence(request, 0).is_some_and(|token| lineage.iter().any(|item| item == token)) {
        return Ok(None);
    }
    if action(request) == Some("suspend") {
        return suspend_state(store, request).map(Some);
    }
    // A resume names the suspend it resumes; without that suspension it has nothing to resume.
    let Some(suspend) =
        evidence(request, 1).and_then(|id| requests.iter().find(|claim| claim.id == id))
    else {
        return Ok(None);
    };
    let mut state = suspend_state(store, suspend)?;
    if state.phase != "suspended" {
        return Ok(None);
    }
    state.action = "resume".into();
    state.operation_id = request.id.clone();
    state.requested_by = request.actor.clone();
    state.updated_at_unix_ms = request.accepted_at_unix_ms;
    state.requested_at_unix_ms = request.accepted_at_unix_ms;
    state.phase = "restoring".into();
    if let Some(failed) = store.operation_claim(&resume_failed_key(&request.id))? {
        // A resume that fails leaves the seat suspended on the same snapshot, with the reason.
        state.phase = "suspended".into();
        apply_failure(&mut state, &failed);
        return Ok(Some(state));
    }
    if let Some(started) = store.operation_claim(&resume_started_key(&request.id))? {
        state.phase = "verifying".into();
        state.updated_at_unix_ms = started.accepted_at_unix_ms;
    }
    if let Some(completed) = store.operation_claim(&resume_completed_key(&request.id))? {
        state.phase = "resumed".into();
        state.incarnation_id = field(&completed, "incarnation_id").map(str::to_owned);
        state.updated_at_unix_ms = completed.accepted_at_unix_ms;
    }
    Ok(Some(state))
}

/// The native session the seat's driver bound for `incarnation`, with its harness.
pub fn bound_session(
    store: &Store,
    subject: &str,
    incarnation: &str,
) -> Result<Option<(String, String)>> {
    Ok(store
        .claims_for(subject, Some("harness.session-file"))?
        .into_iter()
        .rev()
        .find(|claim| field(claim, "incarnation_id") == Some(incarnation))
        .and_then(|claim| {
            Some((
                field(&claim, "harness")?.to_owned(),
                field(&claim, "session_id")?.to_owned(),
            ))
        }))
}

/// The key of the diagnostic a driver records once when it cannot continue `session`.
pub fn continue_unavailable_key(subject: &str, session: &str) -> String {
    format!("{CONTINUE_UNAVAILABLE_CODE}:{subject}:{session}")
}

/// The native session a relaunch of `subject` on `harness` continues, with its path: the last
/// one the seat's driver bound for that harness. A seat relaunched for a fresh context since,
/// or whose driver could not continue that session before, starts a new one.
pub fn continue_session(
    store: &Store,
    subject: &str,
    harness: &str,
    account: Option<&str>,
) -> Result<Option<(String, Option<String>)>> {
    let Some(bound) = store
        .claims_for(subject, Some("harness.session-file"))?
        .into_iter()
        .rev()
        .find(|claim| field(claim, "harness") == Some(harness))
    else {
        return Ok(None);
    };
    // Account directories also hold native sessions. Never carry a transcript between logins;
    // only a relaunch on the account that bound it may continue it.
    if field(&bound, "account_ref") != account {
        return Ok(None);
    }
    let Some(session) = field(&bound, "session_id").map(str::to_owned) else {
        return Ok(None);
    };
    let fresh_since = store
        .claims_for(subject, Some("runtime.action.requested"))?
        .iter()
        .any(|claim| {
            claim.store_index > bound.store_index && field(claim, "action") == Some("fresh-context")
        });
    let refused = store
        .operation_claim(&continue_unavailable_key(subject, &session))?
        .is_some();
    if fresh_since || refused {
        return Ok(None);
    }
    Ok(Some((session, field(&bound, "path").map(str::to_owned))))
}

/// Why `subject`'s running `incarnation` cannot be suspended now; empty when it can.
///
/// The harness's own state comes only from its driver's `quiescent` and `blocking` report. The
/// rest is st's: a claimed step whose lease would lapse, and subagents st still records.
pub fn blockers(store: &Store, subject: &str, incarnation: &str) -> Result<Vec<String>> {
    let mut blocking = Vec::new();
    let harness = store
        .latest_observation(subject, "harness.observed")?
        .filter(|claim| field(claim, "incarnation_id") == Some(incarnation));
    match harness {
        None => blocking.push("harness-unobserved".into()),
        Some(claim) => {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            // Older OMP drivers reported idle without observing their native jobs.
            // Their existing `true` is not proof under the upgraded report contract.
            if fields.get("driver").and_then(Value::as_str) == Some("omp")
                && fields.get("background_jobs").is_none()
            {
                blocking.push("background-jobs-unreported".into());
            }
            match fields.get("quiescent").and_then(Value::as_bool) {
                Some(true) => {}
                Some(false) => {
                    let reported = fields
                        .get("blocking")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    if reported.is_empty() {
                        blocking.push("harness-busy".into());
                    }
                    blocking.extend(reported);
                }
                None => blocking.push("quiescence-unreported".into()),
            }
        }
    }
    if store.work_for_reconcile(subject)?.iter().any(|step| {
        step.claimant.as_deref() == Some(subject)
            && matches!(step.status.as_str(), "claimed" | "working" | "verifying")
    }) {
        blocking.push("claimed-work".into());
    }
    if !store.open_subagents(subject)?.is_empty() {
        blocking.push("subagent-running".into());
    }
    if bound_session(store, subject, incarnation)?.is_none() {
        blocking.push("native-session-unbound".into());
    }
    blocking.sort();
    blocking.dedup();
    Ok(blocking)
}

/// The driver's half of quiescence: whether a harness in this state is at a clean boundary, and
/// the typed reasons it is not. `state`, `blocked_on`, `ask` and `input_buffer` are the values a
/// driver already publishes on `harness.observed`.
pub fn harness_quiescence(
    state: &str,
    blocked_on: &str,
    ask: &str,
    input_buffer: &str,
) -> (bool, Vec<&'static str>) {
    let mut blocking = Vec::new();
    match state {
        "idle" | "ready" => {}
        "working" => blocking.push("turn-in-flight"),
        "starting" => blocking.push("starting"),
        _ => blocking.push("harness-indeterminate"),
    }
    // An axis the harness cannot see (`unknown`) is not a blocker; a person's ask is.
    if blocked_on == "human" || !matches!(ask, "" | "none" | "unknown") {
        blocking.push("pending-ask");
    }
    if input_buffer == "nonempty" {
        blocking.push("unsent-input");
    }
    (blocking.is_empty(), blocking)
}

/// Add the driver's quiescence report to a `harness.observed` claim's fields, read from the
/// state the same claim already carries. Every driver publishes through this.
pub fn annotate_quiescence(fields: &mut std::collections::BTreeMap<String, Value>) {
    if let Ok(operation) = std::env::var(crate::rollout::OPERATION_ENV) {
        fields.insert("rollout_operation".into(), Value::String(operation));
    }
    let text = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (_, mut blocking) = harness_quiescence(
        &text("state"),
        &text("blocked_on"),
        &text("ask"),
        &text("input_buffer"),
    );
    if fields.get("driver").and_then(Value::as_str) == Some("omp") {
        match fields.get("background_jobs").and_then(Value::as_u64) {
            Some(0) => {}
            Some(_) => blocking.push("background-jobs-running"),
            None => blocking.push("background-jobs-unreported"),
        }
    }
    fields.insert("quiescent".into(), Value::Bool(blocking.is_empty()));
    fields.insert(
        "blocking".into(),
        Value::Array(blocking.into_iter().map(Value::from).collect()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_idle_harness_with_nothing_pending_is_quiescent() {
        assert_eq!(
            harness_quiescence("idle", "none", "none", "empty"),
            (true, vec![])
        );
        assert_eq!(harness_quiescence("ready", "", "", ""), (true, vec![]));
        assert_eq!(
            harness_quiescence("working", "none", "none", "empty"),
            (false, vec!["turn-in-flight"])
        );
        assert_eq!(
            harness_quiescence("idle", "human", "question", "nonempty"),
            (false, vec!["pending-ask", "unsent-input"])
        );
        assert_eq!(
            harness_quiescence("indeterminate", "none", "none", "empty"),
            (false, vec!["harness-indeterminate"])
        );
    }
}

#[cfg(test)]
mod background_job_tests {
    use super::*;

    #[test]
    fn legacy_omp_idle_is_unproven_until_the_upgraded_driver_reports_jobs() {
        let store = Store::open_memory("node").unwrap();
        for upgraded in [false, true] {
            let mut fields = std::collections::BTreeMap::from([
                ("driver".into(), Value::from("omp")),
                ("state".into(), Value::from("idle")),
                ("incarnation_id".into(), Value::from("fixture-one")),
                ("quiescent".into(), Value::from(true)),
            ]);
            if upgraded {
                fields.insert("background_jobs".into(), Value::from(0));
            }
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: "agent/example".into(),
                    kind: "harness.observed".into(),
                    actor: Some("agent/example".into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let blocking = blockers(&store, "agent/example", "fixture-one").unwrap();
            assert_eq!(
                blocking.iter().any(|reason| reason == "background-jobs-unreported"),
                !upgraded
            );
        }
    }

    #[test]
    fn omp_idle_requires_a_known_empty_native_job_snapshot() {
        for (jobs, reason) in [(Value::from(1), Some("background-jobs-running")), (Value::Null, Some("background-jobs-unreported")), (Value::from(0), None)] {
            let mut fields = std::collections::BTreeMap::from([
                ("driver".into(), Value::from("omp")), ("state".into(), Value::from("idle")), ("background_jobs".into(), jobs),
            ]);
            annotate_quiescence(&mut fields);
            assert_eq!(fields["quiescent"], reason.is_none());
            assert_eq!(fields["blocking"], reason.map_or_else(|| serde_json::json!([]), |reason| serde_json::json!([reason])));
        }
    }
}
