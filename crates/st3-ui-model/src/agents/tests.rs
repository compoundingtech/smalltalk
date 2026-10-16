use super::*;
use serde_json::{Value, json};

/// Facts read from a whole agent row, as st reads them from the row it serves.
fn agent(fields: Value) -> Facts {
    let mut row = json!({
        "id": "agent/example/atlas", "kind": "agent", "revision": "r1",
        "updated_at": "2026-10-14T09:00:00Z", "name": "Atlas", "state": "running",
        "reachability": "local", "harness_state": "idle", "under": [],
        "usage": {"input_tokens": 1}, "subagents": [],
    });
    row.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    Facts::deserialize(&row).unwrap()
}

fn label(id: &str, path: &str) -> Value {
    json!({
        "id": id, "mission_id": "mission/fleet/example/audit",
        "mission_run_id": "mission-run/fleet/example/audit/1", "path": path,
        "title": "Audit the build", "goal": "Find the failing step.", "state": "claimed",
        "since": "2026-10-14T08:00:00Z",
    })
}

#[test]
fn states_follow_their_precedence() {
    for (fields, expected) in [
        (json!({"fault": "mailbox unreadable"}), State::Fault),
        (json!({"delivery": {"state": "stale"}}), State::Fault),
        (json!({"state": "failed"}), State::Fault),
        (
            json!({"state": "down", "down_since": "2026-10-14T08:00:00Z"}),
            State::Down,
        ),
        (
            json!({"state": "waiting", "harness_error_state": "needs-login"}),
            State::NeedsLogin,
        ),
        (
            json!({"state": "waiting", "harness_state": "unauthenticated"}),
            State::NeedsLogin,
        ),
        (
            json!({"state": "waiting", "reason": "providerAuth"}),
            State::NeedsLogin,
        ),
        (json!({"activity": "compacting"}), State::Compacting),
        (json!({"harness_state": "working"}), State::Working),
        (json!({}), State::Idle),
        (
            json!({"state": "waiting", "harness_state": "idle", "blocked_on": "human"}),
            State::NeedsYou,
        ),
        (
            json!({"state": "waiting", "harness_state": "blocked", "blocked_on": "channel"}),
            State::Fault,
        ),
        (
            json!({"state": "waiting", "harness_state": "blocked"}),
            State::NeedsYou,
        ),
        (
            json!({"state": "waiting", "harness_state": "indeterminate", "observation": "stale"}),
            State::Idle,
        ),
        (
            json!({"state": "waiting", "harness_state": "indeterminate", "observation": "stale", "reachability": "unreachable"}),
            State::Starting,
        ),
        (json!({"state": "desired"}), State::Starting),
        (json!({"state": "stopped"}), State::Stopped),
        (json!({"state": "retired"}), State::Retired),
        // Retired is final, whatever else st still says of the seat.
        (
            json!({"state": "retired", "fault": "its runtime failed"}),
            State::Retired,
        ),
    ] {
        assert_eq!(state(&agent(fields.clone())), expected, "{fields}");
    }
}

#[test]
fn every_state_and_group_is_a_words_json_id() {
    let words: Value =
        serde_json::from_str(include_str!("../../../../fixtures/clients/words.json")).unwrap();
    let ids = words["agent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|word| word["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let served = State::ALL
        .iter()
        .map(|state| {
            serde_json::to_value(state)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(served, ids, "the states, in words.json order");
    for state in State::ALL {
        let group = serde_json::to_value(state.group()).unwrap();
        assert!(ids.contains(&group.as_str().unwrap().to_owned()), "{group}");
    }
}

#[test]
fn presentation_names_why_now_and_next() {
    let busy = present(&agent(json!({
        "harness_state": "working",
        "current_work": [label("step-run/a", "build")],
        "next_work": label("step-run/b", "test"),
        "upcoming_work": [label("step-run/b", "test"), label("step-run/c", "ship")],
        "queued_work_count": 2,
    })));
    assert_eq!(
        (busy.state, busy.group, busy.because.as_deref()),
        (State::Working, Group::Working, None)
    );
    let now = busy.now.unwrap();
    assert_eq!(now.work.label, "fleet/example/audit › build");
    assert_eq!(now.goal.as_deref(), Some("Find the failing step."));
    assert_eq!(now.since, "2026-10-14T08:00:00Z");
    assert_eq!(
        busy.next
            .iter()
            .map(|work| work.path.as_str())
            .collect::<Vec<_>>(),
        ["test", "ship"],
        "next first, never twice"
    );
    assert_eq!(busy.queued, 2);

    let asking = present(&agent(json!({
        "state": "waiting", "harness_state": "blocked", "ask": "Approve the deploy?",
    })));
    assert_eq!(asking.because.as_deref(), Some("Approve the deploy?"));
    let broken = present(&agent(json!({"delivery": {"state": "stale"}})));
    assert_eq!(
        (broken.group, broken.because.as_deref()),
        (
            Group::Fault,
            Some("Its message path stopped taking messages")
        )
    );
    let down = present(&agent(
        json!({"state": "down", "down_since": "2026-10-14T08:00:00Z"}),
    ));
    assert_eq!(down.group, Group::Fault);
    assert_eq!(
        down.because.as_deref(),
        Some("No running session since 2026-10-14T08:00:00Z")
    );
    let unattached = present(&agent(json!({
        "state": "waiting", "harness_state": "blocked", "blocked_on": "channel",
        "reason": "claude-channel-unattached",
    })));
    assert_eq!(
        (unattached.state, unattached.because.as_deref()),
        (
            State::Fault,
            Some("Its st channel is not attached, so its mail is held (claude-channel-unattached)")
        )
    );
    assert!(present(&agent(json!({}))).now.is_none());
}

#[test]
fn a_label_is_the_last_segment_in_title_case_without_its_host() {
    assert_eq!(
        super::label("fleet/example/image-sorter", None),
        "Image Sorter"
    );
    assert_eq!(
        super::label("harbor.image-sorter", Some("host/harbor")),
        "Image Sorter"
    );
    // Another host's prefix stays: it is part of the name.
    assert_eq!(
        super::label("harbor.image-sorter", Some("host/dock")),
        "Harbor.image Sorter"
    );
    assert_eq!(super::label("fleet/st3-cos-ios", None), "ST COS iOS");
    assert_eq!(
        super::label("fleet/release-captain/omp", None),
        "Release Captain · OMP"
    );
    assert_eq!(super::label("Initial seat", None), "Initial seat");
}
