//! Pure client-v0 agent formatting from captured, complete dependency outputs.
//! No Store access or process/clock assessment occurs here.
use super::*;
use crate::model::{SubjectStatus, UsageSummary};
use crate::store::{AgentWorkQueue, StepLabel};
use anyhow::Context as _;

pub(crate) struct CardParts {
    pub subject: SubjectStatus,
    pub declaration: Option<Value>,
    pub actual_at: Option<u128>,
    pub last_activity_at: Option<u128>,
    pub working_since: Option<u128>,
    pub queue: AgentWorkQueue,
    pub labels: BTreeMap<String, StepLabel>,
    pub usage: Option<UsageSummary>,
    pub fault: Option<String>,
    pub handoff: Option<crate::placement::Handoff>,
    pub suspension: Option<crate::suspension::Suspension>,
    pub rollout: Option<Value>,
    pub todo: Option<ClaimRecord>,
    pub session: Option<ClaimRecord>,
    pub subagents: Vec<Value>,
    pub delivery: Option<(Value, bool)>,
}

pub(crate) struct FormattedCard {
    pub body: Value,
    pub current: bool,
    pub next_deadline: Option<u128>,
}

pub(crate) fn format(
    parts: CardParts,
    local_host: &str,
    now: u128,
) -> anyhow::Result<FormattedCard> {
    let CardParts {
        subject,
        declaration,
        actual_at,
        last_activity_at,
        working_since,
        queue,
        labels,
        usage,
        fault,
        handoff,
        suspension,
        rollout,
        todo,
        session,
        subagents,
        delivery,
    } = parts;
    let current = subject.projection.layer == "current";
    let label = |id: &String| {
        labels.get(id).map(|step| json!({
        "id":id, "mission_id":step.mission, "mission_run_id":step.run,
        "path":step.path, "title":step.title, "goal":step.goal,
        "state":client_work_state(&step.status), "since":client_timestamp(step.updated_at_unix_ms)
    }))
    };
    let fields = subject
        .actual
        .as_ref()
        .map(|actual| actual.get("fields").unwrap_or(actual));
    let observed = fields
        .and_then(|fields| fields.get("status"))
        .and_then(Value::as_str);
    let driver = subject
        .harness
        .as_ref()
        .and_then(|harness| harness.driver.clone())
        .or_else(|| subject.desired.as_ref().and_then(desired_harness_driver));
    let harness_state = subject
        .harness
        .as_ref()
        .map(|harness| harness.state.clone());
    let silent_since = if harness_state.as_deref() == Some("working") {
        let working_since =
            working_since.or_else(|| subject.harness.as_ref().map(|h| h.observed_at_unix_ms));
        match (last_activity_at, working_since) {
            (Some(activity), Some(start)) => Some(activity.max(start)),
            (Some(activity), None) => Some(activity),
            (None, start) => start,
        }
    } else {
        None
    };
    // A live wrapper is necessary but not sufficient for a running agent. Native
    // harnesses only become running once the current runtime incarnation has produced a
    // ready observation; an ended or indeterminate harness must never be painted green
    // merely because its wrapper process still has a running observation.
    let state = match (
        observed,
        driver.as_deref(),
        harness_state.as_deref(),
        subject.reachability.as_str(),
    ) {
        (Some("running" | "ready" | "working" | "idle"), _, _, reachability)
            if reachability != "reachable" =>
        {
            "waiting"
        }
        (
            Some("running" | "ready" | "working" | "idle"),
            Some(_),
            Some("ready" | "working" | "idle"),
            _,
        ) => {
            if subject
                .harness
                .as_ref()
                .is_some_and(|harness| harness.blocked_on.as_deref() == Some("human"))
            {
                "waiting"
            } else {
                "running"
            }
        }
        (Some("running" | "ready" | "working" | "idle"), Some(_), Some("ended" | "failed"), _) => {
            "failed"
        }
        // A harness fenced at a login or trust prompt waits on a person.
        (
            Some("running" | "ready" | "working" | "idle"),
            Some(_),
            Some("indeterminate" | "unknown" | "unauthenticated" | "needs-login" | "blocked"),
            _,
        ) => "waiting",
        (Some("running" | "ready" | "working" | "idle"), Some(_), _, _) => "starting",
        (Some("running" | "ready" | "working" | "idle"), None, _, _) => "running",
        (Some("starting" | "pending"), _, _, _) => "starting",
        (Some("waiting"), _, _, _) => "waiting",
        (Some("failed"), _, _, _) => "failed",
        (Some("stopped" | "exited" | "absent"), _, _, _) => "stopped",
        _ if subject.desired.is_some() => "desired",
        _ => "stopped",
    };
    let moving = handoff.as_ref().is_some_and(|h| h.phase != "running");
    let state = if fault.is_some() {
        "failed"
    } else if moving {
        "waiting"
    } else {
        state
    };
    // A suspended seat has no process by design: it is neither stopped nor failed.
    let state = match suspension.as_ref().map(|item| item.phase.as_str()) {
        Some("suspended") if fault.is_none() => "suspended",
        Some("snapshotting" | "fencing-source" | "transferring" | "restoring")
            if state == "stopped" =>
        {
            "suspended"
        }
        _ => state,
    };
    let runtime_id = fields
        .and_then(|fields| fields.get("runtime_id"))
        .and_then(Value::as_str);
    let runtime_ids = runtime_id
        .map(|runtime| vec![format!("runtime/{runtime}")])
        .unwrap_or_default();
    let incarnation_id = fields
        .filter(|_| !moving)
        .and_then(|fields| fields.get("incarnation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let current_session_id = incarnation_id
        .as_deref()
        .or(runtime_id)
        .map(|identity| managed_session_id(&subject.subject, identity));
    let updated_at = subject
        .harness
        .as_ref()
        .map(|harness| client_timestamp(harness.observed_at_unix_ms))
        .or_else(|| actual_at.map(client_timestamp))
        .unwrap_or_default();
    let name =
        crate::model::effective_agent_name(&subject.subject, subject.desired.as_ref()).to_owned();
    let revision = subject
        .desired_revision
        .clone()
        .or_else(|| subject.claims.last().cloned())
        .unwrap_or_else(|| format!("agent/{}", subject.subject));
    let mut value = json!({
        "id": subject.subject,
        "kind": "agent",
        "revision": revision,
        "updated_at": updated_at,
        "name": name,
        "state": state,
        "reachability": subject.reachability,
        "runtime_ids": runtime_ids,
        "owner_run_id": subject.owner_run,
        "driver": driver,
        "harness_state": harness_state.as_deref().map(|state| if state == "needs-login" { "unauthenticated" } else { state }),
        "harness_error_state": (harness_state.as_deref() == Some("needs-login")).then_some("needs-login"),
        "since": subject.harness.as_ref().map(|harness| client_timestamp(harness.since_unix_ms)),
                        "blocked_on": subject.harness.as_ref().and_then(|harness| harness.blocked_on.as_deref()),
        "ask": subject.harness.as_ref().and_then(|harness| harness.ask.as_deref()),
        "reason": subject.harness.as_ref().and_then(|harness| harness.reason.as_deref()),
        "host_id": declaration.as_ref().and_then(|d| d.get("host_id")),
        "workspace": declaration.as_ref().and_then(|d| d.get("workspace")),
        "checkout": declaration.as_ref().and_then(|d| d.get("checkout")),
        "last_activity_at": last_activity_at.map(client_timestamp),
        "silent_since": silent_since.map(client_timestamp),
        "fault": fault,
        "incarnation_id": incarnation_id,
        "current_session_id": current_session_id,
        "current_work_ids": queue.current_work_ids,
        "active_work_count": queue.active_work_count,
        "next_work_id": queue.next_work_id,
        "upcoming_work_ids": queue.upcoming_work_ids,
        "queued_work_count": queue.queued_work_count,
        "current_work": queue.current_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
        "next_work": queue.next_work_id.as_ref().and_then(label),
        "upcoming_work": queue.upcoming_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
        "usage": usage,
        "under": subject.under.into_iter().map(|relationship| json!({
            "agent_id": relationship.agent,
            "reason": relationship.reason
        })).collect::<Vec<_>>(),
        "operational": subject.projection,
        "suspension": suspension.as_ref().map(client_suspension),
        "handoff": handoff,
        "rollout": rollout,
    });

    value["todo"] = client_v0::agent_todo_value(
        todo.as_ref(),
        session.as_ref(),
        value["incarnation_id"].as_str(),
    );
    let (observation, next_deadline) = match subject.harness.as_ref() {
        None => ("missing", None),
        Some(h) if now.saturating_sub(h.observed_at_unix_ms) > 90_000 => ("stale", None),
        Some(h) => (
            "current",
            Some(h.observed_at_unix_ms.saturating_add(90_001)),
        ),
    };
    value["observation"] = json!(observation);
    if observation == "stale" && value["harness_state"] == "idle" {
        value["harness_state"] = json!("indeterminate");
        if value["state"] == "running" {
            value["state"] = json!("waiting");
        }
    }
    let native = value["driver"]
        .as_str()
        .is_some_and(|d| ["claude", "codex", "opencode", "pi", "omp"].contains(&d));
    let local = value["host_id"].as_str() == Some(local_host);
    let live = matches!(value["state"].as_str(), Some("running" | "waiting"));
    if native && local && live {
        let (assessment, stale) = delivery.context("local native delivery source missing")?;
        if stale {
            value["state"] = json!("waiting");
        }
        value["delivery"] = assessment;
    }
    value["subagents"] = Value::Array(subagents);
    Ok(FormattedCard {
        body: value,
        current,
        next_deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGENT: &str = "agent/node.amber";

    // Whole Store reads are an independent oracle only in these controls.
    fn captured(store: &Store) -> CardParts {
        let index = store.index().unwrap();
        let mut subject = store
            .agent_card_status_at(None, index, true)
            .unwrap()
            .subjects
            .into_iter()
            .find(|s| s.subject == AGENT)
            .unwrap();
        subject.harness = store.observed_harness_at(AGENT, index).unwrap();
        let declaration = store.desired_subjects_named(&[AGENT.into()]).unwrap().into_iter().next().and_then(|d| d.member.map(|m| {
            let checkout = crate::checkout::Checkout::from_desired(&d.desired).map(|c| json!({"repository":c.repository,"base":c.base,"branch":c.branch}));
            json!({"host_id":client_host_id(&m.host),"workspace":m.workspace,"checkout":checkout})
        }));
        let actual_at = subject
            .actual_claim
            .as_deref()
            .and_then(|id| store.claim_by_id(id).unwrap())
            .map(|c| c.accepted_at_unix_ms);
        let last_activity_at = store
            .agent_last_activity_at(
                AGENT,
                subject.harness.as_ref().map(|h| h.incarnation_id.as_str()),
                index,
            )
            .unwrap();
        let working_since = subject.harness.as_ref().and_then(|h| {
            store
                .agent_working_since(AGENT, &h.incarnation_id, index)
                .unwrap()
        });
        let queue = store
            .agent_work_queues()
            .unwrap()
            .remove(AGENT)
            .unwrap_or_default();
        let steps = queue
            .current_work_ids
            .iter()
            .chain(queue.next_work_id.iter())
            .chain(queue.upcoming_work_ids.iter())
            .cloned()
            .collect::<Vec<_>>();
        let labels = store.step_labels(&steps).unwrap();
        let handoff = subject
            .desired_token
            .as_deref()
            .and_then(|t| crate::placement::handoff(store, AGENT, t, index).unwrap());
        CardParts {
            subject,
            declaration,
            actual_at,
            last_activity_at,
            working_since,
            queue,
            labels,
            usage: store
                .usage_summaries_at(&[AGENT.into()], Some(index))
                .unwrap()
                .remove(AGENT),
            fault: store
                .member_reconcile_faults_for(&[AGENT.into()], index)
                .unwrap()
                .remove(AGENT),
            handoff,
            suspension: crate::suspension::current(store, AGENT).unwrap(),
            rollout: crate::rollout::status(store, AGENT).unwrap(),
            todo: None,
            session: None,
            subagents: vec![],
            delivery: None,
        }
    }

    fn append(store: &Store, kind: &str, fields: Value) {
        store
            .append_claim(&ClaimInput {
                subject: AGENT.into(),
                kind: kind.into(),
                actor: Some(AGENT.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    fn check(store: &Store) {
        let index = store.index().unwrap();
        let mut expected = client_agent_resources_uncached(store, true, index).unwrap();
        add_agent_todos(store, &mut expected, index).unwrap();
        overlay_agent_resources(store, &mut expected, "").unwrap();
        let actual = format(
            captured(store),
            &client_host_id(store.origin()),
            client_now_ms(),
        )
        .unwrap();
        assert_eq!(
            actual.body,
            expected.into_iter().find(|v| v["id"] == AGENT).unwrap()
        );
    }

    #[test]
    fn complete_public_formatter_matches_real_store_sparse_status_and_declaration() {
        let store = Store::open_memory("node").unwrap();
        let source = "version 2\nagent \"amber\" { command \"true\" }\n";
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let plan = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &plan.subject_tokens, "card-control")
            .unwrap();
        check(&store);
        append(
            &store,
            "runtime.observed",
            json!({"status":"running","runtime_id":"r","incarnation_id":"one"}),
        );
        append(
            &store,
            "harness.observed",
            json!({"state":"ready","driver":"invented","incarnation_id":"one"}),
        );
        check(&store);
        append(
            &store,
            "harness.observed",
            json!({"state":"blocked","blocked_on":"human","ask":"approve invented action","reason":"permission","incarnation_id":"one"}),
        );
        check(&store);
        append(
            &store,
            "harness.observed",
            json!({"state":"idle","blocked_on":null,"ask":null,"reason":null,"incarnation_id":"one"}),
        );
        check(&store);
        append(
            &store,
            "runtime.observed",
            json!({"status":"stopped","runtime_id":"r","incarnation_id":"one"}),
        );
        check(&store);
    }

    #[test]
    fn captured_clock_has_exact_strict_freshness_boundary_and_preserves_since() {
        let store = Store::open_memory("node").unwrap();
        append(
            &store,
            "runtime.observed",
            json!({"status":"running","runtime_id":"r","incarnation_id":"one"}),
        );
        append(
            &store,
            "harness.observed",
            json!({"state":"idle","driver":"invented","incarnation_id":"one"}),
        );
        let parts = captured(&store);
        let at = parts.subject.harness.as_ref().unwrap().observed_at_unix_ms;
        let fresh = format(captured(&store), "host/node", at + 90_000).unwrap();
        let stale = format(parts, "host/node", at + 90_001).unwrap();
        assert_eq!(fresh.next_deadline, Some(at + 90_001));
        assert_eq!(fresh.body["observation"], "current");
        assert_eq!(fresh.body["state"], "running");
        assert_eq!(stale.body["observation"], "stale");
        assert_eq!(stale.body["harness_state"], "indeterminate");
        assert_eq!(stale.body["state"], "waiting");
        assert_eq!(fresh.body["since"], stale.body["since"]);
    }
}
