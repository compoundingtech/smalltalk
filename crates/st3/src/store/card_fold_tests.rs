//! The agent card fold reads many subjects per statement. Its answers must be the ones the
//! per-subject reductions give, at every snapshot, whatever order replication delivered the
//! claims in, after a rolled-back write and after a checkpoint trimmed history.
use super::tests::{exchange_from, receive_and_project};
use super::*;

/// Every snapshot of `store` that a fold could read: each store index up to its head.
fn snapshots(store: &Store) -> Vec<u64> {
    (0..=store.index().unwrap()).collect()
}

fn agent_subjects(connection: &Connection, index: u64) -> Vec<String> {
    connection
        .prepare_cached(RANGE_SUBJECTS)
        .unwrap()
        .query_map(params![index, "agent/", "agent0"], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// The agent card status pass as it was before the fold read subjects a chunk at a time:
/// one runtime view entry and one card reduction per subject.
fn per_subject_card_status(
    connection: &Connection,
    names: &[String],
    index: u64,
    history: bool,
) -> Vec<Value> {
    names
        .iter()
        .filter(|name| {
            history
                || !runtime_view_entry(connection, name, index, true)
                    .unwrap()
                    .history
        })
        .map(|name| {
            subject_status_at_with_mode(
                connection, name, Some(index), None, SubjectStatusMode::AgentCard,
            )
            .unwrap()
            .unwrap()
            .0
        })
        .filter(|status| history || status.projection.layer == "current")
        .map(|status| serde_json::to_value(status).unwrap())
        .collect()
}

/// Assert that the chunked fold and the per-subject reductions agree on `store` at `indexes`:
/// each runtime view entry, each card reduction, and the cold roster status pass.
pub(super) fn assert_card_fold_parity(store: &Store, indexes: &[u64]) {
    for &index in indexes {
        let connection = store.readers.get();
        let names = agent_subjects(&connection, index);
        for owners in [false, true] {
            let mut reads = card_fold::CardReads::new(index);
            let mut entries = store
                .runtime_view_entries(&connection, &names, index, owners, &mut reads)
                .unwrap();
            for name in &names {
                let one = runtime_view_entry(&connection, name, index, owners).unwrap();
                let many = entries.remove(name).unwrap();
                assert_eq!(
                    (many.head, many.history, many.declared, many.owners),
                    (one.head, one.history, one.declared, one.owners),
                    "view entry of {name} at {index}, owners={owners}"
                );
            }
            // The status pass shares what the view pass read, as the roster does.
            let mut statuses = store
                .agent_card_statuses(&connection, &names, index, &mut reads)
                .unwrap();
            for name in &names {
                let (status, action) = subject_status_at_with_mode(
                    &connection, name, Some(index), None, SubjectStatusMode::AgentCard,
                )
                .unwrap()
                .unwrap();
                let many = statuses.remove(name).unwrap();
                assert_eq!(
                    serde_json::to_value(&many).unwrap(),
                    serde_json::to_value((status, action)).unwrap(),
                    "card status of {name} at {index}"
                );
            }
        }
        drop(connection);
        assert_card_reads_parity(store, &names, index);
        let connection = store.readers.get();
        for history in [false, true] {
            let expected = per_subject_card_status(&connection, &names, index, history);
            store.forget_current_views();
            let cold = store.agent_card_status_at(None, index, history).unwrap();
            assert_eq!(cold.store_index, index);
            let cold = cold
                .subjects
                .iter()
                .map(|status| serde_json::to_value(status).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(cold, expected, "cold roster status at {index}, history={history}");
        }
    }
}

/// The roster's per-card reads of `names` at `index` against the one-agent readers they replace.
fn assert_card_reads_parity(store: &Store, names: &[String], index: u64) {
    // With each agent's selected actual claim, as the roster passes them, and without.
    let connection = store.readers.get();
    let selected = names
        .iter()
        .map(|name| {
            let (status, _) = subject_status_at_with_mode(
                &connection, name, Some(index), None, SubjectStatusMode::AgentCard,
            )
            .unwrap()
            .unwrap();
            (name.clone(), status.actual_claim)
        })
        .collect::<HashMap<_, _>>();
    drop(connection);
    for actual_claims in [selected, HashMap::new()] {
        assert_card_reads_parity_with(store, names, index, &actual_claims);
    }
}

fn assert_card_reads_parity_with(
    store: &Store,
    names: &[String],
    index: u64,
    actual_claims: &HashMap<String, Option<String>>,
) {
    let mut reads = store.agent_card_reads(names, index, actual_claims).unwrap();
    for name in names {
        let one = store.observed_harness_at(name, index).unwrap();
        let many = reads.take_harness(store, name).unwrap();
        assert_eq!(
            serde_json::to_value(&many).unwrap(),
            serde_json::to_value(&one).unwrap(),
            "harness of {name} at {index}"
        );
        let incarnation = one.as_ref().map(|harness| harness.incarnation_id.as_str());
        assert_eq!(
            reads.last_activity_at(store, name, incarnation).unwrap(),
            store.agent_last_activity_at(name, incarnation, index).unwrap(),
            "last activity of {name} at {index}"
        );
        if !reads.may_have_suspension(name) {
            assert!(crate::suspension::current(store, name).unwrap().is_none(), "{name}");
        }
        if !reads.may_have_rollout(name) {
            assert_eq!(crate::rollout::status(store, name).unwrap(), None, "{name}");
        }
    }
}

// Replication/checkpoint corpus: retain authentic legacy graph claims in this fixture.
fn append(store: &Store, subject: &str, kind: &str, actor: Option<&str>, fields: Value) {
    store
        .append_legacy_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(str::to_owned),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn running(store: &Store, subject: &str, incarnation: &str) {
    append(
        store,
        subject,
        "runtime.observed",
        Some(subject),
        json!({"status": "running", "runtime_id": subject, "incarnation_id": incarnation}),
    );
}

fn declare(store: &Store, intent: &NormalizedIntent, key: &str) -> u64 {
    let planned = store
        .mission(intent, IntentInput { kdl: key.into(), source_name: None })
        .unwrap();
    store.apply(intent, &planned.subject_tokens, key).unwrap().store_index
}

/// A run and generation that own a declared agent, so card reductions read owner states.
fn owning_run(store: &Store) -> MissionRunView {
    let source = r#"version 2
mission "orchard-crew" state="ready" {
  goal "Own a seat whose card reads its run and generation."
  step "work" { agentless }
}"#;
    let intent = crate::graph::parse_intent(source, "alder").unwrap();
    declare(store, &intent, "orchard-crew-mission");
    store
        .create_mission_run(&MissionRunRequest {
            mission: "orchard-crew".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/avery".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "orchard-crew-run".into(),
        })
        .unwrap()
}

/// Seats on host alder: plain, owned by a run, stopped by a later declaration, undeclared and
/// running, undeclared and stopped, and one with harness reports and a person's progress.
fn alder() -> Store {
    let store = Store::open_memory("alder").unwrap();
    let run = owning_run(&store);
    let mut intent = crate::graph::parse_intent(
        r#"version 2
agent "plain" { command "true" }
agent "owned" { command "true" }
agent "halted" { command "true" }"#,
        "alder",
    )
    .unwrap();
    let owned = intent.subjects.get_mut("agent/alder.owned").unwrap();
    owned.owner_run = Some(run.subject.clone());
    owned.owner_generation = Some(run.generation.clone());
    declare(&store, &intent, "seats");
    for (subject, incarnation) in [
        ("agent/alder.plain", "plain-1"),
        ("agent/alder.owned", "owned-1"),
        ("agent/alder.halted", "halted-1"),
        ("agent/drifter", "drifter-1"),
        ("agent/retired", "retired-1"),
    ] {
        running(&store, subject, incarnation);
    }
    for state in ["working", "idle"] {
        append(&store, "agent/alder.plain", "harness.observed", Some("agent/alder.plain"),
            json!({"state": state, "driver": "omp", "incarnation_id": "plain-1"}));
    }
    append(&store, "agent/retired", "runtime.observed", Some("agent/retired"),
        json!({"status": "stopped", "runtime_id": "agent/retired", "incarnation_id": "retired-1"}));
    let halted = crate::graph::parse_intent(
        "version 2\nstop \"agent/alder.halted\"\n",
        "alder",
    )
    .unwrap();
    declare(&store, &halted, "halt");
    append(&store, "agent/alder.halted", "runtime.observed", Some("agent/alder.halted"),
        json!({"status": "stopped", "runtime_id": "agent/alder.halted", "incarnation_id": "halted-1"}));
    // What the card's last activity reads: replicated and local timeline entries of the
    // incarnation, a work report the seat made, and mail it sent.
    let timeline = |entry: &str| json!({"fields": {"operation": "append", "entry_id": entry,
        "revision": 1, "role": "assistant", "entry_type": "content", "final": true,
        "body": {"media_type": "text/plain", "text": entry}, "driver": "omp",
        "incarnation_id": "plain-1", "sequence": 1}});
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        append_claim_tx(&transaction, "alder", "agent/alder.plain", "harness.timeline",
            Some("agent/alder.plain"), &timeline("replicated"), &[], None).unwrap();
        append_claim_tx(&transaction, "alder", "step-run/orchard/review", "work.progress",
            Some("agent/alder.plain"), &json!({"fields": {"summary": "halfway"}}), &[], None)
            .unwrap();
        transaction.commit().unwrap();
    }
    store.append_local_observations_for_test(&[ClaimInput {
        subject: "agent/alder.plain".into(),
        kind: "harness.timeline".into(),
        actor: Some("agent/alder.plain".into()),
        fields: serde_json::from_value(timeline("local")["fields"].clone()).unwrap(),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }]);
    append(&store, "message/orchard-note", "message.sent", None,
        json!({"from": "agent/alder.plain", "to": "agent/alder.owned", "title": "Note",
               "status": "sent"}));
    // A suspend request for the plain seat's current declaration.
    let token = current_desired_row(&store.readers.get(), "agent/alder.plain")
        .unwrap()
        .unwrap()
        .claim_id;
    store
        .append_claim(&ClaimInput {
            subject: "agent/alder.plain".into(),
            kind: "runtime.action.requested".into(),
            actor: Some("person/avery".into()),
            fields: serde_json::from_value(json!({"action": "suspend",
                "runtime_id": "alder.plain", "incarnation_id": "plain-1"}))
            .unwrap(),
            evidence: vec![token],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    // A refused admission keeps the stopped seat's harness view.
    append(&store, "agent/alder.halted", "harness.diagnostic", Some("agent/alder.halted"),
        json!({"code": "harness-admission-failed", "status": "degraded",
               "reason": "admission failed", "incarnation_id": "halted-1"}));
    // The owning generation ends: its seat leaves the current view at this snapshot.
    append(&store, &run.generation, "run-generation.superseded", Some("person/avery"),
        json!({"status": "superseded", "successor": "run-generation/orchard-next",
               "reason": "revised"}));
    store
}

/// Host birch observes alder's plain seat too: a rival origin the card must weigh.
fn birch() -> Store {
    let store = Store::open_memory("birch").unwrap();
    running(&store, "agent/alder.plain", "plain-birch");
    running(&store, "agent/birch.solo", "solo-1");
    store
}

#[test]
fn card_fold_matches_per_subject_reductions_in_any_replication_order() {
    let alder = alder();
    let birch = birch();
    assert_card_fold_parity(&alder, &snapshots(&alder));
    // The fixture reaches every per-card read it compares.
    let index = alder.index().unwrap();
    let names = ["agent/alder.plain", "agent/alder.halted"].map(str::to_owned);
    let mut reads = alder.agent_card_reads(&names, index, &HashMap::new()).unwrap();
    let plain = reads.take_harness(&alder, &names[0]).unwrap().unwrap();
    assert!(reads.last_activity_at(&alder, &names[0], Some(&plain.incarnation_id)).unwrap().is_some());
    assert!(reads.may_have_suspension(&names[0]));
    assert!(crate::suspension::current(&alder, &names[0]).unwrap().is_some());
    assert_eq!(reads.take_harness(&alder, &names[1]).unwrap().unwrap().state, "indeterminate");
    let from_alder = exchange_from(&alder, &ReplicationInventory::default());
    let from_birch = exchange_from(&birch, &ReplicationInventory::default());
    assert!(from_alder.envelopes.len() >= 3, "a meaningful envelope permutation");
    for (reverse, birch_first) in [(false, false), (true, false), (false, true), (true, true)] {
        let target = Store::open_memory("cedar").unwrap();
        let mut from_alder = from_alder.clone();
        if reverse {
            from_alder.envelopes.reverse();
        }
        if birch_first {
            receive_and_project(&target, "birch", &from_birch);
        }
        receive_and_project(&target, "alder", &from_alder);
        if !birch_first {
            receive_and_project(&target, "birch", &from_birch);
        }
        // A duplicate delivery changes nothing.
        receive_and_project(&target, "alder", &from_alder);
        assert_card_fold_parity(&target, &snapshots(&target));
        let index = target.index().unwrap();
        let status = target.agent_card_status_at(None, index, true).unwrap();
        let plain = status.subjects.iter().find(|s| s.subject == "agent/alder.plain").unwrap();
        assert!(plain.actual_origin.is_some(), "the rival origins were both read");
    }
}

#[test]
fn card_fold_matches_after_a_rolled_back_write() {
    let store = alder();
    let index = store.index().unwrap();
    let before = store.agent_card_status_at(None, index, true).unwrap();
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        append_claim_tx(
            &transaction,
            "alder",
            "agent/drifter",
            "runtime.observed",
            Some("agent/drifter"),
            &json!({"fields": {"status": "stopped", "runtime_id": "agent/drifter",
                               "incarnation_id": "drifter-1"}}),
            &[],
            None,
        )
        .unwrap();
        transaction.rollback().unwrap();
    }
    assert_eq!(store.index().unwrap(), index);
    assert_card_fold_parity(&store, &snapshots(&store));
    store.forget_current_views();
    let after = store.agent_card_status_at(None, index, true).unwrap();
    assert_eq!(
        serde_json::to_value(&after.subjects).unwrap(),
        serde_json::to_value(&before.subjects).unwrap()
    );
}

#[test]
fn card_fold_marks_unknown_kinds_and_conflicting_operations_as_the_reduction_does() {
    let store = alder();
    // Two hosts used one idempotency key for different restarts of the plain seat.
    let restart = |store: &Store, incarnation: &str| {
        store
            .append_claim(&ClaimInput {
                subject: "agent/alder.plain".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/alder.plain".into()),
                fields: serde_json::from_value(json!({"status": "running",
                    "runtime_id": "agent/alder.plain", "incarnation_id": incarnation}))
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("plain-restart".into()),
            })
            .unwrap()
    };
    restart(&store, "plain-2");
    let birch = Store::open_memory("birch").unwrap();
    restart(&birch, "plain-birch-2");
    store.import_replication("birch", &birch.export_replication(0).unwrap()).unwrap();
    // A newer member's claim kind this build does not know, about an undeclared seat.
    let cypress = Store::open_memory("cypress").unwrap();
    running(&cypress, "agent/drifter", "drifter-cypress");
    let mut batch = cypress.export_replication(0).unwrap();
    let claim = &mut batch.batches[0].claims[0];
    claim.kind = "future.seat-note".into();
    claim.id = claim_hash(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    )
    .unwrap();
    store.import_replication("cypress", &batch).unwrap();
    assert_card_fold_parity(&store, &snapshots(&store));
    let index = store.index().unwrap();
    let status = store.agent_card_status_at(None, index, true).unwrap();
    for subject in ["agent/alder.plain", "agent/drifter"] {
        let card = status.subjects.iter().find(|s| s.subject == subject).unwrap();
        assert_eq!(card.reachability, "indeterminate", "{subject}");
    }
}

#[test]
fn card_fold_matches_across_chunk_boundaries() {
    let store = Store::open_memory("alder").unwrap();
    let source = (0..300)
        .map(|n| format!("agent \"seat-{n:03}\" {{ command \"true\" }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let intent = crate::graph::parse_intent(&format!("version 2\n{source}"), "alder").unwrap();
    declare(&store, &intent, "many-seats");
    for n in (0..300).step_by(2) {
        running(&store, &format!("agent/alder.seat-{n:03}"), &format!("seat-{n}"));
    }
    for n in 0..20 {
        running(&store, &format!("agent/stray-{n:02}"), &format!("stray-{n}"));
    }
    let head = store.index().unwrap();
    assert_card_fold_parity(&store, &[head]);
}

#[test]
fn newest_claim_reads_sort_only_the_newest_millisecond() {
    let store = Store::open_memory("alder").unwrap();
    let connection = store.readers.get();
    let plan = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {}", card_fold::newest_claims_sql()))
        .unwrap()
        .query_map(params!["[]", 1], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    assert!(plan.contains("USING INDEX claims_subject_accepted_index"), "{plan}");
    // A sort of only the order's last terms ends at the first newer time block.
    assert!(plan.contains("USE TEMP B-TREE FOR LAST"), "{plan}");
}

#[test]
fn card_fold_orders_claims_that_tie_in_one_batch_by_their_position() {
    let store = alder();
    // Put the retired seat's running and stopped observations in one batch at one time, so
    // only their positions in the batch, and then their ids, order them.
    {
        let connection = store.connection.write();
        let (first_batch, accepted) = connection
            .query_row(
                "SELECT batch_id, accepted_at_unix_ms FROM claims
                 WHERE subject='agent/retired' AND kind='runtime.observed' ORDER BY store_index LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        connection
            .execute(
                "UPDATE claims SET batch_id=?1, accepted_at_unix_ms=?2
                 WHERE subject='agent/retired' AND kind='runtime.observed'",
                params![first_batch, accepted],
            )
            .unwrap();
    }
    store.forget_current_views();
    let index = store.index().unwrap();
    assert_card_fold_parity(&store, &[index]);
    let status = store.agent_card_status_at(None, index, true).unwrap();
    let retired = status.subjects.iter().find(|s| s.subject == "agent/retired").unwrap();
    assert_eq!(
        retired.actual.as_ref().unwrap()["status"],
        latest_actual_at(&store.readers.get(), "agent/retired", Some(index)).unwrap().unwrap()["status"]
    );
}
