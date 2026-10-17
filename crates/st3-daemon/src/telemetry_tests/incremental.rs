//! The incremental pass's tests that need the store, the graph or the model's request types.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::graph;
use crate::incremental::*;
use crate::store::Store;

fn change(subject: &str, kind: &str, actor: Option<&str>, body: &str) -> Change {
    Change {
        subject: subject.into(),
        kind: kind.into(),
        actor: actor.map(Into::into),
        body: body.into(),
    }
}

fn observe(store: &Store, resource: &str) {
    store
        .append_claim(&crate::model::ClaimInput {
            subject: format!("resource/{resource}"),
            kind: "resource.observed".into(),
            actor: None,
            fields: std::collections::BTreeMap::from([(
                "kind".into(),
                Value::String("custom.st3.test".into()),
            )]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(resource.into()),
        })
        .unwrap();
}

/// A correction names what the item wrote: a claim another thread wrote meanwhile, which a
/// change feed read would include, is not its write.
#[test]
fn a_recording_lists_only_its_own_writes() {
    let store = std::sync::Arc::new(Store::open_memory("node").unwrap());
    let other = store.clone();
    let ((), wrote) = smallclaims::touched::record_wrote(|| {
        observe(&store, "own");
        std::thread::spawn(move || observe(&other, "other"))
            .join()
            .unwrap();
    });
    assert_eq!(wrote, ["resource.observed resource/own"]);
    assert!(
        smallclaims::touched::wrote_since(0).is_empty(),
        "nothing records outside it"
    );
}

#[test]
fn a_change_marks_only_the_items_that_read_it() {
    let retirement = change_keys(&change("agent/a", "agent.retired", None, "{}"));
    assert!(retirement.contains(&"kind:intent.desired".to_owned()));
    assert!(retirement.contains(&"desired-kind:agent".to_owned()));
    let incremental = Incremental::default();
    incremental.evaluated(
        "mission-run/a",
        BTreeSet::from(["step-run/a/1".to_owned()]),
        None,
    );
    incremental.evaluated(
        "mission-run/b",
        BTreeSet::from(["mailbox:agent/x".to_owned()]),
        Some(100),
    );
    assert!(!incremental.needs("mission-run/a", 0));
    assert!(!incremental.needs("mission-run/b", 50));
    assert!(incremental.needs("mission-run/b", 100), "its time came");
    assert!(incremental.needs("mission-run/c", 0), "a new item");
    let keys = change_keys(&change(
        "message/m",
        "message.sent",
        Some("person/example"),
        r#"{"fields":{"to":"agent/x"}}"#,
    ));
    assert!(keys.contains(&"mailbox:agent/x".to_owned()));
    assert!(keys.contains(&"actor:person/example".to_owned()));
    assert!(keys.contains(&"kind:message.sent".to_owned()));
}

#[test]
fn a_subscription_start_wakes_only_its_run() {
    let store = Store::open_memory("orchid").unwrap();
    let incremental = Incremental::default();
    incremental.observe(&store).unwrap();
    for run in ["a", "b"] {
        incremental.evaluated(
            &format!("mission-run/{run}"),
            BTreeSet::from([format!("subscription-run:mission-run/{run}")]),
            None,
        );
    }
    store
        .append_claim(&crate::model::ClaimInput {
            subject: "subscription/reviews".into(),
            kind: "subscription.mission-started".into(),
            actor: None,
            fields: std::collections::BTreeMap::from([
                ("mission_run".into(), Value::String("mission-run/a".into())),
                ("request".into(), Value::String("a".repeat(64))),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    incremental.observe(&store).unwrap();
    assert!(incremental.needs("mission-run/a", 0));
    assert!(!incremental.needs("mission-run/b", 0));
}

fn work_fixture() -> Store {
    let store = Store::open_memory("orchid").unwrap();
    let intent = graph::parse_internal_intent(
        "version 2\nagent \"example/alder\" { workspace \"/tmp\"; command \"true\" }\nagent \"example/birch\" { workspace \"/tmp\"; command \"true\" }\nmission \"new-work\" state=\"ready\" { goal \"Check work membership\"; step \"work\" { assigned-to \"agent/example/alder\"; goal \"Inspect\"; } }\nmission \"offered-work\" state=\"ready\" { goal \"Offer work\"; step \"work\" { available-to \"agent/example/alder\"; available-to \"agent/example/birch\"; goal \"Inspect\"; } }",
        "orchid",
    )
    .unwrap();
    store
        .apply_internal(&intent, "work-dependency-fixture")
        .unwrap();
    store
}

fn start_work(store: &Store, actor: &str, key: &str) -> crate::model::StepRunView {
    store
        .start_work(&crate::model::WorkStartRequest {
            actor: actor.into(),
            title: "Inspect a fixture".into(),
            idempotency_key: key.into(),
        })
        .unwrap()
}

fn settle_work(incremental: &Incremental, store: &Store, actor: &str) {
    let (_, reads) = smallclaims::touched::record(|| store.work_for_reconcile(actor).unwrap());
    incremental.evaluated(actor, reads, None);
    assert!(!incremental.needs(actor, 0));
}

#[test]
fn work_state_invalidates_its_seat_without_broadcasting_to_unrelated_work() {
    let store = work_fixture();
    let alder = "agent/example/alder";
    let birch = "agent/example/birch";
    start_work(&store, alder, "alder-work");
    let changed = start_work(&store, birch, "birch-work");
    let incremental = Incremental::default();
    incremental.observe(&store).unwrap();
    settle_work(&incremental, &store, alder);
    settle_work(&incremental, &store, birch);
    assert!(
        store
            .set_step_state(&changed.subject, "completed", None)
            .unwrap()
    );
    incremental.observe(&store).unwrap();
    assert!(
        incremental.needs(birch, 0),
        "the step's reader must reevaluate"
    );
    assert!(!incremental.needs(alder, 0), "unrelated work remains clean");
}

#[test]
fn work_revision_approval_and_cancellation_invalidate_phase_visibility() {
    let store = work_fixture();
    let alder = "agent/example/alder";
    let publish = |goal: &str, key: &str| {
        let source = format!(
            r#"version 2
mission "protected-work" state="ready" revision-cutover="when-idle" revisions="human-only" revision-reviewer="person/reviewer" {{
goal {goal:?}
step "work" {{ assigned-to "agent/example/alder"; goal "Inspect"; }}
}}"#
        );
        let intent = graph::parse_internal_intent(&source, "orchid").unwrap();
        store.apply_internal(&intent, key).unwrap();
        intent.missions["protected-work"].clone()
    };
    publish("First goal", "protected-work-first");
    let run = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "protected-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: std::collections::BTreeMap::new(),
            idempotency_key: "protected-work-run".into(),
        })
        .unwrap();
    assert!(
        store
            .set_step_state(&run.steps[0].subject, "ready", None)
            .unwrap()
    );
    let revised = publish("Second goal", "protected-work-second");
    let incremental = Incremental::default();
    incremental.observe(&store).unwrap();
    settle_work(&incremental, &store, alder);
    let proposal = store
        .create_revision_proposal(
            &run.id,
            &revised,
            "person/requester",
            "Revise this fixture",
            "protected-work-proposal",
        )
        .unwrap();
    assert_eq!(proposal.status, "pending-approval");
    incremental.observe(&store).unwrap();
    assert!(incremental.needs(alder, 0));
    settle_work(&incremental, &store, alder);
    store
        .approve_revision_proposal(
            &proposal.id,
            "person/reviewer",
            proposal.preview_hash.as_deref().unwrap(),
            "protected-work-approval",
        )
        .unwrap();
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().phase,
        "revision-draining"
    );
    incremental.observe(&store).unwrap();
    assert!(incremental.needs(alder, 0));
    assert!(store.work_for_reconcile(alder).unwrap().is_empty());
    settle_work(&incremental, &store, alder);
    store
        .cancel_revision_proposal(
            &proposal.id,
            "person/requester",
            Some("Keep the original fixture"),
            "protected-work-cancel",
        )
        .unwrap();
    assert_eq!(store.mission_run(&run.id).unwrap().unwrap().phase, "normal");
    incremental.observe(&store).unwrap();
    assert!(incremental.needs(alder, 0));
    let visible = store.work_for_reconcile(alder).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].subject, run.steps[0].subject);
}

#[test]
fn work_new_generation_invalidates_a_seat_without_previous_steps() {
    let store = work_fixture();
    let alder = "agent/example/alder";
    let incremental = Incremental::default();
    incremental.observe(&store).unwrap();
    settle_work(&incremental, &store, alder);
    store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "new-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: std::collections::BTreeMap::new(),
            idempotency_key: "new-generation-work".into(),
        })
        .unwrap();
    incremental.observe(&store).unwrap();
    assert!(incremental.needs(alder, 0));
    assert_eq!(store.work_for_reconcile(alder).unwrap().len(), 1);
}

#[test]
fn work_pending_available_step_invalidates_its_candidates_when_it_becomes_ready() {
    let store = work_fixture();
    let alder = "agent/example/alder";
    let run = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "offered-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: std::collections::BTreeMap::new(),
            idempotency_key: "offered-work-run".into(),
        })
        .unwrap();
    let step = &run.steps[0];
    assert_eq!(step.status, "pending");
    assert!(store.work_for_reconcile(alder).unwrap().is_empty());
    let incremental = Incremental::default();
    incremental.observe(&store).unwrap();
    settle_work(&incremental, &store, alder);
    assert!(store.set_step_state(&step.subject, "ready", None).unwrap());
    incremental.observe(&store).unwrap();
    assert!(incremental.needs(alder, 0));
    let visible = store.work_for_reconcile(alder).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].subject, step.subject);
}

#[test]
fn work_handoff_invalidates_a_new_recipient_without_previous_steps() {
    let store = work_fixture();
    let alder = "agent/example/alder";
    let birch = "agent/example/birch";
    let work = start_work(&store, alder, "handoff-work");
    store
        .work_action(
            &work.subject,
            "claim",
            &crate::model::WorkRequest {
                actor: Some(alder.into()),
                incarnation: Some(format!("{alder}-one")),
                wait_ms: None,
                summary: None,
                reason: None,
                evidence: vec![],
                idempotency_key: "claim-for-handoff".into(),
            },
        )
        .unwrap();
    let incremental = Incremental::default();
    incremental.observe(&store).unwrap();
    settle_work(&incremental, &store, alder);
    settle_work(&incremental, &store, birch);
    let person_handoff = change_keys(&change(
        "step-run/unrelated/work",
        "work.released",
        Some("agent/example/other"),
        r#"{"fields":{"handoff_to":"person/alex"}}"#,
    ));
    assert!(person_handoff.contains(&"actor:person/alex".to_owned()));
    for key in person_handoff {
        incremental.touch(&key);
    }
    assert!(!incremental.needs(alder, 0));
    assert!(!incremental.needs(birch, 0));
    store
        .handoff_work(
            &work.subject,
            &crate::model::WorkHandoffRequest {
                actor: alder.into(),
                incarnation: Some(format!("{alder}-one")),
                to: birch.into(),
                note: "Inspect this fixture next".into(),
                evidence: vec![],
                idempotency_key: "handoff-to-birch".into(),
            },
        )
        .unwrap();
    incremental.observe(&store).unwrap();
    assert!(incremental.needs(alder, 0));
    assert!(
        incremental.needs(birch, 0),
        "new recipient has not read the step yet"
    );
    let received = store.work_for_reconcile(birch).unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].subject, work.subject);
    assert_eq!(received[0].assigned_to.as_deref(), Some(birch));
}
