use serde_json::{Value, json};
use crate::model::{ClaimRecord, SubjectStatus};
use crate::suspension::Suspension;

/// Read-only terminal evidence. An unknown actor/exit never implies success.
pub(super) fn project(subject: &SubjectStatus, suspension: Option<&Suspension>, desired: Option<&ClaimRecord>, retired: bool, updated_at: &str) -> Option<Value> {
    if subject.projection.layer != "current" { return None; }
    let fields = subject.actual.as_ref().map(|v| v.get("fields").unwrap_or(v));
    let actual = fields.and_then(|v| v.get("status")).and_then(Value::as_str);
    let stop = subject.kind.as_deref() == Some("stop");
    let suspended = suspension.filter(|s| s.action == "suspend" && s.phase == "suspended");
    let actor = suspended.and_then(|s| s.requested_by.as_deref()).or_else(|| stop.then(|| desired.and_then(|c| c.actor.as_deref())).flatten());
    let harness = subject.harness.as_ref().filter(|h| h.state == "ended");
    let raw_exit = harness.and_then(|h| h.exit.as_deref());
    let exit_code = fields.and_then(|v| v.get("exit_code")).and_then(Value::as_i64).or_else(|| raw_exit.and_then(|v| v.strip_prefix("exit ")).and_then(|v| v.parse::<i64>().ok()));
    let exit_signal = fields.and_then(|v| v.get("exit_signal")).filter(|v| !v.is_null()).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).or_else(|| raw_exit.and_then(|v| v.strip_prefix("signal ")).map(str::to_owned));
    let diagnostic = harness.and_then(|h| h.reason.as_deref());
    let kind = if retired && matches!(actual, Some("stopped" | "exited" | "absent" | "vanished")) { "retired" }
    else if stop || suspended.is_some() {
        match actor {
            Some(a) if a.starts_with("person/") => "stopped-by-person",
            Some(a) if a.starts_with("agent/") => "stopped-by-agent",
            _ => return None,
        }
    } else if subject.reachability == "unreachable" && matches!(actual, Some("running" | "ready" | "working" | "idle")) { "host-lost" }
    else if harness.is_some() && diagnostic.is_some() { "harness-exited" }
    else if (harness.is_some() || actual == Some("exited")) && (exit_signal.is_some() || exit_code.is_some_and(|code| code != 0)) { "crashed" }
    else if harness.is_some() && exit_code == Some(0) && exit_signal.is_none() && matches!(subject.reachability.as_str(), "reachable" | "local") { "completed" }
    else { return None; };
    let ended_at = suspended.map(|s| super::client_timestamp(s.suspended_at_unix_ms.unwrap_or(s.updated_at_unix_ms)))
        .or_else(|| harness.map(|h| super::client_timestamp(h.observed_at_unix_ms)))
        .or_else(|| desired.filter(|_| stop).map(|c| super::client_timestamp(c.accepted_at_unix_ms)))
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
        assert_eq!(project(&seat, None, None, false, "2026-10-04T00:00:00Z").unwrap()["kind"], "completed");
        seat.projection.layer = "historical".into();
        assert!(project(&seat, None, None, false, "2026-10-04T00:00:00Z").is_none());
        assert!(project(&subject(None), None, None, false, "2026-10-04T00:00:00Z").is_none());
        seat = subject(Some("exit 0"));
        seat.kind = Some("stop".into());
        assert!(project(&seat, None, None, false, "2026-10-04T00:00:00Z").is_none());
    }
    #[test]
    fn signal_and_diagnostic_never_become_completed() {
        let mut seat = subject(Some("signal 9"));
        let end = project(&seat, None, None, false, "2026-10-04T00:00:00Z").unwrap();
        assert_eq!(end["kind"], "crashed");
        assert_eq!(end["exit_signal"], "9");
        seat.harness.as_mut().unwrap().reason = Some("auth refused: exact provider text".into());
        let end = project(&seat, None, None, false, "2026-10-04T00:00:00Z").unwrap();
        assert_eq!(end["kind"], "harness-exited");
        assert_eq!(end["diagnostic"], "auth refused: exact provider text");
    }
    #[test]
    fn actor_stop_and_retirement_take_precedence_over_clean_exit() {
        let seat = subject(Some("exit 0"));
        for (actor, kind) in [("person/alice", "stopped-by-person"), ("agent/helper", "stopped-by-agent")] {
            let suspension = Suspension { action: "suspend".into(), phase: "suspended".into(), requested_by: Some(actor.into()), suspended_at_unix_ms: Some(1000), ..Suspension::default() };
            let end = project(&seat, Some(&suspension), None, false, "2026-10-04T00:00:00Z").unwrap();
            assert_eq!(end["kind"], kind);
            assert_eq!(end["actor"], actor);
        }
        assert_eq!(project(&seat, None, None, true, "2026-10-04T00:00:00Z").unwrap()["kind"], "retired");
    }
}
