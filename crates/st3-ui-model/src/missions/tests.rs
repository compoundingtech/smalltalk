use super::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

#[derive(Default)]
struct Inputs {
    missions: Vec<st3_client::Mission>,
    agents: Vec<st3_client::Agent>,
    attention: Vec<st3_client::Attention>,
}

fn window<T: DeserializeOwned>(items: Vec<Value>) -> Vec<T> {
    items
        .into_iter()
        .map(|item| serde_json::from_value(item).unwrap())
        .collect()
}

fn project(input: &Inputs) -> Vec<Mission> {
    adapt(
        input.missions.iter(),
        input.agents.iter(),
        input.attention.iter(),
        "2026-09-29T10:00:00Z",
        &Display {
            mission_label: &|mission| mission.header.id.clone(),
            agent_label: &|agent| agent.name.clone(),
            age_label: &|_, _| "2m ago".into(),
            clean_text: &str::to_owned,
        },
    )
}

#[test]
fn a_run_set_to_completed_reads_done_and_says_who_set_it_and_why() {
    let mut model = Inputs::default();
    model.missions = window(vec![serde_json::json!({
        "id": "mission/fleet/harbor/ship", "kind": "mission", "revision": "r1",
        "updated_at": "2026-09-29T09:58:00Z", "title": "fleet/harbor/ship",
        "state": "completed", "mission_revision": "r1",
        "runs": ["mission-run/ship-1"],
        "run_details": [{
            "id": "mission-run/ship-1", "requester": "person/avery", "status": "completed",
            "phase": "terminal", "progress": {"done": 1, "total": 2}, "current_steps": [],
            "must_act": "nobody", "state_since": "2026-09-29T09:58:00Z",
            "outcome": {
                "status": "completed", "previous_status": "failed",
                "reason": "the change merged after its gate was fixed",
                "actor": "person/avery", "at": "2026-09-29T09:58:00Z",
            },
            "steps": [{
                "id": "step-run/ship-1/gate", "path": "gate", "state": "failed", "attempt": 1,
                "since": "2026-09-29T09:50:00Z", "goals": [], "constraints": [], "blockers": [],
            }],
        }],
    })]);
    let missions = project(&model);
    assert_eq!(missions[0].word, Word::Done);
    let outcome = missions[0].outcome.as_deref().expect("the outcome shows");
    assert!(
        outcome.starts_with("completed (was failed) · set by person/avery ")
            && outcome.ends_with(" ago: the change merged after its gate was fixed"),
        "{outcome}"
    );
}

#[test]
fn a_mission_nobody_started_says_so_and_a_moving_step_drops_its_old_reason() {
    let mut model = Inputs::default();
    model.missions = window(vec![
        json!({
            "id": "mission/fleet/harbor/someday", "kind": "mission", "revision": "r",
            "updated_at": "2026-09-29T09:58:00Z", "title": "fleet/harbor/someday",
            "state": "ready", "mission_revision": "r", "runs": [], "run_details": [],
        }),
        json!({
            "id": "mission/fleet/harbor/gate", "kind": "mission", "revision": "r",
            "updated_at": "2026-09-29T09:58:00Z", "title": "fleet/harbor/gate",
            "state": "running", "mission_revision": "r", "runs": ["mission-run/gate-1"],
            "run_details": [{
                "id": "mission-run/gate-1", "requester": "person/avery", "status": "running",
                "phase": "normal", "progress": {"done": 0, "total": 1}, "current_steps": [],
                "must_act": "person", "state_since": "2026-09-29T09:58:00Z",
                "steps": [{
                    "id": "step-run/gate-1/answer", "path": "answer", "state": "working",
                    "attempt": 1, "assignee": null, "claimant": null, "agentless": true,
                    "since": "2026-09-29T09:58:00Z",
                    "blocked_reason": "the eligible agentless execution started",
                    "goals": [], "constraints": [], "blockers": [],
                }],
            }],
        }),
    ]);
    let missions = project(&model);
    let someday = missions
        .iter()
        .find(|mission| mission.id.ends_with("/someday"))
        .unwrap();
    assert_eq!(someday.word, Word::NotStarted);
    let gate = missions
        .iter()
        .find(|mission| mission.id.ends_with("/gate"))
        .unwrap();
    assert_eq!(gate.steps[0].note, None);
    assert_eq!(
        gate.steps[0].state,
        StepState::Working,
        "st calls it working"
    );

    // The same step, once st asks a person to answer it.
    model.attention = window(vec![json!({
        "id": "attention/gate", "kind": "attention", "revision": "r",
        "updated_at": "2026-09-29T09:58:00Z", "attention_kind": "human-gate",
        "source_id": "step-run/gate-1/answer", "person_id": "person/avery",
        "mission_id": "mission/fleet/harbor/gate", "title": "answer",
        "detail": "Which tide table?", "priority": "normal", "state": "open",
        "requested_at": "2026-09-29T09:58:00Z", "targets": [], "target_states": [],
    })]);
    let missions = project(&model);
    let gate = missions
        .iter()
        .find(|mission| mission.id.ends_with("/gate"))
        .unwrap();
    assert_eq!(gate.steps[0].state, StepState::NeedsYou);
    assert_eq!(gate.steps[0].owner.as_deref(), Some("you"));
}

#[test]
fn mission_steps_map_every_contract_state_and_held_work_is_working() {
    for (state, expected) in [
        ("waiting", StepState::Waiting),
        ("ready", StepState::Ready),
        ("claimed", StepState::Working),
        ("blocked", StepState::Waiting),
        ("verifying", StepState::Working),
        ("completed", StepState::Done),
        ("failed", StepState::Failed),
        ("cancelled", StepState::Cancelled),
        // Cached projections from older daemons keep their meaning.
        ("working", StepState::Working),
        ("pending", StepState::Waiting),
    ] {
        let mut model = Inputs::default();
        model.missions = window(vec![serde_json::json!({
            "id": "mission/fleet/harbor/build", "kind": "mission", "revision": "r1",
            "updated_at": "2026-09-29T09:58:00Z", "title": "fleet/harbor/build",
            "state": "running", "mission_revision": "r1", "runs": ["mission-run/build-1"],
            "run_details": [{
                "id": "mission-run/build-1", "requester": "person/avery", "status": "running",
                "phase": "normal", "progress": {"done": 0, "total": 2}, "current_steps": [],
                "must_act": "agent", "state_since": "2026-09-29T09:58:00Z",
                "steps": [
                    {"id": "step-run/build-1/build", "path": "build", "state": state, "attempt": 1,
                     "claimant": "agent/example/harbor/builder", "since": "2026-09-29T09:58:00Z"},
                    {"id": "step-run/build-1/deploy", "path": "deploy", "state": "waiting", "attempt": 0,
                     "since": "2026-09-29T09:58:00Z"}
                ],
            }],
        })]);
        let missions = project(&model);
        assert_eq!(missions[0].steps[0].state, expected, "{state}");
        if expected == StepState::Working {
            assert_eq!(missions[0].word, Word::Working, "{state}");
        }
    }
}

#[test]
fn missions_read_joined_steps_and_agent_queues() {
    let step = |id: &str, path: &str, state: &str, claimant: Option<&str>| {
        serde_json::json!({
            "id": id, "path": path, "state": state, "attempt": 1,
            "assignee": "agent/example/harbor/keeper", "claimant": claimant,
            "since": "2026-09-29T09:58:00Z", "goals": [format!("Do {path}.")],
            "constraints": [], "blockers": [],
            "blocked_reason": format!("previous reason for {path}"),
            "last_progress": format!("progress for {path}"),
        })
    };
    let label = |id: &str, path: &str, state: &str| {
        serde_json::json!({
            "id": id, "mission_id": "mission/fleet/harbor/audit",
            "mission_run_id": "mission-run/audit-1", "path": path,
            "title": null, "goal": format!("Do {path}."), "state": state,
            "since": "2026-09-29T09:58:00Z",
        })
    };
    let run = |id: &str, status: &str, steps: Vec<Value>| {
        serde_json::json!({
            "id": id, "requester": "person/avery", "status": status, "phase": "normal",
            "progress": {"done": 0, "total": steps.len()}, "current_steps": [],
            "must_act": "agent", "state_since": "2026-09-29T09:58:00Z", "steps": steps,
        })
    };
    let mut model = Inputs::default();
    model.missions = window(vec![serde_json::json!({
        "id": "mission/fleet/harbor/audit", "kind": "mission", "revision": "r1",
        "updated_at": "2026-09-29T09:58:00Z", "title": "fleet/harbor/audit",
        "state": "running", "mission_revision": "r1",
        "runs": ["mission-run/audit-0", "mission-run/audit-1"],
        "run_details": [
            // A finished earlier run whose steps must not mix with the open one.
            run("mission-run/audit-0", "completed", vec![step("step-run/old/scan", "scan", "completed", None)]),
            run("mission-run/audit-1", "running", vec![
                step("step-run/audit-1/scan", "scan", "claimed", Some("agent/example/harbor/keeper")),
                step("step-run/audit-1/report", "report", "ready", None),
            ]),
        ],
    })]);
    model.agents = window(vec![serde_json::json!({
        "id": "agent/example/harbor/keeper", "kind": "agent", "revision": "r2",
        "updated_at": "2026-09-29T09:59:00Z", "name": "fleet/harbor/keeper",
        "state": "running", "reachability": "local", "harness_state": "working",
        "host_id": "host/lighthouse", "runtime_ids": ["runtime/keeper"],
        "current_work_ids": ["step-run/audit-1/scan"],
        "next_work_id": "step-run/audit-1/report",
        "upcoming_work_ids": ["step-run/audit-1/report"], "queued_work_count": 1,
        "current_work": [label("step-run/audit-1/scan", "scan", "claimed")],
        "next_work": label("step-run/audit-1/report", "report", "ready"),
        "upcoming_work": [label("step-run/audit-1/report", "report", "ready")],
        "under": [],
    })]);
    let missions = project(&model);
    let mission = &missions[0];
    assert_eq!(mission.word, Word::Working);
    assert_eq!(
        mission
            .steps
            .iter()
            .map(|step| (step.name.as_str(), step.state))
            .collect::<Vec<_>>(),
        [("scan", StepState::Working), ("report", StepState::Ready)]
    );
    assert_eq!(mission.steps[0].goals, ["Do scan."]);
    assert_eq!(mission.agents, ["agent/example/harbor/keeper"]);
    assert_eq!(
        mission.steps[1].note.as_deref(),
        Some(
            "queued for fleet/harbor/keeper, which is busy with mission/fleet/harbor/audit › scan"
        )
    );
    let metadata = &mission.step_metadata["step-run/audit-1/report"];
    assert_eq!(
        metadata.blocked_reason.as_deref(),
        Some("previous reason for report")
    );
    assert_eq!(
        metadata.last_progress.as_deref(),
        Some("progress for report")
    );
}

fn run(id: &str, status: &str, steps: Vec<Value>) -> Value {
    json!({
        "id": id, "requester": "person/avery", "status": status, "phase": "normal",
        "progress": {"done": 0, "total": steps.len()}, "current_steps": [],
        "must_act": "agent", "state_since": "2026-09-29T09:58:00Z", "steps": steps,
    })
}

fn step(id: &str, state: &str, agentless: bool) -> Value {
    json!({
        "id": format!("step/{id}"), "path": id, "state": state,
        "attempt": 1, "agentless": agentless, "since": "2026-09-29T09:58:00Z",
    })
}

fn input(runs: Vec<Value>) -> Inputs {
    Inputs {
        missions: window(vec![json!({
            "id": "mission/audit", "kind": "mission", "revision": "r1", "title": "Audit",
            "updated_at": "2026-09-29T09:58:00Z", "state": "running", "mission_revision": "r1",
            "runs": runs.iter().map(|run| run["id"].clone()).collect::<Vec<_>>(),
            "run_details": runs,
        })]),
        ..Default::default()
    }
}

fn metadata_case(
    state: &str,
    reason: Option<&str>,
    progress: Option<&str>,
    expected_state: StepState,
    expected_word: Word,
    expected_note: Option<&str>,
) {
    let mut raw_step = step("assess", state, false);
    raw_step["id"] = json!("step-run/audit-1/assess");
    raw_step["blocked_reason"] = json!(reason);
    raw_step["last_progress"] = json!(progress);
    let run_state = match state {
        "failed" | "cancelled" => state,
        _ => "running",
    };
    let mut model = input(vec![run("mission-run/audit-1", run_state, vec![raw_step])]);
    model.missions[0].state = run_state.into();
    let missions = adapt(
        model.missions.iter(),
        model.agents.iter(),
        model.attention.iter(),
        "2026-09-29T10:00:00Z",
        &Display {
            mission_label: &|mission| mission.header.id.clone(),
            agent_label: &|agent| agent.name.clone(),
            age_label: &|_, _| "2m ago".into(),
            // Metadata must bypass even a display policy that rewrites text.
            clean_text: &|_| "normalized".into(),
        },
    );
    let mission = &missions[0];
    assert_eq!(mission.word, expected_word);
    assert_eq!(mission.steps[0].id, "step-run/audit-1/assess");
    assert_eq!(mission.steps[0].state, expected_state);
    assert_eq!(mission.steps[0].note.as_deref(), expected_note);
    assert_eq!(mission.step_metadata.len(), 1);
    let metadata = &mission.step_metadata[&mission.steps[0].id];
    assert_eq!(metadata.blocked_reason.as_deref(), reason);
    assert_eq!(metadata.last_progress.as_deref(), progress);
    // Older JSON consumers retain the existing step presentation; metadata is additive.
    let serialized = serde_json::to_value(mission).unwrap();
    assert_eq!(serialized["steps"][0]["note"], json!(expected_note));
    assert_eq!(
        serialized["step_metadata"]["step-run/audit-1/assess"],
        json!({"blocked_reason": reason, "last_progress": progress})
    );
}

#[test]
fn failed_retry_reason_is_retained_without_becoming_a_blocker() {
    metadata_case(
        "failed",
        Some("Retry after Cedar Q1"),
        None,
        StepState::Failed,
        Word::Failed,
        None,
    );
}

#[test]
fn cancelled_failure_reason_is_retained_without_becoming_a_blocker() {
    metadata_case(
        "cancelled",
        Some("another step failed"),
        None,
        StepState::Cancelled,
        Word::Cancelled,
        None,
    );
}

#[test]
fn working_progress_and_old_reason_are_retained_verbatim() {
    metadata_case(
        "working",
        Some("the eligible execution started"),
        Some("  Compared 12 rows.\nChecking the remaining rows.  "),
        StepState::Working,
        Word::Working,
        None,
    );
}

#[test]
fn waiting_blocker_remains_presented_and_retains_raw_metadata() {
    for (state, word) in [("waiting", Word::Held), ("blocked", Word::Stalled)] {
        metadata_case(
            state,
            Some("waiting on the harbor restart"),
            Some("the snapshot is ready"),
            StepState::Waiting,
            word,
            Some("waiting on the harbor restart"),
        );
    }
}

#[test]
fn all_open_runs_contribute_and_only_the_latest_finished_run_is_the_fallback() {
    let mut model = input(vec![
        run("old", "completed", vec![step("old", "completed", false)]),
        run("first", "running", vec![step("first", "claimed", false)]),
        run("second", "running", vec![step("second", "ready", false)]),
    ]);
    let missions = project(&model);
    assert_eq!(
        missions[0]
            .step_metadata
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["step/first", "step/second"]
    );
    assert_eq!(
        missions[0]
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    model.missions[0].run_details[1].status = "failed".into();
    model.missions[0].run_details[2].status = "completed".into();
    let missions = project(&model);
    assert_eq!(
        missions[0]
            .step_metadata
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["step/second"]
    );
    assert_eq!(
        missions[0]
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>(),
        ["second"]
    );
}

#[test]
fn only_agentless_keep_open_work_is_watching() {
    for (path, agentless, expected) in [
        ("keep-watch", true, Word::Watching),
        ("nested/seat-retirement", true, Word::Watching),
        ("keep-watch", false, Word::Working),
        ("command", true, Word::Working),
    ] {
        let model = input(vec![run(
            "open",
            "running",
            vec![
                step("done", "completed", false),
                step(path, "claimed", agentless),
            ],
        )]);
        assert_eq!(
            project(&model)[0].word,
            expected,
            "{path}, agentless={agentless}"
        );
    }
}

#[test]
fn ready_work_is_unclaimed_queued_or_unstaffed_from_the_agents_queue() {
    let mut model = input(vec![run(
        "open",
        "running",
        vec![step("ready", "ready", false)],
    )]);
    assert_eq!(project(&model)[0].word, Word::Unclaimed);
    model.agents = window(vec![json!({
        "id": "agent/keeper", "kind": "agent", "revision": "r1",
        "updated_at": "2026-09-29T09:58:00Z", "name": "Keeper", "state": "running",
        "reachability": "local", "harness_state": "idle", "under": [],
        "upcoming_work_ids": ["step/ready"],
    })]);
    assert_eq!(project(&model)[0].word, Word::Queued);
    model.agents[0].state = "stopped".into();
    assert_eq!(project(&model)[0].word, Word::Unstaffed);
    model.agents[0].state = "failed".into();
    assert_eq!(project(&model)[0].word, Word::Unstaffed);
}

#[test]
fn duplicate_paths_keep_run_identity_and_the_actual_seat_after_pipeline_sort() {
    let run = |id: &str, state: &str, claimant: Option<&str>, agentless: bool| {
        json!({
            "id": format!("mission-run/{id}"), "requester": "person/avery",
            "status": "running", "phase": "normal", "progress": {},
            "current_steps": [], "state_since": "2026-09-29T09:58:00Z",
            "must_act": "agent",
            "steps": [{
                "id": format!("step-run/{id}/build"), "path": "build", "state": state,
                "attempt": 1, "assignee": "agent/assigned", "claimant": claimant,
                "agentless": agentless, "since": "2026-09-29T09:58:00Z",
                "blocked_reason": format!("reason for {id}"),
                "last_progress": format!("progress for {id}"),
            }],
        })
    };
    let model = Inputs {
        missions: window(vec![json!({
            "id": "mission/build", "revision": "r", "updated_at": "2026-09-29T09:58:00Z",
            "title": "build", "state": "running", "mission_revision": "r",
            "runs": ["mission-run/a", "mission-run/b", "mission-run/c"],
            "run_details": [
                run("a", "waiting", None, false),
                run("b", "working", Some("agent/claimant"), false),
                run("c", "completed", Some("agent/obsolete"), true),
            ],
        })]),
        ..Inputs::default()
    };
    let missions = project(&model);
    let identities = missions[0]
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step.seat.as_deref(), step.state))
        .collect::<Vec<_>>();
    assert_eq!(
        identities,
        [
            ("step-run/c/build", None, StepState::Done),
            (
                "step-run/b/build",
                Some("agent/claimant"),
                StepState::Working
            ),
            (
                "step-run/a/build",
                Some("agent/assigned"),
                StepState::Waiting
            ),
        ]
    );
    for id in ["a", "b", "c"] {
        let metadata = &missions[0].step_metadata[&format!("step-run/{id}/build")];
        assert_eq!(metadata.blocked_reason, Some(format!("reason for {id}")));
        assert_eq!(metadata.last_progress, Some(format!("progress for {id}")));
    }
}
