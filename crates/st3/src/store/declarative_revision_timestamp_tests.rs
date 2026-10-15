// Exercise the publication entry point rather than the timestamp helper in isolation.
fn fixture(reviewed: bool) -> (Store, MissionRunView, NormalizedIntent, MissionResponse, String) {
    let store = Store::open_memory("node").unwrap();
    // Only this in-memory writer's acceptance clock moves. The local planning clock stays real.
    store.set_write_clock_at(now_ms() + 60_000).unwrap();
    let publish = |goal: &str, key: &str| {
        let review = if reviewed { "revisions=\"human-only\"" } else { "" };
        publish_mission(
            &store,
            &format!(
                "version 2\nmission \"dated-revision\" state=\"ready\" revision-cutover=\"when-idle\" {review} {{\n goal {goal:?}\n step \"work\" {{ agentless }}\n}}"
            ),
            key,
        )
    };
    let initial = publish("Use the original goal.", "timestamp-initial");
    let run = store.create_mission_run(&MissionRunRequest {
        mission: initial.id,
        revision: None,
        workspace: "/tmp/dated-revision".into(),
        requester: Some("person/operator".into()),
        mode: None,
        inputs: BTreeMap::new(),
        idempotency_key: "timestamp-run".into(),
    }).unwrap();
    let next = publish("Use the revised goal.", "timestamp-candidate");
    // Return this writer to its real clock. Its existing own-batch acceptance floor remains.
    store.set_write_clock_offset(0).unwrap();
    let source = format!(
        "version 2\nmission-run {:?} {{\n revision \"timestamp-one\" {{\n mission {:?}\n from {:?}\n reason \"Use the current definition.\"\n }}\n}}",
        run.subject,
        format!("mission/{}@{}", next.id, next.revision),
        run.generation,
    );
    let intent = crate::graph::parse_intent(&source, "node").unwrap();
    let preview = store.mission(&intent, IntentInput { kdl: source.clone(), source_name: None }).unwrap();
    assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
    (store, run, intent, preview, source)
}

fn typed_rows(store: &Store) -> BTreeMap<String, Vec<Vec<rusqlite::types::Value>>> {
    let reader = store.readers.get();
    ["mission_runs", "run_generations"].into_iter().map(|table| {
        let columns = projection_digest::columns(&reader, table, &[]).unwrap();
        let sql = format!("SELECT {} FROM {table} ORDER BY id", columns.join(","));
        let mut statement = reader.prepare(/* sqlite-large-value: allow-for-now lv-h1e4897955eb5 owner=st3::store::declarative_revision_timestamp_tests TODO=trace-statement-and-value-representation */ &sql).unwrap();
        let rows = statement.query_map([], |row| {
            (0..columns.len()).map(|index| row.get(index)).collect::<rusqlite::Result<Vec<_>>>()
        }).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        (table.into(), rows)
    }).collect()
}

fn checked_digests(store: &Store) -> BTreeMap<String, String> {
    let reader = store.readers.get();
    let cached = projection_digest::tables(&reader).unwrap();
    // This oracle scans only the tiny isolated fixture, never a production store.
    let oracle = projection_digest::oracle(&reader).unwrap();
    ["mission_runs", "run_generations"].into_iter().map(|table| {
        assert_eq!(cached[table], oracle[table], "{table}: cached versus actual fixture rows");
        (table.into(), oracle[table].clone())
    }).collect()
}

fn run_time(store: &Store, run: &str) -> (String, String, String) {
    store.readers.get().query_row(
        "SELECT created_at_unix_ms,updated_at_unix_ms,phase FROM mission_runs WHERE id=?1",
        [run], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap()
}

#[test]
fn declarative_draining_revision_uses_accepted_time_on_origin_replica_and_replay() {
    let (source, run, intent, preview, _) = fixture(false);
    let before = run_time(&source, &run.id);
    let generations_before = typed_rows(&source)["run_generations"].clone();
    let applied = source.apply_as(
        &intent, &preview.subject_tokens, "timestamp-revision", Some("person/operator"),
    ).unwrap();
    assert!(applied.changed);
    assert!(applied.operations.iter().any(|operation| operation.action == "propose-revision:timestamp-one"));
    let proposal = source.revision_proposal_for_run(&run.id).unwrap().unwrap();
    assert_eq!(proposal.status, "draining");
    let claim = source.readers.get().query_row(
        "SELECT accepted_at_unix_ms,batch_id FROM claims WHERE subject=?1 AND kind='revision-proposal.created'",
        [&proposal.subject], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).unwrap();
    assert_eq!(Some(claim.1.as_str()), applied.batch_id.as_deref());
    // Prove the clocks are displaced before the assertion that catches the old local-now write.
    assert!(claim.0.parse::<u128>().unwrap() > now_ms() + 30_000);
    let after = run_time(&source, &run.id);
    assert_eq!(after.0, before.0, "proposal must not change creation time");
    assert_eq!(after.2, "revision-draining");
    assert_eq!(after.1, claim.0, "origin run date must come from this admitted proposal");
    assert_eq!(typed_rows(&source)["run_generations"], generations_before);
    let rows = typed_rows(&source);
    let digests = checked_digests(&source);

    let replica = Store::open_memory("peer").unwrap();
    replica.import_replication("node", &source.export_replication(0).unwrap()).unwrap();
    assert_eq!(typed_rows(&replica), rows, "same authored inputs on the replica");
    assert_eq!(checked_digests(&replica), digests);
    source.replay_replication_graph().unwrap();
    assert_eq!(typed_rows(&source), rows, "full replay keeps the origin's typed dates");
    assert_eq!(checked_digests(&source), digests);
}

#[test]
fn declarative_pending_approval_and_repeated_operation_do_not_retime_the_run() {
    for reviewed in [false, true] {
        let (store, run, intent, preview, source) = fixture(reviewed);
        let before = run_time(&store, &run.id);
        let applied = store.apply_as(
            &intent, &preview.subject_tokens, "timestamp-revision", Some("person/operator"),
        ).unwrap();
        let proposal = store.revision_proposal_for_run(&run.id).unwrap().unwrap();
        if reviewed {
            assert_eq!(proposal.status, "pending-approval");
            assert_eq!(run_time(&store, &run.id), before);
        } else {
            assert_eq!(proposal.status, "draining");
        }
        let index = store.index().unwrap();
        let rows = typed_rows(&store);
        let digests = checked_digests(&store);
        store.set_write_clock_at(now_ms() + 120_000).unwrap();
        let cached = store.apply_as(
            &intent, &preview.subject_tokens, "timestamp-revision", Some("person/operator"),
        ).unwrap();
        assert_eq!(cached.claim_ids, applied.claim_ids);
        // Preview the same parsed revision with its current token, then use a fresh request key.
        let current = store.mission(&intent, IntentInput { kdl: source, source_name: None }).unwrap();
        let repeated = store.apply_as(
            &intent, &current.subject_tokens, "timestamp-repeated-request", Some("person/operator"),
        ).unwrap();
        assert!(!repeated.changed);
        assert!(repeated.claim_ids.is_empty());
        assert_eq!(store.index().unwrap(), index);
        assert_eq!(typed_rows(&store), rows);
        assert_eq!(checked_digests(&store), digests);
    }
}

#[test]
fn declarative_revision_late_failure_rolls_back_the_proposal_and_run_date() {
    let (store, run, intent, preview, _) = fixture(false);
    let index = store.index().unwrap();
    let rows = typed_rows(&store);
    let digests = checked_digests(&store);
    // Refuse the final publication receipt, after the actual proposal and date writes.
    store.connection.write().execute_batch(&format!(
        /* sqlite-large-value: allow-for-now lv-h43168b8c0ccc owner=st3::store::declarative_revision_timestamp_tests TODO=bound-or-reference-values-and-reclamation */ "CREATE TEMP TRIGGER refuse_timestamp_receipt BEFORE INSERT ON idempotency
         WHEN NEW.operation_id='{}'
         BEGIN SELECT RAISE(ABORT,'injected late timestamp publication failure'); END;",
        opaque_cache_key("timestamp-revision"),
    )).unwrap();
    let error = store.apply_as(
        &intent, &preview.subject_tokens, "timestamp-revision", Some("person/operator"),
    ).unwrap_err();
    assert!(error.to_string().contains("injected late timestamp publication failure"), "{error}");
    assert_eq!(store.index().unwrap(), index);
    assert_eq!(typed_rows(&store), rows);
    assert_eq!(checked_digests(&store), digests);
    assert!(store.revision_proposal_for_run(&run.id).unwrap().is_none());
}
