use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
struct Agents {
    seats: BTreeMap<String, Seat>,
    queues: BTreeMap<String, String>,
}

impl Agents {
    fn with(mut self, id: &str, state: &str, busy: Option<(&str, &str)>, queued: &[&str]) -> Self {
        self.seats.insert(
            id.into(),
            Seat {
                id: id.into(),
                label: id.trim_start_matches("agent/").into(),
                state: state.into(),
                fault: false,
                busy_with: busy.map(|(mission, path)| (mission.into(), path.into())),
            },
        );
        for step in queued {
            self.queues.insert((*step).into(), id.into());
        }
        self
    }
}

impl Fleet for Agents {
    fn seat(&self, agent: &str) -> Option<Seat> {
        self.seats.get(agent).cloned()
    }
    fn queued_for(&self, step: &str) -> Option<Seat> {
        self.queues.get(step).and_then(|agent| self.seat(agent))
    }
    fn mission_label(&self, mission: &str) -> String {
        mission.trim_start_matches("mission/").into()
    }
    fn person_label(&self, person: &str) -> String {
        person.trim_start_matches("person/").into()
    }
}

fn step(id: &str, state: &str, extra: Value) -> Value {
    let mut step = json!({
        "id": format!("step-run/audit/{id}"), "path": id, "state": state, "attempt": 1,
        "since": "2026-10-14T09:00:00Z",
    });
    step.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    step
}

fn mission(state: &str, steps: Vec<Value>) -> st3_client::Mission {
    serde_json::from_value(json!({
        "id": "mission/fleet/example/audit", "kind": "mission", "revision": "r1",
        "title": "fleet/example/audit", "updated_at": "2026-10-14T10:00:00Z", "state": state,
        "mission_revision": "r1", "runs": ["mission-run/audit/1"], "run_generations": {},
        "run_details": [{
            "id": "mission-run/audit/1", "requester": "person/avery", "status": state,
            "phase": "running", "progress": {"done": 0, "total": 0}, "current_steps": [],
            "must_act": "agent", "state_since": "2026-10-14T09:00:00Z", "steps": steps,
        }],
    }))
    .unwrap()
}

#[test]
fn a_gate_makes_a_person_decide_and_owns_its_step() {
    let mission = mission("running", vec![step("review", "verifying", json!({}))]);
    let gates = [Gate {
        mission: "mission/fleet/example/audit".into(),
        step: Some("step-run/audit/review".into()),
        person: "person/avery".into(),
    }];
    let shown = present(&mission, &gates, &Agents::default());
    assert_eq!(shown.word, Word::Decision);
    let avery = Actor::Person {
        person: "person/avery".into(),
        label: "avery".into(),
    };
    assert_eq!(shown.who, avery);
    assert_eq!(
        (shown.steps[0].word, &shown.steps[0].owner),
        (StepState::NeedsYou, &avery)
    );
    assert_eq!(shown.card, None);
    // Another mission's gate is not this one's.
    let other = [Gate {
        mission: "mission/other".into(),
        ..gates[0].clone()
    }];
    assert_ne!(
        present(&mission, &other, &Agents::default()).word,
        Word::Decision
    );
}

#[test]
fn a_ready_step_reads_from_the_queue_that_holds_it() {
    let ready = || {
        mission(
            "running",
            vec![step("build", "ready", json!({"assignee": "agent/keeper"}))],
        )
    };
    let unclaimed = present(&ready(), &[], &Agents::default());
    assert_eq!(
        (unclaimed.word, &unclaimed.who),
        (Word::Unclaimed, &Actor::Nobody)
    );

    let busy = Agents::default().with(
        "agent/keeper",
        "running",
        Some(("mission/fleet/example/ship", "deploy")),
        &["step-run/audit/build"],
    );
    let queued = present(&ready(), &[], &busy);
    assert_eq!(queued.word, Word::Queued);
    assert_eq!(queued.card, Some(Card::NothingToDo));
    assert!(
        queued.actions.is_empty(),
        "nothing to do while it waits its turn"
    );
    let behind = queued.queued_behind.unwrap();
    assert_eq!(
        behind.busy_with.unwrap().label,
        "fleet/example/ship › deploy"
    );
    assert_eq!(
        queued.steps[0].note.as_deref(),
        Some("queued for keeper, which is busy with fleet/example/ship › deploy")
    );

    let stopped =
        Agents::default().with("agent/keeper", "stopped", None, &["step-run/audit/build"]);
    let unstaffed = present(&ready(), &[], &stopped);
    assert_eq!(unstaffed.word, Word::Unstaffed);
    assert_eq!(unstaffed.card, Some(Card::WhatYouCanDo));
    assert_eq!(
        unstaffed.who,
        Actor::Agent {
            agent: "agent/keeper".into(),
            label: "keeper".into()
        },
        "the stuck step's seat, whatever it is doing"
    );
    let stuck = unstaffed.stuck.unwrap();
    assert_eq!(
        (stuck.word, stuck.agent.unwrap().state),
        (StepState::Ready, "stopped".into())
    );
    let offers = unstaffed
        .actions
        .iter()
        .map(|action| (action.id, action.action))
        .collect::<Vec<_>>();
    assert_eq!(
        offers,
        [
            (Offer::RestartAgent, "agent.start"),
            (Offer::Chat, "message.send"),
            (Offer::CancelRun, "mission.cancel")
        ]
    );
}

#[test]
fn a_failed_step_offers_a_retry_and_names_its_seat() {
    let mission = mission(
        "running",
        vec![
            step("scan", "completed", json!({"claimant": "agent/keeper"})),
            step(
                "fix",
                "failed",
                json!({"claimant": "agent/keeper", "attempt": 3, "blockers": ["tests fail"]}),
            ),
        ],
    );
    let agents = Agents::default().with("agent/keeper", "running", None, &[]);
    let shown = present(&mission, &[], &agents);
    assert_eq!(shown.word, Word::Failed);
    let stuck = shown.stuck.as_ref().unwrap();
    assert_eq!(
        (stuck.attempts, stuck.blockers.as_slice()),
        (3, ["tests fail".to_owned()].as_slice())
    );
    assert_eq!(
        shown
            .actions
            .iter()
            .map(|action| action.id)
            .collect::<Vec<_>>(),
        [Offer::RetryStep, Offer::Chat, Offer::CancelRun],
        "a running seat needs no restart"
    );
    assert!(
        shown
            .actions
            .iter()
            .find(|action| action.id == Offer::CancelRun)
            .unwrap()
            .confirm
    );
    assert_eq!(shown.progress, Progress { done: 1, total: 2 });
}

#[test]
fn working_names_who_holds_it_and_watching_is_st() {
    let working = mission(
        "running",
        vec![step(
            "build",
            "claimed",
            json!({"claimant": "agent/keeper", "last_progress": "Tests pass"}),
        )],
    );
    let shown = present(
        &working,
        &[],
        &Agents::default().with("agent/keeper", "running", None, &[]),
    );
    assert_eq!(shown.word, Word::Working);
    assert_eq!(
        shown.who,
        Actor::Agent {
            agent: "agent/keeper".into(),
            label: "keeper".into()
        }
    );
    assert_eq!(shown.last_progress.as_deref(), Some("Tests pass"));

    let watching = mission(
        "standing",
        vec![step(
            "keep-watch",
            "claimed",
            json!({"agentless": true, "keep_open": true}),
        )],
    );
    let shown = present(&watching, &[], &Agents::default());
    assert_eq!((shown.word, &shown.who), (Word::Watching, &Actor::St));
    assert!(shown.steps[0].keep_open);
}

#[test]
fn a_settled_mission_says_when_and_serializes_words_json_ids() {
    let done = mission("completed", vec![step("build", "completed", json!({}))]);
    let shown = present(&done, &[], &Agents::default());
    assert_eq!(shown.settled_at.as_deref(), Some("2026-10-14T10:00:00Z"));
    let wire = serde_json::to_value(&shown).unwrap();
    assert_eq!(wire["word"], "done");
    assert_eq!(wire["who"], json!({"kind": "nobody"}));
    assert_eq!(wire["steps"][0]["word"], "done");
    let words: Value =
        serde_json::from_str(include_str!("../../../../../fixtures/clients/words.json")).unwrap();
    let ids = words["mission"]
        .as_array()
        .unwrap()
        .iter()
        .map(|word| word["id"].clone())
        .collect::<Vec<_>>();
    for word in [
        Word::Decision,
        Word::Stalled,
        Word::Unstaffed,
        Word::Unclaimed,
        Word::Queued,
        Word::Working,
        Word::Watching,
        Word::Held,
        Word::Idle,
        Word::Done,
        Word::Failed,
        Word::Cancelled,
        Word::NotStarted,
    ] {
        assert!(
            ids.contains(&serde_json::to_value(word).unwrap()),
            "{word:?}"
        );
    }
}
