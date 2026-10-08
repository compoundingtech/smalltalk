use super::*;
use crate::api::mission_eligibility_doctor::{THRESHOLD_MS, check};
use serde_json::json;

fn missing() -> (Arc<Store>, Reconciler<FakeRuntime>, MissionRunView) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        r#"version 2
mission "orchid" state="ready" {
 goal "Diagnose a sustained missing seat."
 completion { when "all-steps-exhausted" }
 step "work" { assigned-to "agent/orchid/absent" }
}"#,
        "first",
    );
    let run = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "orchid".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/lichen".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "doctor-run".into(),
        })
        .unwrap();
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    for _ in 0..4 {
        reconciler.reconcile_once().unwrap();
    }
    (store, reconciler, run)
}

fn current(store: &Store) -> Vec<crate::model::AttentionItemView> {
    store
        .fault_snapshot(now_ms())
        .unwrap()
        .into_iter()
        .map(|fault| fault.item)
        .collect()
}

#[test]
fn doctor_reuses_the_current_episode_at_the_five_minute_boundary_without_writes() {
    let (store, _reconciler, run) = missing();
    let mut items = current(&store);
    assert_eq!(items.len(), 1);
    let requested = items[0].requested_at_unix_ms;
    let before = store.index().unwrap();
    assert_eq!(
        check(&store, &items, requested + THRESHOLD_MS - 1)
            .unwrap()
            .status,
        "pass"
    );
    items.push(items[0].clone());
    let failure = check(&store, &items, requested + THRESHOLD_MS).unwrap();
    assert_eq!(
        (failure.name.as_str(), failure.status.as_str()),
        ("mission-no-eligible-agent", "fail")
    );
    assert!(failure.message.starts_with("1 current missing-agent"));
    assert!(failure.message.contains(&run.subject));
    assert!(failure.message.contains(&run.generation));
    assert!(failure.message.contains(&items[0].step.clone().unwrap()));
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(
        store
            .claims_for(&run.subject, Some("operational.failure"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn doctor_clears_on_eligibility_root_stop_cutover_or_terminal_state() {
    for action in ["restore", "root-stop", "revision", "cancel"] {
        let (store, reconciler, run) = missing();
        let items = current(&store);
        let overdue = items[0].requested_at_unix_ms + THRESHOLD_MS;
        assert_eq!(check(&store, &items, overdue).unwrap().status, "fail");
        match action {
            "restore" => {
                apply_source(
                    &store,
                    "version 2\nagent \"orchid/absent\" { command \"true\"; restart \"never\" }",
                    "restore",
                );
                for _ in 0..4 {
                    reconciler.reconcile_once().unwrap();
                }
            }
            "root-stop" => apply_source(&store, "version 2\nstop \"agent/orchid/absent\"", "stop"),
            "revision" => {
                apply_source(
                    &store,
                    r#"version 2
mission "orchid" state="ready" {
 goal "Change the diagnostic goal."
 completion { when "all-steps-exhausted" }
 step "work" { assigned-to "agent/orchid/absent" }
}"#,
                    "second",
                );
                let next = store.mission_spec("orchid", None).unwrap().unwrap();
                store
                    .adopt_mission_revision(
                        &run.id,
                        &next,
                        "person/lichen",
                        "Change goal",
                        "cutover",
                    )
                    .unwrap();
            }
            "cancel" => {
                store
                    .request_mission_run_cancellation(&run.id, "End control")
                    .unwrap();
                for _ in 0..4 {
                    reconciler.reconcile_once().unwrap();
                }
            }
            _ => unreachable!(),
        }
        let items = current(&store);
        assert_eq!(
            check(&store, &items, overdue).unwrap().status,
            "pass",
            "{action}"
        );
        assert_eq!(
            store
                .claims_for(&run.subject, Some("operational.failure"))
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn doctor_does_not_infer_a_live_fault_from_a_missing_or_unrelated_episode() {
    let (store, _reconciler, _run) = missing();
    let mut items = current(&store);
    let overdue = items[0].requested_at_unix_ms + THRESHOLD_MS;
    items[0].episode = "missing-claim".into();
    assert_eq!(check(&store, &items, overdue).unwrap().status, "pass");
    let unrelated = store
        .append_claim(&crate::model::ClaimInput {
            subject: items[0].subject.clone(),
            kind: "operational.failure".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("condition".into(), json!("work-wake-exhausted")),
                ("episode".into(), json!("unrelated-episode")),
                ("reviewer".into(), json!("person/lichen")),
                ("title".into(), json!("An unrelated fault")),
                (
                    "reason".into(),
                    json!("Preserve the other fault's own currentness."),
                ),
                ("severity".into(), json!("error")),
                ("targets".into(), json!([items[0].subject])),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("other-condition".into()),
        })
        .unwrap();
    items[0].episode = unrelated.id;
    assert_eq!(check(&store, &items, overdue).unwrap().status, "pass");
}

#[test]
fn doctor_keeps_dependency_waits_quiet_beyond_the_threshold() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        r#"version 2
mission "orchid" state="ready" {
 goal "Wait on a dependency without diagnosing its future assignment."
 completion { when "all-steps-exhausted" }
 step "gate" { agentless; gate "wait" { document "doc/orchid-ready" } }
 step "work" { assigned-to "agent/orchid/absent"; depends-on { step "gate" completed } }
}"#,
        "waiting",
    );
    store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "orchid".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/lichen".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "waiting-run".into(),
        })
        .unwrap();
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    for _ in 0..4 {
        reconciler.reconcile_once().unwrap();
    }
    assert_eq!(
        check(&store, &current(&store), now_ms() + THRESHOLD_MS * 2)
            .unwrap()
            .status,
        "pass"
    );
}
