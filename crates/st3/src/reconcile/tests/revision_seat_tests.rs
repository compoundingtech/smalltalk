use super::*;

fn publish(store: &Store, source: &str, key: &str) -> MissionSpec {
    apply_source(store, source, key);
    store.mission_spec("orchid", None).unwrap().unwrap()
}

fn source(goal: &str) -> String {
    format!(
        r#"version 2
mission "orchid" state="ready" {{
  goal "{goal}"
  completion {{ when "all-steps-exhausted" }}
  step "start-seats" {{
    agentless
    agent "amber" {{ workspace "/tmp"; command "true"; restart "never" }}
    agent "birch" {{ workspace "/tmp"; command "true"; restart "never" }}
    agent "cedar" {{ workspace "/tmp"; command "true"; restart "never" }}
    agent "dahlia" {{ workspace "/tmp"; command "true"; restart "never" }}
  }}
  step "amber" {{ assigned-to "agent/${{ST_MISSION_RUN}}/amber"; depends-on {{ step "start-seats" completed }} }}
  step "birch" {{ assigned-to "agent/${{ST_MISSION_RUN}}/birch"; depends-on {{ step "start-seats" completed }} }}
  step "cedar" {{ assigned-to "agent/${{ST_MISSION_RUN}}/cedar"; depends-on {{ step "start-seats" completed }} }}
  step "dahlia" {{ assigned-to "agent/${{ST_MISSION_RUN}}/dahlia"; depends-on {{ step "start-seats" completed }} }}
}}"#
    )
}

fn start(store: &Store, key: &str) -> MissionRunView {
    store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "orchid".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/gardener".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: key.into(),
        })
        .unwrap()
}

fn settle(reconciler: &Reconciler<FakeRuntime>) {
    for _ in 0..8 {
        for member in reconciler.runtime.started_members.lock().unwrap().iter() {
            if member.terminal {
                let mut ptys = reconciler.runtime.ptys.lock().unwrap();
                if !ptys.iter().any(|p| p.runtime_id == member.runtime_id) {
                    ptys.push(RuntimeObservation {
                        runtime_id: member.runtime_id.clone(),
                        terminal: true,
                        status: "running".into(),
                        exit_code: None,
                        incarnation_id: Some(format!("{}-one", member.runtime_id)),
                    });
                }
            }
        }
        reconciler.reconcile_once().unwrap();
    }
}

#[test]
fn revision_carries_completed_start_seats_and_four_queued_builders() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "garden");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    assert_eq!(
        store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .find(|s| s.step == "start-seats")
            .unwrap()
            .status,
        "completed"
    );
    for round in 1..=2 {
        let next = publish(
            &store,
            &source(&format!("Grow garden {round}.")),
            &format!("publish-{round}"),
        );
        let revised = store
            .adopt_mission_revision(
                &run.id,
                &next,
                "person/gardener",
                "Update garden goal",
                &format!("revise-{round}"),
            )
            .unwrap();
        // Ownership must already be safe when the revision receipt is returned.
        for name in ["amber", "birch", "cedar", "dahlia"] {
            let desired = store
                .desired_subjects_named(&[format!("agent/{}/{name}", run.id)])
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(desired.kind, "agent");
            assert_eq!(
                desired.owner_generation.as_deref(),
                Some(revised.generation.as_str())
            );
            assert_eq!(
                desired.owner_step.as_deref(),
                Some(
                    format!(
                        "step-run/{}/start-seats",
                        revised.generation.trim_start_matches("run-generation/")
                    )
                    .as_str()
                )
            );
        }
        settle(&reconciler);
        let current = store.mission_run(&run.id).unwrap().unwrap();
        let cards = store
            .agent_card_status_at(None, store.index().unwrap(), false)
            .unwrap();
        assert_eq!(cards.subjects.len(), 4);
        assert!(
            cards
                .subjects
                .iter()
                .all(|s| s.projection.owner_generation.as_deref()
                    == Some(revised.generation.as_str()))
        );
        assert_eq!(
            current
                .steps
                .iter()
                .find(|s| s.step == "start-seats")
                .unwrap()
                .status,
            "completed"
        );
        assert!(
            current
                .steps
                .iter()
                .filter(|s| s.step != "start-seats")
                .all(|s| s.status == "ready"),
            "{:?}",
            current.steps
        );
    }
}

#[test]
fn no_eligible_agent_fault_is_actionable_deduped_and_recovers() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(
        &store,
        r#"version 2
mission "orchid" state="ready" {
  goal "Offer work to an absent seat."
  completion { when "all-steps-exhausted" }
  step "work" { assigned-to "agent/orchid/amber" }
}"#,
        "publish",
    );
    let run = start(&store, "absent");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    settle(&reconciler);
    let current = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(current.status, "blocked");
    let faults = store.fault_items(Some("person/gardener")).unwrap();
    let fault = faults.iter().find(|f| f.targets.contains(&run.generation)).unwrap_or_else(|| panic!("requester receives a missing-agent fault: items={faults:?}, claims={:?}, raised={:?}", store.claims_for(&run.subject, Some("operational.failure")).unwrap(), reconciler.raised_faults.lock().unwrap()));
    assert!(fault.targets.contains(&current.steps[0].subject));
    assert!(fault.detail.contains("eligible"));
    let index = store.index().unwrap();
    settle(&reconciler);
    assert_eq!(
        store.index().unwrap(),
        index,
        "settled faults do not replay history or write again"
    );
    assert!(
        !reconciler.incremental.needs(&run.subject, now_ms()),
        "blocked evaluation is settled clean"
    );
    assert!(
        reconciler
            .incremental
            .reads_of(&run.subject)
            .contains("agent/orchid/amber")
    );
    apply_source(
        &store,
        "version 2\nagent \"orchid/amber\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }",
        "restore",
    );
    reconciler.incremental.observe(&store).unwrap();
    assert!(
        reconciler.incremental.needs(&run.subject, now_ms()),
        "only the restoring desired event dirties admission"
    );
    settle(&reconciler);
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
        "ready"
    );
    assert!(
        !store
            .fault_items(Some("person/gardener"))
            .unwrap()
            .iter()
            .any(|f| f.targets.contains(&run.generation))
    );
}

#[test]
fn explicit_stop_survives_completed_declaration_carry_without_a_fault() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "garden-stop");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let name = format!("agent/{}/amber", run.id);
    let text = format!("version 2\nstop {name:?}");
    let intent = parse_intent(&text, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: text,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &plan.subject_tokens,
            "stop-amber",
            Some("person/gardener"),
        )
        .unwrap();
    let next = publish(&store, &source("Respect a stopped seat."), "second");
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &next,
            "person/gardener",
            "Update goal",
            "stop-cutover",
        )
        .unwrap();
    settle(&reconciler);
    assert_eq!(
        store.selected_desired_kind(&name).unwrap().as_deref(),
        Some("stop")
    );
    assert!(
        !store
            .fault_snapshot(now_ms())
            .unwrap()
            .iter()
            .any(|f| f.item.targets.contains(&revised.generation))
    );
}

#[test]
fn changed_declarations_execute_again_and_removed_seats_retire() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "garden-change");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let next_source = source("Change the garden.")
        .replace(
            "agent \"amber\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }",
            "agent \"amber\" { workspace \"/tmp\"; command \"sleep 1\"; restart \"never\" }",
        )
        .replace(
            "    agent \"dahlia\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }",
            "",
        );
    let next = publish(&store, &next_source, "second");
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &next,
            "person/gardener",
            "Change declaring step explicitly",
            "change-cutover",
        )
        .unwrap();
    assert_eq!(
        revised
            .steps
            .iter()
            .find(|s| s.step == "start-seats")
            .unwrap()
            .status,
        "pending"
    );
    settle(&reconciler);
    assert_eq!(
        store
            .selected_desired_kind(&format!("agent/{}/dahlia", run.id))
            .unwrap()
            .as_deref(),
        Some("stop")
    );
    let amber = store
        .desired_subjects_named(&[format!("agent/{}/amber", run.id)])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(amber.kind, "agent");
    assert!(
        serde_json::to_string(&amber.member)
            .unwrap()
            .contains("sleep 1")
    );
}

#[test]
fn unsafe_completed_exec_carry_refuses_and_rolls_back_generation_publication() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let first = source("Grow a garden.").replace("    agent \"dahlia\" {", "    exec \"dahlia\" {");
    publish(&store, &first, "first");
    let run = start(&store, "garden-refuse");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let current = store.mission_run(&run.id).unwrap().unwrap();
    let declaring = current
        .steps
        .iter()
        .find(|s| s.step == "start-seats")
        .unwrap();
    store
        .set_step_state(&declaring.subject, "completed", None)
        .unwrap();
    let next = publish(
        &store,
        &first.replace("Grow a garden.", "Change only the goal."),
        "second",
    );
    let cards_before = store
        .agent_card_status_at(None, store.index().unwrap(), false)
        .unwrap();
    let before = store.index().unwrap();
    let error = store
        .adopt_mission_revision(
            &run.id,
            &next,
            "person/gardener",
            "Must not replay exec",
            "refuse-cutover",
        )
        .unwrap_err();
    assert_eq!(error.code, "unsafe-completed-declaration-carry");
    assert_eq!(store.index().unwrap(), before);
    let cards_after = store
        .agent_card_status_at(None, store.index().unwrap(), false)
        .unwrap();
    assert_eq!(
        serde_json::to_value(cards_after).unwrap(),
        serde_json::to_value(cards_before).unwrap()
    );
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().generation,
        run.generation
    );
}

#[test]
fn dependency_waits_and_cancellation_do_not_raise_missing_agent_faults() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(
        &store,
        r#"version 2
mission "orchid" state="ready" {
 goal "Wait on a real dependency."
 completion { when "all-steps-exhausted" }
 step "gate" { agentless; gate "wait" { document "doc/garden-ready" } }
 step "work" { assigned-to "agent/orchid/absent"; depends-on { step "gate" completed } }
}"#,
        "publish",
    );
    let run = start(&store, "waiting");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    assert_eq!(
        store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .find(|s| s.step == "work")
            .unwrap()
            .status,
        "pending"
    );
    assert!(
        !store
            .fault_snapshot(now_ms())
            .unwrap()
            .iter()
            .any(|f| f.item.targets.contains(&run.generation))
    );
    store
        .request_mission_run_cancellation(&run.id, "End dependency wait")
        .unwrap();
    settle(&reconciler);
    assert!(
        !store
            .fault_snapshot(now_ms())
            .unwrap()
            .iter()
            .any(|f| f.item.targets.contains(&run.generation))
    );
}

#[test]
fn completed_seat_carry_replays_canonical_desired_ownership() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "garden-replica");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let next = publish(&store, &source("Grow a revised garden."), "second");
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &next,
            "person/gardener",
            "Update goal",
            "replica-cutover",
        )
        .unwrap();
    let replica = Store::open_memory("replica").unwrap();
    replica
        .import_replication("node", &store.export_replication(0).unwrap())
        .unwrap();
    for name in ["amber", "birch", "cedar", "dahlia"] {
        let subject = format!("agent/{}/{name}", run.id);
        let desired = replica
            .desired_subjects_named(&[subject.clone()])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            desired.owner_generation.as_deref(),
            Some(revised.generation.as_str())
        );
        let canonical = replica
            .latest_claim(&subject, Some("intent.desired"))
            .unwrap()
            .unwrap();
        assert_eq!(canonical.body["owner_generation"], revised.generation);
    }
    let cards = replica
        .agent_card_status_at(None, replica.index().unwrap(), false)
        .unwrap();
    assert_eq!(cards.subjects.len(), 4);
    assert!(cards.subjects.iter().all(
        |card| card.projection.owner_generation.as_deref() == Some(revised.generation.as_str())
    ));
    assert_eq!(
        replica.mission_run(&run.id).unwrap().unwrap().generation,
        revised.generation
    );
}

#[test]
fn declared_revision_uses_the_same_atomic_completed_seat_carry() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "declared-revision");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let next = publish(&store, &source("Grow a declared revision."), "second");
    let text = format!(
        "version 2\nmission-run {:?} {{ revision \"update\" {{ mission {:?}; from {:?}; reason \"Update the garden\" }} }}",
        run.id,
        format!("mission/orchid@{}", next.revision),
        run.generation
    );
    let intent = parse_intent(&text, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: text,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &plan.subject_tokens,
            "declared-cutover",
            Some("person/gardener"),
        )
        .unwrap();
    let current = store.mission_run(&run.id).unwrap().unwrap();
    assert_ne!(current.generation, run.generation);
    for desired in store.desired_subjects_for_owner_run(&run.subject).unwrap() {
        assert_eq!(
            desired.owner_generation.as_deref(),
            Some(current.generation.as_str())
        );
    }
    settle(&reconciler);
    assert_eq!(
        store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .find(|s| s.step == "start-seats")
            .unwrap()
            .status,
        "completed"
    );
}

#[test]
fn generation_dependent_completed_declaration_is_refused_before_publication() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let first = source("Grow a garden.")
        .replace("command \"true\"", "command \"echo ${ST_RUN_GENERATION}\"");
    publish(&store, &first, "first");
    let run = start(&store, "dynamic-source");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let intent = parse_intent(
        &first.replace("Grow a garden.", "Change only the goal."),
        "node",
    )
    .unwrap();
    let next = &intent.missions["orchid"];
    let current = store.mission_run(&run.id).unwrap().unwrap();
    let before = store.index().unwrap();
    assert_eq!(
        store
            .validate_revision_seat_carry(&current, next)
            .unwrap_err()
            .code,
        "unsafe-completed-declaration-carry"
    );
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(
        store
            .mission_spec("orchid", None)
            .unwrap()
            .unwrap()
            .revision,
        current.revision
    );
}

#[test]
fn reopening_after_cleanup_requires_explicit_reexecution_of_completed_declarations() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "reopen-source");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let current = store.mission_run(&run.id).unwrap().unwrap();
    for step in current.steps.iter().filter(|s| s.step != "start-seats") {
        store
            .set_step_state(&step.subject, "failed", Some("Control failure"))
            .unwrap();
    }
    store
        .set_mission_run_state(&run.id, "failed", "terminal", Some("Control failure"))
        .unwrap();
    let next = parse_intent(&source("Reopen the garden."), "node")
        .unwrap()
        .missions
        .remove("orchid")
        .unwrap();
    let current = store.mission_run(&run.id).unwrap().unwrap();
    let before = store.index().unwrap();
    assert_eq!(
        store
            .validate_revision_seat_carry(&current, &next)
            .unwrap_err()
            .code,
        "unsafe-completed-declaration-carry"
    );
    assert_eq!(store.index().unwrap(), before);
}

#[test]
fn completed_seat_owner_cards_survive_reopen_and_dirty_both_owner_steps() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let store = Arc::new(Store::open(&path, "node").unwrap());
    publish(&store, &source("Grow a garden."), "first");
    let run = start(&store, "garden-reopen");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let next = publish(&store, &source("Grow a revised garden."), "second");
    let old_step = run
        .steps
        .iter()
        .find(|step| step.step == "start-seats")
        .unwrap()
        .subject
        .clone();
    let incremental = crate::incremental::Incremental::default();
    incremental.observe(&store).unwrap();
    let (owned, reads) =
        smallclaims::touched::record(|| store.desired_subjects_for_owner_step(&old_step).unwrap());
    assert_eq!(owned.len(), 4);
    incremental.evaluated("old-owner-reader", reads, None);
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &next,
            "person/gardener",
            "Update goal",
            "reopen-cutover",
        )
        .unwrap();
    let new_step = revised
        .steps
        .iter()
        .find(|step| step.step == "start-seats")
        .unwrap()
        .subject
        .clone();
    let (owned, reads) = smallclaims::touched::record(|| {
        store
            .desired_subjects_for_owner_steps(std::slice::from_ref(&new_step))
            .unwrap()
    });
    assert_eq!(owned.len(), 4);
    incremental.evaluated("new-owner-reader", reads, None);
    incremental.observe(&store).unwrap();
    assert!(incremental.needs("old-owner-reader", now_ms()));
    assert!(incremental.needs("new-owner-reader", now_ms()));
    assert!(
        store
            .desired_subjects_for_owner_step(&old_step)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .desired_subjects_for_owner_step(&new_step)
            .unwrap()
            .len(),
        4
    );
    let carry = store
        .latest_claim(&format!("agent/{}/amber", run.id), Some("intent.desired"))
        .unwrap()
        .unwrap();
    let keys = crate::incremental::change_keys(&crate::store::Change {
        subject: carry.subject,
        kind: carry.kind,
        actor: carry.actor,
        body: carry.body.to_string(),
    });
    assert!(keys.contains(&format!("owned-step:{old_step}")));
    assert!(keys.contains(&format!("owned-step:{new_step}")));
    assert!(keys.contains(&run.generation));
    assert!(keys.contains(&revised.generation));
    assert!(keys.contains(&format!("owned:{}", run.subject)));
    settle(&reconciler);
    let before = store
        .agent_card_status_at(None, store.index().unwrap(), false)
        .unwrap();
    drop(reconciler);
    drop(store);
    let reopened = Store::open(&path, "node").unwrap();
    let after = reopened
        .agent_card_status_at(None, reopened.index().unwrap(), false)
        .unwrap();
    assert_eq!(after.subjects.len(), 4);
    assert!(after.subjects.iter().all(
        |card| card.projection.owner_generation.as_deref() == Some(revised.generation.as_str())
    ));
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}

#[test]
fn ready_work_losing_binding_reports_a_new_episode_and_terminal_clears_it() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let first = r#"version 2
agent "orchid/amber" { workspace "/tmp"; command "true"; restart "never" }
mission "orchid" state="ready" {
 goal "Admit work before its binding is retired."
 completion { when "all-steps-exhausted" }
 step "work" { assigned-to "agent/orchid/amber" }
}"#;
    publish(&store, first, "first");
    let run = start(&store, "lost-ready");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    settle(&reconciler);
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
        "ready"
    );
    for round in 1..=2 {
        // An owned lifecycle stop differs from a deliberate root stop.
        let mut intent = parse_intent("version 2\nstop \"agent/orchid/amber\"", "node").unwrap();
        intent
            .subjects
            .get_mut("agent/orchid/amber")
            .unwrap()
            .owner_run = Some(run.subject.clone());
        intent
            .subjects
            .get_mut("agent/orchid/amber")
            .unwrap()
            .owner_generation = Some(run.generation.clone());
        store
            .apply_internal(&intent, &format!("retire-{round}"))
            .unwrap();
        reconciler.incremental.observe(&store).unwrap();
        assert!(reconciler.incremental.needs(&run.subject, now_ms()));
        settle(&reconciler);
        let failures = store
            .claims_for(&run.subject, Some("operational.failure"))
            .unwrap();
        assert_eq!(failures.len(), round);
        assert_eq!(store.fault_items(Some("person/gardener")).unwrap().len(), 1);
        let episode = &failures.last().unwrap().body["fields"]["episode"];
        assert!(
            episode
                .as_str()
                .unwrap()
                .starts_with("mission-no-eligible-agent:")
        );
        apply_source(
            &store,
            "version 2\nagent \"orchid/amber\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }",
            &format!("restore-{round}"),
        );
        settle(&reconciler);
        assert!(
            store
                .fault_items(Some("person/gardener"))
                .unwrap()
                .is_empty()
        );
    }
    let current = store.mission_run(&run.id).unwrap().unwrap();
    store
        .set_step_state(
            &current.steps[0].subject,
            "blocked",
            Some("no eligible agent is present in the desired graph"),
        )
        .unwrap();
    store
        .set_mission_run_state(&run.id, "cancelled", "terminal", Some("End control"))
        .unwrap();
    let index = store.index().unwrap();
    assert!(
        !store
            .record_missing_agent_failure(&current, &current.steps[0])
            .unwrap()
    );
    assert_eq!(store.index().unwrap(), index);
    assert!(
        store
            .fault_items(Some("person/gardener"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retry_backoff_defers_missing_agent_fault_until_admission() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let source = r#"version 2
mission "orchid" state="ready" {
 goal "Respect retry admission."
 completion { when "all-steps-exhausted" }
 step "work" { assigned-to "agent/orchid/absent" }
}"#;
    publish(&store, source, "first");
    let run = start(&store, "backoff");
    store
        .set_step_state(&run.steps[0].subject, "failed", Some("Control retry"))
        .unwrap();
    store
        .retry_step(&run.steps[0].subject, "Wait for a seat", 120_000)
        .unwrap();
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
        "pending"
    );
    assert!(
        store
            .claims_for(&run.subject, Some("operational.failure"))
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .fault_items(Some("person/gardener"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn stale_generation_cannot_report_or_retain_an_eligibility_fault() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let first = r#"version 2
mission "orchid" state="ready" {
 goal "Offer work to a missing seat."
 completion { when "all-steps-exhausted" }
 step "work" { assigned-to "agent/orchid/absent" }
}"#;
    publish(&store, first, "first");
    let run = start(&store, "stale-fault");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    settle(&reconciler);
    let predecessor = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(store.fault_items(Some("person/gardener")).unwrap().len(), 1);
    let next = publish(
        &store,
        &first.replace("Offer work to a missing seat.", "Revise missing work."),
        "second",
    );
    let successor = store
        .adopt_mission_revision(
            &run.id,
            &next,
            "person/gardener",
            "Change goal",
            "stale-cutover",
        )
        .unwrap();
    let index = store.index().unwrap();
    assert!(
        !store
            .record_missing_agent_failure(&predecessor, &predecessor.steps[0])
            .unwrap()
    );
    assert_eq!(store.index().unwrap(), index);
    assert!(
        store
            .fault_items(Some("person/gardener"))
            .unwrap()
            .is_empty()
    );
    settle(&reconciler);
    let faults = store.fault_items(Some("person/gardener")).unwrap();
    assert_eq!(faults.len(), 1);
    assert!(faults[0].targets.contains(&successor.generation));
    assert!(!faults[0].targets.contains(&predecessor.generation));
}
