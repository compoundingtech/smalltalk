#[test]
fn cancellation_releases_admission_capacity_before_final_work_and_cleanup_finish() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "upstream" state="ready" {
  goal "Hold admission."
  step "work" { agentless }
}
"#,
        "capacity-upstream",
    );
    let mission = publish_mission(
        &store,
        r#"version 2
mission "capacity" state="ready" {
  concurrent-runs max=1
  goal "Count runs admitted for work."
  step "work" { agentless }
  finally { step "cleanup" { agentless } }
}
"#,
        "capacity-mission",
    );
    let request = |mission: &str, key: &str| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/operator".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    };
    let upstream = store
        .create_mission_run(&request("upstream", "capacity-predecessor"))
        .unwrap();
    let source = format!(
        "version 2\nmission-run \"queued-capacity\" {{\n mission \"mission/capacity@{}\"\n workspace \"/tmp\"\n requester \"person/operator\"\n after {:?}\n}}\n",
        mission.revision, upstream.subject
    );
    let intent = parse_intent(&source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "capacity-queued")
        .unwrap();
    assert_eq!(
        store
            .create_mission_run(&request("capacity", "capacity-refused-live"))
            .unwrap_err()
            .code,
        "mission-run-capacity"
    );
    store
        .request_mission_run_cancellation("queued-capacity", "replace the waiting run")
        .unwrap();
    let cancelled = store.mission_run("queued-capacity").unwrap().unwrap();
    assert_eq!(cancelled.status, "running");
    assert_eq!(cancelled.phase, "final-cancelled");
    assert!(
        cancelled
            .steps
            .iter()
            .any(|step| step.step == "cleanup" && step.status == "pending")
    );
    let replacement = store
        .create_mission_run(&request("capacity", "capacity-replacement"))
        .expect("cancelled waiting run must release capacity before its final work finishes");
    assert_eq!(
        store
            .create_mission_run(&request("capacity", "capacity-refused-replacement"))
            .unwrap_err()
            .code,
        "mission-run-capacity"
    );
    store
        .set_mission_run_state("queued-capacity", "running", "cleanup-cancelled", None)
        .unwrap();
    store
        .request_mission_run_cancellation(&replacement.id, "replace admitted work")
        .unwrap();
    let running_cancel = store.mission_run(&replacement.id).unwrap().unwrap();
    assert_eq!(running_cancel.phase, "final-cancelled");
    store
        .create_mission_run(&request("capacity", "capacity-second-replacement"))
        .expect("neither final-cancelled nor cleanup-cancelled consumes admission capacity");
    assert_eq!(
        store
            .create_mission_run(&request("capacity", "capacity-refused-second-replacement"))
            .unwrap_err()
            .code,
        "mission-run-capacity"
    );
}
