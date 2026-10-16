//! Registry compatibility through normal reconciliation/replication; not an old-binary claim.
use super::*;
use serde_json::json;
use crate::schedule_standby::tests::{advertise, pair, sync};

#[tokio::test]
async fn old_registry_unknown_takeover_detail_cannot_replay_a_primary_occurrence() {
    let (primary, standby, standby_spec) = pair();
    let primary = Arc::new(primary);
    let root = tempfile::tempdir().unwrap();
    let at = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let source = format!(r#"version 2
mission "harvest" state="ready" {{
  goal "Perform the scheduled harvest occurrence."
 completion {{ when "all-steps-exhausted" }}
 step "done" {{ agentless }}
}}
schedule "orchard" {{ host "ivory"; standby {{ host "jade"; after "50ms" }}
 at {at:?}; work {{ mission "harvest"; workspace {:?} }}
}}"#, root.path().to_str().unwrap());
    apply_source(&primary, &source, "standby-compat-source");
    sync(&primary, &standby);
    let reconciler = Reconciler::new(primary.clone(), Arc::new(FakeRuntime::default()), "ivory".into(), Arc::new(Notify::new()));
    sync(&standby, &primary);
    for _ in 0..4 {
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before = primary.claims_for("schedule/orchard", Some("schedule.work-started")).unwrap();
    assert_eq!(before.len(), 1, "normal primary path did not start the occurrence");
    let run = Store::scheduled_mission_run_subject("schedule/orchard", 0);
    assert!(primary.mission_run(&run).unwrap().is_some());

    let mut old_registry = st3_schema::registry().clone();
    for kind in ["schedule.occurrence-scheduled", "schedule.occurrence-reached"] {
        old_registry.claims.get_mut(kind).unwrap().fields.remove("standby");
        old_registry.claims.get_mut(kind).unwrap().fields.remove("reason");
    }
    primary.set_claim_registry(old_registry);
    advertise(&primary, 0);
    sync(&primary, &standby);
    assert!(standby.schedule_standby_adoption_epoch(&standby_spec).is_err(), "missing old-member advertisement enabled standby");
    // A retained future detail may be received even though our own standby admission holds.
    // Feed it through normal export/admission, not a direct table mutation.
    standby.append_claim(&ClaimInput {
        subject: "schedule/orchard".into(), kind: "schedule.occurrence-reached".into(), actor: None,
        fields: BTreeMap::from([
            ("occurrence".into(), Value::from(0)), ("revision".into(), json!("future-detail")),
            ("scheduled_at_unix_ms".into(), json!("1")),
            ("standby".into(), json!({"host":"jade","primary":"ivory","reason":"invented compatibility input"})),
        ]), evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap();
    let exchange = standby.export_replication_exchange_answering(crate::schedule_standby::tests::FLEET,
        &primary.replication_inventory().unwrap(), &primary.replication_signature_requests().unwrap()).unwrap();
    primary.receive_replication_exchange("jade", crate::schedule_standby::tests::FLEET, &exchange).unwrap();
    let admission = primary.validate_replication_backlog().unwrap();
    assert!(admission.unknown > 0, "old registry did not retain unknown takeover detail");
    primary.project_replication_backlog().unwrap();
    for _ in 0..3 { reconciler.reconcile_once().unwrap(); tokio::task::yield_now().await; }
    let after = primary.claims_for("schedule/orchard", Some("schedule.work-started")).unwrap();
    assert_eq!(after.len(), before.len(), "unknown future detail replayed known primary work");
    assert_eq!(after[0].body["fields"]["mission_run"], run);
}


#[tokio::test]
async fn future_standby_skip_tick_is_local_until_due_and_eligible() {
    let (primary, standby, _) = pair();
    let primary = Arc::new(primary);
    let standby = Arc::new(standby);
    let root = tempfile::tempdir().unwrap();
    let anchor = (Utc::now() + chrono::Duration::seconds(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let source = format!(r#"version 2
mission "harvest" state="ready" {{
  goal "Perform the scheduled harvest occurrence."
 completion {{ when "all-steps-exhausted" }}
 step "done" {{ agentless }}
}}
schedule "orchard" {{ host "ivory"; standby {{ host "jade"; after "20ms" }}
 every "10s"; anchor {anchor:?}; catch-up "skip"
 work {{ mission "harvest"; workspace {:?} }}
}}"#, root.path().to_str().unwrap());
    apply_source(&primary, &source, "future-standby-skip");
    sync(&primary, &standby);
    let primary_reconciler = Reconciler::new(primary.clone(), Arc::new(FakeRuntime::default()), "ivory".into(), Arc::new(Notify::new()));
    let reconciler = Reconciler::new(standby.clone(), Arc::new(FakeRuntime::default()), "jade".into(), Arc::new(Notify::new()));
    sync(&standby, &primary);
    primary_reconciler.reconcile_once().unwrap();
    // Retain the primary's ordinary future scheduling operation before the partition.
    sync(&primary, &standby);
    assert_eq!(standby.claims_for("schedule/orchard", Some("schedule.occurrence-scheduled")).unwrap().len(), 1);
    reconciler.reconcile_once().unwrap();
    assert!(standby.claims_for("schedule/orchard", Some("schedule.occurrence-scheduled")).unwrap().iter().all(|claim| claim.origin != "jade"),
        "future standby wrote a replicated scheduling claim before eligibility");
    assert!(reconciler.delayed_restarts.lock().unwrap().keys().any(|key| key.starts_with("standby-due:schedule/orchard:")));
    let (_, tick, at) = reconciler.standby_due_ticks.lock().unwrap()["schedule/orchard"].clone();
    assert_eq!(tick, 0);
    tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms() as i64).max(0) as u64 + 10)).await;
    reconciler.reconcile_once().unwrap();
    assert!(standby.claims_for("schedule/orchard", Some("schedule.occurrence-scheduled")).unwrap().iter().all(|claim| claim.origin != "jade"),
        "fresh heartbeat wrote a scheduling claim");
    tokio::time::sleep(Duration::from_millis(30)).await;
    reconciler.reconcile_once().unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let scheduled = standby.claims_for("schedule/orchard", Some("schedule.occurrence-scheduled")).unwrap()
        .into_iter().filter(|claim| claim.origin == "jade").collect::<Vec<_>>();
    assert_eq!(scheduled.len(), 1);
    assert_eq!(scheduled[0].body["fields"]["occurrence"], 0, "skip lost the preselected due tick");
    assert_eq!(scheduled[0].body["fields"]["standby"]["host"], "jade");
    let reached = standby.claims_for("schedule/orchard", Some("schedule.occurrence-reached")).unwrap();
    assert_eq!(reached.len(), 1);
    assert_eq!(reached[0].body["fields"]["occurrence"], 0);
    // Work requests carry mission/NAME; the standby must find the stored bare mission ID.
    reconciler.reconcile_once().unwrap();
    let run = Store::scheduled_mission_run_subject("schedule/orchard", 0);
    assert!(standby.mission_run(&run).unwrap().is_some(), "standby rejected an available prefixed mission revision");
    assert_eq!(standby.claims_for("schedule/orchard", Some("schedule.work-started")).unwrap().len(), 1);
    assert!(standby.claims_for("schedule/orchard", Some("schedule.work-failed")).unwrap().is_empty());
    sync(&standby, &primary);
    assert_eq!(primary.claims_for("schedule/orchard", Some("schedule.occurrence-scheduled")).unwrap().len(), 2);
    assert_eq!(primary.idempotency_conflicts(10).unwrap().0, 0,
        "host publication attempts conflicted despite sharing only occurrence run identity");
}
