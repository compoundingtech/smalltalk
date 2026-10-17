use serde_json::{Value, json};
use crate::model::{ClaimRecord, CurrentHarnessView, SubjectStatus};
use crate::suspension::Suspension;

/// Read-only terminal evidence. An unknown actor/exit never implies success.
pub(super) fn project(subject: &SubjectStatus, terminal_harness: Option<&CurrentHarnessView>, suspension: Option<&Suspension>, desired: Option<&ClaimRecord>, retired: bool, updated_at: &str) -> Option<Value> {
    let fields = subject.actual.as_ref().map(|v| v.get("fields").unwrap_or(v));
    let actual = fields.and_then(|v| v.get("status")).and_then(Value::as_str);
    let stop = subject.kind.as_deref() == Some("stop");
    let terminal_runtime = matches!(actual, Some("stopped" | "exited" | "absent" | "vanished"));
    // Historical stop declarations can explain an observed termination, but never imply success.
    if subject.projection.layer != "current" && !(stop && terminal_runtime) { return None; }
    let suspended = suspension.filter(|s| s.action == "suspend" && s.phase == "suspended");
    let actor = suspended.and_then(|s| s.requested_by.as_deref()).or_else(|| stop.then(|| desired.and_then(|c| c.actor.as_deref())).flatten());
    let harness = subject.harness.as_ref().or(terminal_harness).filter(|h| h.state == "ended");
    let raw_exit = harness.and_then(|h| h.exit.as_deref());
    let runtime_exit_code = fields.and_then(|v| v.get("exit_code")).and_then(Value::as_i64);
    let harness_exit_code = raw_exit.and_then(|v| v.strip_prefix("exit ")).and_then(|v| v.parse::<i64>().ok());
    let exit_code = harness_exit_code.or(runtime_exit_code);
    let exit_signal = raw_exit.and_then(|v| v.strip_prefix("signal ")).map(str::to_owned).or_else(|| fields.and_then(|v| v.get("exit_signal")).filter(|v| !v.is_null()).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())));
    let failed_exit = exit_signal.is_some() || harness_exit_code.is_some_and(|code| code != 0) || runtime_exit_code.is_some_and(|code| code != 0);
    let diagnostic = harness.and_then(|h| h.reason.as_deref());
    let kind = if retired && matches!(actual, Some("stopped" | "exited" | "absent" | "vanished")) { "retired" }
    else if (stop && (terminal_runtime || harness.is_some())) || suspended.is_some() {
        match actor {
            Some(a) if a.starts_with("person/") => "stopped-by-person",
            Some(a) if a.starts_with("agent/") => "stopped-by-agent",
            _ => return None,
        }
    } else if subject.reachability == "unreachable" && matches!(actual, Some("running" | "ready" | "working" | "idle")) { "host-lost" }
    else if harness.is_some() && diagnostic.is_some() { "harness-exited" }
    else if (harness.is_some() || actual == Some("exited")) && failed_exit { "crashed" }
    else if (harness.is_some() || actual == Some("exited")) && exit_code == Some(0) && !failed_exit && matches!(subject.reachability.as_str(), "reachable" | "local") { "completed" }
    else { return None; };
    let ended_at = suspended.map(|s| super::client_timestamp(s.suspended_at_unix_ms.unwrap_or(s.updated_at_unix_ms)))
        .or_else(|| harness.map(|h| super::client_timestamp(h.observed_at_unix_ms)))
        .unwrap_or_else(|| updated_at.to_owned());
    if ended_at.is_empty() { return None; }
    let mut result = json!({"kind": kind, "ended_at": ended_at});
    let object = result.as_object_mut().expect("object literal");
    if let Some(actor) = actor { object.insert("actor".into(), json!(actor)); }
    if let Some(code) = exit_code { object.insert("exit_code".into(), json!(code)); }
    if let Some(signal) = exit_signal { object.insert("exit_signal".into(), json!(signal)); }
    if let Some(diagnostic) = diagnostic { object.insert("diagnostic".into(), json!(diagnostic)); }
    if let Some(reason) = suspended.and_then(|s| s.reason.as_deref()) { object.insert("reason".into(), json!(reason)); }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn subject(exit: Option<&str>) -> SubjectStatus {
        serde_json::from_value(json!({
            "subject": "agent/example/seat", "kind": "agent", "desired_token": null,
            "desired_revision": null, "desired": {}, "actual": {"fields": {"status": "exited"}},
            "harness": {"state": "ended", "incarnation_id": "one", "claim": "claim/harness",
                "exit": exit, "observed_at_unix_ms": 1000},
            "conflicts": [], "claims": [], "owner_run": null, "gap": null,
            "reachability": "reachable", "reason": null
        })).unwrap()
    }
    #[test]
    fn completion_requires_explicit_clean_current_evidence() {
        let mut seat = subject(Some("exit 0"));
        assert_eq!(project(&seat, None, None, None, false, "2026-10-04T00:00:00Z").unwrap()["kind"], "completed");
        seat.projection.layer = "historical".into();
        assert!(project(&seat, None, None, None, false, "2026-10-04T00:00:00Z").is_none());
        assert!(project(&subject(None), None, None, None, false, "2026-10-04T00:00:00Z").is_none());
        seat = subject(Some("exit 0"));
        seat.kind = Some("stop".into());
        assert!(project(&seat, None, None, None, false, "2026-10-04T00:00:00Z").is_none());
    }
    #[test]
    fn signal_and_diagnostic_never_become_completed() {
        let mut seat = subject(Some("signal 9"));
        let end = project(&seat, None, None, None, false, "2026-10-04T00:00:00Z").unwrap();
        assert_eq!(end["kind"], "crashed");
        assert_eq!(end["exit_signal"], "9");
        seat.harness.as_mut().unwrap().reason = Some("auth refused: exact provider text".into());
        let end = project(&seat, None, None, None, false, "2026-10-04T00:00:00Z").unwrap();
        assert_eq!(end["kind"], "harness-exited");
        assert_eq!(end["diagnostic"], "auth refused: exact provider text");
    }
    #[test]
    fn actor_stop_and_retirement_take_precedence_over_clean_exit() {
        let seat = subject(Some("exit 0"));
        for (actor, kind) in [("person/operator", "stopped-by-person"), ("agent/helper", "stopped-by-agent")] {
            let suspension = Suspension { action: "suspend".into(), phase: "suspended".into(), requested_by: Some(actor.into()), suspended_at_unix_ms: Some(1000), ..Suspension::default() };
            let end = project(&seat, None, Some(&suspension), None, false, "2026-10-04T00:00:00Z").unwrap();
            assert_eq!(end["kind"], kind);
            assert_eq!(end["actor"], actor);
        }
        assert_eq!(project(&seat, None, None, None, true, "2026-10-04T00:00:00Z").unwrap()["kind"], "retired");
    }
    fn observe(store: &crate::store::Store, subject: &str, kind: &str, fields: Value) {
        store.append_claim(&crate::model::ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: None,
            fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
            expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    fn declare(store: &crate::store::Store, source: &str, key: &str, actor: &str) -> String {
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let subject = intent.subjects.keys().next().unwrap().clone();
        let preview = store.mission(&intent, crate::model::IntentInput { kdl: source.into(), source_name: None }).unwrap();
        store.apply_as(&intent, &preview.subject_tokens, key, Some(actor)).unwrap();
        subject
    }
    fn row(store: &crate::store::Store, subject: &str, snapshot: u64) -> Value {
        super::super::client_agent_resources(store, true, "end-reason-test", snapshot).unwrap()
            .into_iter().find(|row| row["id"] == subject).unwrap()
    }
    #[test]
    fn runtime_exit_retains_terminal_harness_evidence_at_its_snapshot() {
        let store = crate::store::Store::open_memory("node").unwrap();
        let id = declare(&store, "version 2\nagent \"end\" { workspace \"/tmp\"; harness \"omp\" {} }", "declare", "person/operator");
        observe(&store, &id, "runtime.observed", json!({"status":"running","incarnation_id":"one"}));
        observe(&store, &id, "harness.observed", json!({"state":"ended","incarnation_id":"one","exit":"exit 0","reason":null}));
        observe(&store, &id, "runtime.observed", json!({"status":"exited","incarnation_id":"one","exit_code":0}));
        let completed = store.index().unwrap();
        assert_eq!(row(&store, &id, completed)["end_reason"]["kind"], "completed");
        observe(&store, &id, "harness.observed", json!({"state":"ended","incarnation_id":"one","exit":"signal 9","reason":" exact diagnostic\nsecond line "}));
        let diagnostic = row(&store, &id, store.index().unwrap());
        assert_eq!(diagnostic["end_reason"]["kind"], "harness-exited");
        assert_eq!(diagnostic["end_reason"]["exit_signal"], "9");
        assert_eq!(diagnostic["end_reason"]["diagnostic"], " exact diagnostic\nsecond line ");
        assert_eq!(row(&store, &id, completed)["end_reason"]["kind"], "completed");
        observe(&store, &id, "runtime.observed", json!({"status":"running","incarnation_id":"two"}));
        assert!(row(&store, &id, store.index().unwrap())["end_reason"].is_null());
    }
    #[test]
    fn stop_intent_waits_for_observed_termination_and_survives_history() {
        let store = crate::store::Store::open_memory("node").unwrap();
        let id = declare(&store, "version 2\nagent \"end\" { command \"sleep 100\" }", "declare", "person/operator");
        observe(&store, &id, "runtime.observed", json!({"status":"running","incarnation_id":"one"}));
        declare(&store, &format!("version 2\nstop {id:?}"), "stop", "person/operator");
        assert!(row(&store, &id, store.index().unwrap())["end_reason"].is_null());
        observe(&store, &id, "runtime.observed", json!({"status":"stopped","incarnation_id":"one"}));
        let stopped = row(&store, &id, store.index().unwrap());
        assert_eq!(stopped["operational"]["layer"], "history");
        assert_eq!(stopped["end_reason"]["kind"], "stopped-by-person");
        assert_eq!(stopped["end_reason"]["actor"], "person/operator");
    }
    #[test]
    fn confirmed_one_shot_owned_retirement_survives_history() {
        use crate::store::owned_sets::{Options, Source};
        let store = crate::store::Store::open_memory("node").unwrap();
        let intent = crate::graph::parse_intent("version 2\nagent \"garden/end\" { command \"true\"; one-shot }", "node").unwrap();
        let mut options = Options {
            set: "garden".into(), source: Source { repository: "example/garden".into(), r#ref: "refs/heads/main".into(), sha: "1".repeat(40), sequence: 1 },
            expected_set: "absent".into(), rollout: None, adopt: Default::default(),
            allow_empty: false, confirm_retire: None, expected_subjects: Default::default(),
        };
        options.expected_subjects = store.owned_set_preview(&intent, &options).unwrap().expected_subjects;
        store.apply_owned_set(&intent, &options, "owned", "person/operator").unwrap();
        let id = "agent/garden/end";
        observe(&store, id, "runtime.observed", json!({"status":"running","incarnation_id":"one"}));
        declare(&store, &format!("version 2\nstop {id:?}"), "retire", "daemon/runtime");
        observe(&store, id, "runtime.observed", json!({"status":"exited","incarnation_id":"one","exit_code":0}));
        let retired = row(&store, id, store.index().unwrap());
        assert_eq!(retired["operational"]["layer"], "history");
        assert_eq!(retired["end_reason"]["kind"], "retired");
    }
    #[test]
    fn exact_provider_exit_wins_without_hiding_wrapper_failure() {
        let store = crate::store::Store::open_memory("node").unwrap();
        let id = declare(&store, "version 2\nagent \"end\" { workspace \"/tmp\"; harness \"omp\" {} }", "declare", "person/operator");
        for (provider, wrapper) in [(3, 1), (3, 0), (0, 1)] {
            observe(&store, &id, "harness.observed", json!({"state":"ended","incarnation_id":"one","exit":format!("exit {provider}"),"reason":null}));
            observe(&store, &id, "runtime.observed", json!({"status":"exited","incarnation_id":"one","exit_code":wrapper}));
            let result = row(&store, &id, store.index().unwrap());
            assert_eq!(result["end_reason"]["kind"], "crashed");
            assert_eq!(result["end_reason"]["exit_code"], provider);
        }
    }
    #[test]
    fn suspension_snapshot_and_failed_resume_preserve_original_actor() {
        let store = crate::store::Store::open_memory("node").unwrap();
        let id = declare(&store, "version 2\nagent \"end\" { command \"sleep 100\" }", "declare", "person/operator");
        observe(&store, &id, "runtime.observed", json!({"status":"running","incarnation_id":"one"}));
        let token = store.selected_desired_token(&id).unwrap().unwrap();
        let append = |kind: &str, actor: &str, fields: Value, evidence: Vec<String>, key: Option<String>| {
            store.append_claim(&crate::model::ClaimInput {
                subject: id.clone(), kind: kind.into(), actor: Some(actor.into()),
                fields: serde_json::from_value(fields).unwrap(), evidence,
                expected_subject: None, idempotency_key: key,
            }).unwrap()
        };
        let request = append("runtime.action.requested", "person/operator", json!({"action":"suspend","incarnation_id":"one"}), vec![token.clone()], None);
        let pending = store.index().unwrap();
        let completed = append("runtime.action.succeeded", "person/operator", json!({"action":"suspend","operation_status":"suspended"}), vec![request.id.clone()], Some(crate::suspension::suspend_completed_key(&request.id)));
        let suspended = store.index().unwrap();
        assert!(row(&store, &id, pending)["end_reason"].is_null());
        assert_eq!(row(&store, &id, suspended)["end_reason"]["actor"], "person/operator");
        let resume = append("runtime.action.requested", "agent/helper", json!({"action":"resume"}), vec![token, request.id], None);
        append("runtime.action.failed", "agent/helper", json!({"action":"resume","code":"native-resume-unavailable","reason":"resume failed"}), vec![resume.id.clone()], Some(crate::suspension::resume_failed_key(&resume.id)));
        let failed = row(&store, &id, store.index().unwrap());
        assert_eq!(failed["state"], "suspended");
        assert_eq!(failed["end_reason"]["kind"], "stopped-by-person");
        assert_eq!(failed["end_reason"]["actor"], "person/operator");
        assert_eq!(failed["end_reason"]["ended_at"], super::super::client_timestamp(completed.accepted_at_unix_ms));
        assert!(row(&store, &id, pending)["end_reason"].is_null());
        assert_eq!(row(&store, &id, suspended)["end_reason"]["actor"], "person/operator");
    }
}
