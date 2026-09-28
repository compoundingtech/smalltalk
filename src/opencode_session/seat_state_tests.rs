//! Seat-level state across an OpenCode parent session and the child sessions its subagents run.

use serde_json::Value;

use super::EventMachine;
use crate::harness_state::{Activity, BlockedOn};

fn apply(machine: &mut EventMachine, raw: &str) {
    let event: Value = serde_json::from_str(raw).unwrap();
    machine.apply(&event);
}

fn status(session: &str, word: &str) -> String {
    format!(
        r#"{{"type":"session.status","properties":{{"sessionID":"{session}","status":{{"type":"{word}"}}}}}}"#
    )
}

/// A subagent's child session starts and finishes inside the parent's turn. The child going idle
/// is not the parent going idle, and a background child that outlives the parent's turn keeps the
/// seat working until it, too, is idle.
#[test]
fn a_child_session_finishing_never_idles_a_parent_that_is_still_busy() {
    let mut machine = EventMachine::default();
    apply(&mut machine, &status("ses_parent", "busy"));
    apply(&mut machine, &status("ses_child", "busy"));
    apply(&mut machine, &status("ses_child", "idle"));
    apply(
        &mut machine,
        r#"{"type":"session.idle","properties":{"sessionID":"ses_child"}}"#,
    );
    assert_eq!(machine.observation().unwrap().state, Activity::Active);

    apply(&mut machine, &status("ses_background", "busy"));
    apply(&mut machine, &status("ses_parent", "idle"));
    assert_eq!(
        machine.observation().unwrap().state,
        Activity::Active,
        "a background child the turn started is still running"
    );
    apply(&mut machine, &status("ses_background", "idle"));
    let idle = machine.observation().unwrap();
    assert_eq!(
        (idle.state, idle.blocked_on),
        (Activity::Idle, BlockedOn::None)
    );
}

/// A child session's permission prompt holds the seat on a human, and a rejection releases it; the
/// parent's turn then ends idle, not blocked.
#[test]
fn a_rejected_child_permission_releases_the_seat_and_the_turn_ends_idle() {
    let mut machine = EventMachine::default();
    apply(&mut machine, &status("ses_parent", "busy"));
    apply(&mut machine, &status("ses_child", "busy"));
    apply(
        &mut machine,
        r#"{"type":"permission.asked","properties":{"id":"per_child","sessionID":"ses_child","permission":"bash"}}"#,
    );
    assert_eq!(machine.observation().unwrap().blocked_on, BlockedOn::Human);
    apply(
        &mut machine,
        r#"{"type":"permission.replied","properties":{"sessionID":"ses_child","requestID":"per_child","reply":"reject"}}"#,
    );
    assert_eq!(machine.observation().unwrap().blocked_on, BlockedOn::None);
    apply(&mut machine, &status("ses_child", "idle"));
    apply(&mut machine, &status("ses_parent", "idle"));
    let idle = machine.observation().unwrap();
    assert_eq!(
        (idle.state, idle.blocked_on),
        (Activity::Idle, BlockedOn::None)
    );
}

/// A subagent's child session fails, and the parent recovers and finishes its own turn. That turn
/// ended in success, so the seat's turn end must not carry the child's error.
#[test]
#[ignore = "fails on main: a child session's error labels the parent's successful turn end as an error"]
fn a_child_sessions_error_does_not_label_the_parents_successful_turn_end() {
    let mut machine = EventMachine::default();
    apply(&mut machine, &status("ses_parent", "busy"));
    apply(&mut machine, &status("ses_child", "busy"));
    apply(
        &mut machine,
        r#"{"type":"session.error","properties":{"sessionID":"ses_child","error":{"name":"APIError"}}}"#,
    );
    assert_eq!(machine.observation().unwrap().state, Activity::Active);
    apply(&mut machine, &status("ses_parent", "idle"));
    let finished = machine.observation().unwrap();
    assert_eq!(finished.state, Activity::Idle);
    assert_eq!(
        finished.reason, None,
        "the parent's turn succeeded, but its end reads as {:?}",
        finished.reason
    );
}
