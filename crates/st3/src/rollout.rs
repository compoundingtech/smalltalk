//! Durable cutovers of owned native seats. Publication opts in; phases live in runtime claims.
use crate::model::{ClaimInput, DesiredSubject, MemberSpec};
use crate::store::{Store, owned_sets::Source};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
pub const OPERATION_ENV: &str = "ST3_ROLLOUT_OPERATION";
pub const PREDECESSOR_ENV: &str = "ST3_ROLLOUT_PREDECESSOR";
pub const RESUME_PATH_ENV: &str = "ST3_NATIVE_RESUME_PATH";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub mode: Mode,
    #[serde(default = "default_deadline")]
    pub deadline_ms: u64,
    #[serde(default)]
    pub force_after_deadline: bool,
}

/// Publication metadata, fenced by the selected desired claim.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeferredRestart {
    pub policy: Policy,
    pub deadline_unix_ms: u128,
}

pub fn deferred_restart(store: &Store, subject: &str) -> Result<Option<DeferredRestart>> {
    let Some(token) = store.selected_desired_token(subject)? else {
        return Ok(None);
    };
    let Some(claim) = store.claim_by_id(&token)? else {
        return Ok(None);
    };
    claim.body.get("deferred_restart").cloned()
        .map(serde_json::from_value).transpose().map_err(Into::into)
}

/// Native quiescence plus st-owned asks and deliveries. An ask_person parks its
/// origin step, releasing the claimant, so suspension blockers alone are insufficient.
pub(crate) fn restart_blockers(store: &Store, subject: &str, incarnation: &str) -> Result<Vec<String>> {
    let mut blockers = crate::suspension::blockers(store, subject, incarnation)?;
    if store.restart_person_work_pending(subject)? {
        blockers.push("pending-person-work".into());
    }
    if store.messages(Some(subject), false)?.iter().any(|message| {
        matches!(message.status.as_str(), "sent" | "staged" | "delivered")
    }) {
        blockers.push("pending-delivery".into());
    }
    blockers.sort();
    blockers.dedup();
    Ok(blockers)
}

pub fn deferred_restart_status(store: &Store, subject: &str) -> Result<Option<serde_json::Value>> {
    let Some(pending) = deferred_restart(store, subject)? else {
        return Ok(None);
    };
    let Some(runtime) = store.latest_observation(subject, "runtime.observed")? else {
        return Ok(None);
    };
    let fields = runtime.body.get("fields").unwrap_or(&runtime.body);
    if fields["status"] != "running" {
        return Ok(None);
    }
    let Some(incarnation) = fields["incarnation_id"].as_str() else {
        return Ok(None);
    };
    let Some((_, old)) = launched_member(store, subject, incarnation)? else {
        return Ok(None);
    };
    let Some((desired, _)) = store.desired_subject_with_writer(subject)? else {
        return Ok(None);
    };
    if desired.member.as_ref().is_none_or(|new| new.launch_changes(&old).is_empty()) {
        return Ok(None);
    }
    let cutover = store.restart_cutover(subject)?;
    let blocking = restart_blockers(store, subject, incarnation)?;
    let expired = smallclaims::store::now_ms() >= pending.deadline_unix_ms;
    Ok(Some(json!({
        "phase": if cutover { "stopping" } else if expired { "held" } else { "pending" },
        "mode": "when-idle",
        "kind": "apply-restart",
        "deadline_unix_ms": pending.deadline_unix_ms,
        "old_incarnation": incarnation,
        "blocking": blocking,
        "reason": if expired { "safe-point deadline reached; original seat keeps running; reapply with --restart-now to interrupt" } else { "launch change pending until idle with no open ask" },
    })))
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub set: String,
    pub receipt: String,
    pub source: Source,
    pub policy: Policy,
    pub manual: bool,
    pub desired_token: String,
    pub target: String,
    pub desired: DesiredSubject,
    pub actor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Operation {
    pub id: String,
    pub set: String,
    pub receipt: String,
    pub source: Source,
    pub desired_token: String,
    pub target: String,
    pub publication_policy: Policy,
    #[serde(default)]
    pub publication_manual: bool,
    pub policy: Policy,
    pub old_incarnation: String,
    pub old_member: MemberSpec,
    pub deadline_unix_ms: u128,
    pub requested_by: Option<String>,
    pub requested_at_unix_ms: u128,
    pub phase: String,
    pub phase_at_unix_ms: u128,
    pub drain_ack: Option<u128>,
    pub start_attempted: bool,
    pub allowed_work: BTreeMap<String, u32>,
    pub native_session_id: Option<String>,
    pub native_path: Option<String>,
    #[serde(default)]
    pub native_account: Option<String>,
    pub replacement_incarnation: Option<String>,
    pub forced: bool,
    pub blocking: Vec<String>,
    pub reason: Option<String>,
}
impl Operation {
    pub fn holds_intake(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "draining" | "stopping" | "starting" | "verifying" | "failed" | "blocked"
        )
    }
    pub fn holds_seat(&self) -> bool {
        self.holds_intake() || self.phase == "held"
    }
}

/// The authored policy stays in canonical declaration bytes and therefore in receipt digests.
pub fn manual(desired: &DesiredSubject) -> bool {
    desired.desired["children"]
        .as_array()
        .is_some_and(|children| {
            children
                .iter()
                .any(|child| child["name"] == "rollout" && child["arguments"][0] == "manual")
        })
}

/// Pending manual publication is informational: it holds neither messages nor work intake.
pub fn status(store: &Store, subject: &str) -> Result<Option<serde_json::Value>> {
    let (operation, selection) = store.rollout_and_selection(subject)?;
    if let Some(operation) = &operation
        && operation.phase != "superseded"
    {
        return Ok(Some(serde_json::to_value(operation)?));
    }
    let Some(selection) = selection else {
        return deferred_restart_status(store, subject);
    };
    if selection.manual && hold_render(store, &selection.desired)? {
        return Ok(Some(
            json!({"phase":"pending", "mode":"manual", "publication":"published",
            "status":"published, rollout pending (manual)", "set":selection.set,
            "receipt":selection.receipt, "desired_token":selection.desired_token}),
        ));
    }
    operation
        .map(serde_json::to_value)
        .transpose()
        .map_err(Into::into)
}

pub fn target(desired: &DesiredSubject) -> Result<String> {
    let launch = desired.member.as_ref().map(|m| json!({"host":m.host,"workspace":m.workspace,
        "cwd":m.cwd,"harness":m.driver,"terminal":m.terminal,"launch":crate::model::authored_launch(&m.launch)}));
    smallclaims::hash::canonical_hash(&(desired.kind.as_str(), launch))
}

pub fn phase(
    store: &Store,
    subject: &str,
    operation: &Operation,
    phase: &str,
    reason: Option<&str>,
    blocking: &[String],
) -> Result<()> {
    let mut state = serde_json::to_value(operation)?;
    state["reason"] = json!(reason);
    state["blocking"] = json!(blocking);
    store.append_claim(&ClaimInput {
        subject: subject.into(),
        kind: if matches!(phase, "failed" | "blocked") {
            "runtime.action.failed"
        } else if phase == "held" {
            "runtime.action.deadline-reached"
        } else {
            "runtime.action.succeeded"
        }
        .into(),
        actor: operation.requested_by.clone(),
        fields: BTreeMap::from([
            ("action".into(), json!("rollout")),
            ("operation".into(), json!(operation.id)),
            ("operation_status".into(), json!(phase)),
            ("rollout".into(), state),
        ]),
        evidence: vec![operation.id.clone()],
        expected_subject: None,
        idempotency_key: Some(format!("seat-rollout:{}:{phase}", operation.id)),
    })?;
    Ok(())
}

/// Before cutover begins, a changed launch must not rewrite the running seat's files.
pub fn hold_render(store: &Store, desired: &DesiredSubject) -> Result<bool> {
    if let Some(pending) = deferred_restart_status(store, &desired.subject)? {
        return Ok(pending["phase"] != "stopping");
    }
    let Some(selection) = store.rollout_selection(&desired.subject)? else {
        return Ok(false);
    };
    if let Some(operation) = store.rollout(&desired.subject)?
        && operation.phase != "superseded"
    {
        return Ok(!matches!(
            operation.phase.as_str(),
            "starting" | "verifying" | "running" | "retired"
        ));
    }
    let Some(actual) = store.latest_actual_value(&desired.subject)? else {
        return Ok(false);
    };
    let Some(incarnation) = actual["incarnation_id"].as_str() else {
        return Ok(false);
    };
    if actual["status"] != "running"
        && !(selection.manual
            && matches!(
                actual["status"].as_str(),
                Some("stopped" | "exited" | "vanished")
            ))
    {
        return Ok(false);
    }
    let old = launched_member(store, &desired.subject, incarnation)?;
    Ok(old.is_none_or(|(_, old)| {
        selection
            .desired
            .member
            .as_ref()
            .is_none_or(|m| !m.launch_changes(&old).is_empty())
    }))
}

pub fn launched_member(
    store: &Store,
    subject: &str,
    incarnation: &str,
) -> Result<Option<(String, MemberSpec)>> {
    for claim in store
        .observations_for(subject, "runtime.action.succeeded")?
        .into_iter()
        .rev()
    {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        if fields["action"] != "start" || fields["incarnation_id"].as_str() != Some(incarnation) {
            continue;
        }
        if let Some(token) = fields["desired_token"].as_str()
            && let Some(claim) = store.claim_by_id(token)?
            && let Ok(desired) = serde_json::from_value::<DesiredSubject>(claim.body)
            && let Some(member) = desired.member
        {
            return Ok(Some((token.into(), member)));
        }
    }
    Ok(None)
}

/// The same idle boundary is checked during drain, status reads and immediately before stop.
pub fn blockers(store: &Store, subject: &str, operation: &Operation) -> Result<Vec<String>> {
    let mut blockers = crate::suspension::blockers(store, subject, &operation.old_incarnation)?;
    if operation.drain_ack.is_none() {
        blockers.push("drain-unacknowledged".into());
    }
    if store.work_for_reconcile(subject)?.iter().any(|step| {
        matches!(step.status.as_str(), "waiting-person" | "ready")
            && operation.allowed_work.get(&step.subject) == Some(&step.attempt)
    }) {
        blockers.push("pending-person-work".into());
    }
    if store.messages(Some(subject), false)?.iter().any(|message| {
        matches!(message.status.as_str(), "sent" | "staged" | "delivered")
            && store.rollout_message_allowed(message).unwrap_or(true)
    }) {
        blockers.push("pending-delivery".into());
    }
    blockers.sort();
    blockers.dedup();
    Ok(blockers)
}

pub fn bound_account(store: &Store, subject: &str, incarnation: &str) -> Result<Option<String>> {
    Ok(store
        .claims_for(subject, Some("harness.session-file"))?
        .into_iter()
        .rev()
        .find(|claim| {
            claim
                .body
                .pointer("/fields/incarnation_id")
                .and_then(serde_json::Value::as_str)
                == Some(incarnation)
        })
        .and_then(|claim| {
            claim
                .body
                .pointer("/fields/account_ref")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }))
}

pub fn binding(
    store: &Store,
    subject: &str,
    incarnation: &str,
) -> Result<Option<(String, String, Option<String>)>> {
    Ok(store
        .claims_for(subject, Some("harness.session-file"))?
        .into_iter()
        .rev()
        .find_map(|claim| {
            let fields = &claim.body["fields"];
            if fields["incarnation_id"].as_str() != Some(incarnation) {
                return None;
            }
            Some((
                fields["harness"].as_str()?.into(),
                fields["session_id"].as_str()?.into(),
                fields["path"].as_str().map(str::to_owned),
            ))
        }))
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    WhenIdle,
}
fn default_deadline() -> u64 {
    30 * 60 * 1000
}
impl Policy {
    pub fn when_idle(deadline_ms: u64, force_after_deadline: bool) -> Self {
        Self {
            mode: Mode::WhenIdle,
            deadline_ms,
            force_after_deadline,
        }
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=7 * 24 * 60 * 60 * 1000).contains(&self.deadline_ms),
            "rollout deadline must be positive and at most seven days"
        );
        Ok(())
    }
}
