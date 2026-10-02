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
