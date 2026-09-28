//! Human waits inside a Codex turn that has already compacted its context.

use serde_json::json;

use super::*;
use crate::harness_state::{Activity, BlockedOn};

fn status(flags: serde_json::Value) -> serde_json::Value {
    json!({
        "method": "thread/status/changed",
        "params": {
            "threadId": "thread-main",
            "status": { "type": "active", "activeFlags": flags }
        }
    })
}

/// Codex compacts automatically inside a running turn and then continues it. A command approval
/// that the same turn asks for afterwards is a person-wait and must be reported as one.
#[test]
#[ignore = "fails on main: after a mid-turn compaction an approval request reads as working, not blocked"]
fn an_approval_after_a_mid_turn_compaction_is_reported_as_a_human_wait() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();
    for method in ["item/started", "item/completed"] {
        state
            .observe(&json!({
                "method": method,
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-1",
                    "item": { "type": "contextCompaction", "id": "item-compact" }
                }
            }))
            .unwrap();
    }
    let compacted = state.observed().harness_observation().unwrap();
    assert_eq!(
        compacted.state,
        Activity::Active,
        "the turn is still running"
    );

    state
        .observe(&status(json!(["waitingOnApproval"])))
        .unwrap();
    let asking = state.observed().harness_observation().unwrap();
    assert_eq!(
        (asking.state, asking.blocked_on),
        (Activity::Active, BlockedOn::Human),
        "the turn is waiting on a person to approve a command, but reads {:?}",
        asking.reason
    );
}
