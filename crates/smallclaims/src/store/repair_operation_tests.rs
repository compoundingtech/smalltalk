//! A successful record repair refreshes only its canonical operation dependencies.
use super::runtime::Plain;
use super::*;

type OperationRows = BTreeMap<String, (String, String, String)>;

fn node() -> Store {
    Store::open_memory("alder", Arc::new(Plain)).unwrap()
}

fn operation_claim(store: &Store, operation: &str, digest: &str, label: &str) -> ClaimRecord {
    store.connection.batched(|tx| {
        let claim = append_claim_record_tx(tx, &store.origin, &format!("note/{label}"),
            "example.note", None,
            &json!({"fields":{"label":label},"_operation":{"id":operation,"request_digest":digest}}),
            &[], None)?;
        register_operation_tx(tx, &claim)?;
        Ok::<_, anyhow::Error>(claim)
    }).unwrap().unwrap()
}

fn record(store: &Store, claim: &ClaimRecord) -> String {
    store.seal_local_batches().unwrap();
    store
        .readers
        .get()
        .query_row(
            "SELECT record_ref FROM replica_records WHERE claim_id=?1",
            [&claim.id],
            |row| row.get(0),
        )
        .unwrap()
}

fn repair(store: &Store, record: &str, replacement: &str, label: &str) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: format!("repair/{label}"),
            kind: "record.repaired".into(),
            actor: None,
            fields: BTreeMap::from([
                ("record".into(), json!(record)),
                ("replacement".into(), json!(replacement)),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

fn rows(store: &Store) -> OperationRows {
    store
        .readers
        .get()
        .prepare("SELECT id, request_digest, canonical_claim_id, state FROM operations ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn assert_canonical(store: &Store) {
    assert_eq!(
        rows(store),
        expected_operations(&store.readers.get()).unwrap()
    );
}

fn tombstone(tx: &Transaction<'_>, claim: &ClaimRecord) {
    tx.execute(
        "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
             actor,predecessors,operation_id,request_digest,accepted_at_unix_ms,checkpoint)
         VALUES (?1,?2,1,'fixture-envelope',?3,?4,NULL,'[]',?5,?6,0,'checkpoint/2026-10-01')",
        params![
            claim.id,
            claim.origin,
            claim.subject,
            claim.kind,
            claim.operation_id,
            claim.request_digest
        ],
    )
    .unwrap();
}

/// Fail even an attempted rewrite of an unrelated row, including DELETE followed by INSERT.
fn protect_unrelated(store: &Store) {
    store.connection.write().execute_batch(
        "CREATE TEMP TRIGGER unrelated_operation_delete BEFORE DELETE ON operations
         WHEN OLD.id LIKE 'unrelated/%' BEGIN SELECT RAISE(ABORT,'unrelated operation deleted'); END;
         CREATE TEMP TRIGGER unrelated_operation_update BEFORE UPDATE ON operations
         WHEN OLD.id LIKE 'unrelated/%' BEGIN SELECT RAISE(ABORT,'unrelated operation updated'); END;
         CREATE TEMP TRIGGER unrelated_operation_insert BEFORE INSERT ON operations
         WHEN NEW.id LIKE 'unrelated/%' BEGIN SELECT RAISE(ABORT,'unrelated operation inserted'); END;",
    ).unwrap();
}

#[test]
fn repair_removes_an_operation_with_no_remaining_stored_source() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let replacement = operation_claim(&store, "op/new", "b", "new");
    operation_claim(&store, "unrelated/stable", "z", "stable");
    let unrelated = rows(&store)["unrelated/stable"].clone();
    let record = record(&store, &old);
    repair(&store, &record, &replacement.id, "remove");
    protect_unrelated(&store);
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert!(!rows(&store).contains_key("op/old"));
    assert_eq!(rows(&store)["unrelated/stable"], unrelated);
    assert_canonical(&store);
}

#[test]
fn repair_recomputes_conflict_and_checkpoint_digest_without_naming_a_dropped_claim() {
    let store = node();
    let old = operation_claim(&store, "op/shared", "a", "old");
    let replacement = operation_claim(&store, "op/shared", "b", "new");
    let dropped = operation_claim(&store, "op/shared", "0", "dropped");
    let record = record(&store, &old);
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tombstone(&tx, &dropped);
        tx.execute("DELETE FROM operations WHERE id='op/shared'", [])
            .unwrap();
        tx.execute("DELETE FROM claims WHERE id=?1", [&dropped.id])
            .unwrap();
        repair_operations_tx(&tx, &["op/shared".into()]).unwrap();
        tx.commit().unwrap();
    }
    repair(&store, &record, &replacement.id, "conflict");
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert_eq!(
        rows(&store)["op/shared"],
        ("0".into(), replacement.id, "conflict".into())
    );
    assert_canonical(&store);
}

#[test]
fn repair_finds_the_original_operation_in_checkpoint_metadata() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let retained = operation_claim(&store, "op/old", "b", "retained");
    let replacement = operation_claim(&store, "op/new", "c", "new");
    let record = record(&store, &old);
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tombstone(&tx, &old);
        tx.execute("DELETE FROM operations WHERE id='op/old'", [])
            .unwrap();
        tx.execute("DELETE FROM claims WHERE id=?1", [&old.id])
            .unwrap();
        repair_operations_tx(&tx, &["op/old".into()]).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(rows(&store)["op/old"].2, "conflict");
    repair(&store, &record, &replacement.id, "checkpointed-original");
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert_eq!(
        rows(&store)["op/old"],
        ("b".into(), retained.id, "active".into())
    );
    assert_canonical(&store);
}

#[test]
fn replacement_retains_previous_operation_dependencies_and_existing_repair_order() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let first = operation_claim(&store, "op/first", "b", "first");
    let second = operation_claim(&store, "op/second", "c", "second");
    let record = record(&store, &old);
    let a = repair(&store, &record, &first.id, "first");
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    // This receipt is in the preceding replacement's dependency set, not an unrelated row.
    store
        .connection
        .write()
        .execute(
            "UPDATE operations SET request_digest='obsolete' WHERE id='op/first'",
            [],
        )
        .unwrap();
    let b = repair(&store, &record, &second.id, "second");
    assert!(store.apply_replication_repairs().unwrap() > 0);
    let selected: String = store
        .readers
        .get()
        .query_row(
            "SELECT replacement_claim_id FROM replica_records WHERE record_ref=?1",
            [&record],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(selected, if a.id > b.id { first.id } else { second.id });
    assert_canonical(&store);
}

#[test]
fn failed_repair_rolls_back_dependencies_and_health_then_retries() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let replacement = operation_claim(&store, "op/new", "b", "new");
    let record = record(&store, &old);
    let repair = repair(&store, &record, &replacement.id, "failure");
    let before = rows(&store);
    store
        .connection
        .write()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_repair_health BEFORE INSERT ON projection_health
         WHEN NEW.status='healthy' AND NEW.aggregate LIKE 'repair:%'
         BEGIN SELECT RAISE(ABORT,'injected failure after repair application'); END;",
        )
        .unwrap();
    assert_eq!(store.apply_replication_repairs().unwrap(), 0);
    assert_eq!(rows(&store), before);
    let reader = store.readers.get();
    assert_eq!(
        reader
            .query_row(
                "SELECT state FROM replica_records WHERE record_ref=?1",
                [&record],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        "valid"
    );
    assert!(
        !reader
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=?1)",
                [&old.id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
    );
    assert_eq!(
        reader
            .query_row(
                "SELECT error_code FROM projection_health WHERE aggregate=?1",
                [format!("repair:{}", repair.id)],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        "repair-failed"
    );
    drop(reader);
    store
        .connection
        .write()
        .execute_batch("DROP TRIGGER fail_repair_health")
        .unwrap();
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert_canonical(&store);
    assert!(
        !store
            .readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projection_health WHERE aggregate=?1)",
                [format!("repair:{}", repair.id)],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
    );
}

#[test]
fn repeated_and_missing_repairs_do_not_rewrite_operations() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let replacement = operation_claim(&store, "op/new", "b", "new");
    let record = record(&store, &old);
    repair(&store, &record, "missing-claim", "missing");
    assert_eq!(store.apply_replication_repairs().unwrap(), 0);
    repair(&store, &record, &replacement.id, "accepted");
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    store
        .connection
        .write()
        .execute_batch(
            "CREATE TEMP TRIGGER no_repeated_operation_write BEFORE INSERT ON operations
         BEGIN SELECT RAISE(ABORT,'repeated operation write'); END;",
        )
        .unwrap();
    assert_eq!(store.apply_replication_repairs().unwrap(), 0);
    assert_canonical(&store);
}

#[test]
fn repair_work_does_not_grow_with_unrelated_operations_or_checkpoint_receipts() {
    fn sample(unrelated: usize) -> u64 {
        let store = node();
        let old = operation_claim(&store, "op/old", "a", "old");
        let replacement = operation_claim(&store, "op/new", "b", "new");
        for n in 0..unrelated {
            let claim = operation_claim(
                &store,
                &format!("unrelated/{n}"),
                "z",
                &format!("stable-{n}"),
            );
            // Unrelated checkpoint inventory must not turn the keyed helper into a global fold.
            store
                .connection
                .batched(|tx| {
                    tombstone(tx, &claim);
                    Ok::<_, anyhow::Error>(())
                })
                .unwrap()
                .unwrap();
        }
        let record = record(&store, &old);
        repair(&store, &record, &replacement.id, "bounded");
        protect_unrelated(&store);
        // Connection-local VM progress observes the actual application/commit, with no global
        // counters that simultaneous tests could contaminate. No elapsed-time oracle.
        let work = Arc::new(AtomicU64::new(0));
        let observed = work.clone();
        store.connection.write().progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let before = work.load(Ordering::Relaxed);
        assert_eq!(store.apply_replication_repairs().unwrap(), 1);
        let spent = work.load(Ordering::Relaxed) - before;
        store
            .connection
            .write()
            .progress_handler(0, None::<fn() -> bool>);
        assert_canonical(&store);
        spent
    }
    let small = sample(16);
    let large = sample(2048);
    eprintln!("repair VM progress: unrelated operations/checkpoints 16={small}, 2048={large}");
    assert!(
        small > 0 && large <= small * 2,
        "repair grew with unrelated history: {small}->{large}"
    );
}
// Frozen legacy keyed oracle: deliberately retains every dropped candidate before folding.
fn legacy_expected_operation(
    connection: &Connection,
    operation: &str,
) -> Result<Option<(String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT id, body FROM claims WHERE json_extract(body, '$._operation.id')=?1
         AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id) ORDER BY id",
    )?;
    let mut stored = Vec::new();
    for row in statement.query_map([operation], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, body) = row?;
        let body: Value = serde_json::from_str(&body)?;
        if let Some((_, digest)) = operation_parts(&body) {
            stored.push((digest.to_owned(), id));
        }
    }
    if stored.is_empty() {
        return Ok(None);
    }
    let dropped = checkpoint::checkpointed_operation(connection, operation)?
        .into_iter()
        .filter(|(_, id)| !stored.iter().any(|(_, stored)| stored == id))
        .collect();
    Ok(Some(operation_row(&stored, dropped)))
}

fn checkpoint_candidate(connection: &Connection, operation: &str, digest: &str, id: &str) {
    connection
        .execute(
            "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,actor,
         predecessors,operation_id,request_digest,accepted_at_unix_ms,checkpoint)
         VALUES (?1,'alder',1,'fixture-envelope','note/checkpoint','example.note',NULL,
         '[]',?2,?3,0,'checkpoint/2026-10-01')",
            params![id, operation, digest],
        )
        .unwrap();
}

fn assert_keyed_oracle(connection: &Connection, operation: &str) {
    let expected = legacy_expected_operation(connection, operation).unwrap();
    assert_eq!(expected_operation(connection, operation).unwrap(), expected);
    assert_eq!(
        expected,
        expected_operations(connection)
            .unwrap()
            .get(operation)
            .cloned()
    );
}

#[test]
fn repair_checkpoint_extrema_preserve_legacy_membership_and_canonical_fallback() {
    let store = node();
    let stored = operation_claim(&store, "op/target", "b", "stored");
    operation_claim(&store, "op/target", "c", "second");
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    // Duplicate metadata cannot lower the stored claim's real digest.
    checkpoint_candidate(&tx, "op/target", "!duplicate", &stored.id);
    checkpoint_candidate(&tx, "op/target", "!repaired", "cp/repaired");
    tx.execute(
        "INSERT INTO projection_digest_repaired_claims(id) VALUES ('cp/repaired')",
        [],
    )
    .unwrap();
    checkpoint_candidate(&tx, "op/target", "ignored", "cp/null-digest");
    tx.execute("UPDATE checkpoint_claims SET request_digest=NULL WHERE id='cp/null-digest'", []).unwrap();
    assert_keyed_oracle(&tx, "op/target");
    for (n, digest) in ["b", "c", "", "é", "𐀀", "0"].into_iter().enumerate() {
        checkpoint_candidate(&tx, "op/target", digest, &format!("cp/{n}"));
        assert_keyed_oracle(&tx, "op/target");
    }
    // Both sources keep conflict refusal, even with a prior cached active receipt.
    assert_eq!(
        checkpointed_operation_outcome(&tx, "op/target", "b")
            .unwrap_err()
            .code,
        "idempotency-conflict"
    );
    tx.commit().unwrap();
}

#[test]
fn repair_checkpoint_extrema_observe_late_arrival_deletion_and_rollback() {
    let store = node();
    operation_claim(&store, "op/target", "b", "stored");
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        assert_keyed_oracle(&tx, "op/target");
        checkpoint_candidate(&tx, "op/target", "a", "cp/late");
        assert_keyed_oracle(&tx, "op/target");
        tx.execute_batch("SAVEPOINT changed_candidates").unwrap();
        tx.execute("DELETE FROM checkpoint_claims WHERE id='cp/late'", [])
            .unwrap();
        checkpoint_candidate(&tx, "op/target", "z", "cp/transient");
        assert_keyed_oracle(&tx, "op/target");
        tx.execute_batch("ROLLBACK TO changed_candidates; RELEASE changed_candidates")
            .unwrap();
        assert_keyed_oracle(&tx, "op/target");
        assert_eq!(
            expected_operation(&tx, "op/target").unwrap().unwrap().0,
            "a"
        );
        tx.commit().unwrap();
    }
    operation_claim(&store, "op/target", "0", "late-stored");
    assert_keyed_oracle(&store.readers.get(), "op/target");
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute("DELETE FROM operations WHERE id='op/target'", [])
            .unwrap();
        tx.execute(
            "DELETE FROM claims WHERE json_extract(body,'$._operation.id')='op/target'",
            [],
        )
        .unwrap();
        assert_eq!(expected_operation(&tx, "op/target").unwrap(), None);
        assert_eq!(legacy_expected_operation(&tx, "op/target").unwrap(), None);
        tx.commit().unwrap();
    }
}

#[test]
fn repair_checkpoint_extrema_refuse_corruption_and_roll_back_the_entire_repair() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let replacement = operation_claim(&store, "op/new", "b", "new");
    let record = record(&store, &old);
    repair(&store, &record, &replacement.id, "bad-checkpoint");
    {
        let connection = store.connection.write();
        checkpoint_candidate(&connection, "op/new", "a", "cp/least");
        checkpoint_candidate(&connection, "op/new", "z", "cp/greatest");
        checkpoint_candidate(&connection, "op/new", "c", "cp/bad");
        // Invalid UTF-8 in an interior TEXT ID must fail too, even though neither digest
        // extremum names it. Refusing only non-text fields would silently accept this.
        connection
            .execute(
                "UPDATE checkpoint_claims SET id=CAST(x'80' AS TEXT) WHERE id='cp/bad'",
                [],
            )
            .unwrap();
        assert!(legacy_expected_operation(&connection, "op/new").is_err());
        assert!(expected_operation(&connection, "op/new").is_err());
        connection
            .execute("DELETE FROM checkpoint_claims WHERE request_digest='c'", [])
            .unwrap();
        checkpoint_candidate(&connection, "op/new", "c", "cp/bad");
        connection
            .execute(
                "UPDATE checkpoint_claims SET id=x'80' WHERE id='cp/bad'",
                [],
            )
            .unwrap();
        assert!(legacy_expected_operation(&connection, "op/new").is_err());
        assert!(expected_operation(&connection, "op/new").is_err());
    }
    let before = rows(&store);
    assert!(store.apply_replication_repairs().is_err());
    assert_eq!(rows(&store), before);
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT state FROM replica_records WHERE record_ref=?1",
                [&record],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "valid"
    );
    store
        .connection
        .write()
        .execute("DELETE FROM checkpoint_claims WHERE typeof(id)<>'text'", [])
        .unwrap();
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert_canonical(&store);
}

#[test]
fn repair_checkpoint_extrema_do_not_bless_a_damaged_unrelated_receipt() {
    let store = node();
    let old = operation_claim(&store, "op/old", "a", "old");
    let replacement = operation_claim(&store, "op/new", "b", "new");
    operation_claim(&store, "unrelated/damaged", "z", "damaged");
    store
        .connection
        .write()
        .execute(
            "UPDATE operations SET request_digest='prior-corruption' WHERE id='unrelated/damaged'",
            [],
        )
        .unwrap();
    let damaged = rows(&store)["unrelated/damaged"].clone();
    let record = record(&store, &old);
    repair(&store, &record, &replacement.id, "keyed");
    protect_unrelated(&store);
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert_eq!(rows(&store)["unrelated/damaged"], damaged);
    assert_keyed_oracle(&store.readers.get(), "op/new");
    assert_ne!(
        rows(&store),
        expected_operations(&store.readers.get()).unwrap(),
        "targeted repair must not certify unrelated corruption"
    );
}

#[test]
fn repair_checkpoint_extrema_use_the_affected_key_as_unrelated_inventory_grows() {
    fn sample(unrelated: usize) -> crate::sqlite::work::SqliteWork {
        let store = node();
        operation_claim(&store, "op/target", "b", "stored");
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            checkpoint_candidate(&tx, "op/target", "a", "cp/least");
            checkpoint_candidate(&tx, "op/target", "z", "cp/greatest");
            for n in 0..unrelated {
                checkpoint_candidate(
                    &tx,
                    &format!("unrelated/{n}"),
                    "0",
                    &format!("cp/unrelated/{n}"),
                );
            }
            tx.commit().unwrap();
        }
        let connection = store.readers.get();
        {
            let query = format!("EXPLAIN QUERY PLAN {CHECKPOINT_OPERATION_EXTREMA_QUERY}");
            let plan = connection
                .prepare(&query)
                .unwrap()
                .query_map(["op/target"], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(plan.iter().any(|line|line.contains("SEARCH checkpoint_claims USING COVERING INDEX checkpoint_claims_operation_digest")),"{plan:?}");
            assert!(
                !plan.iter().any(|line| line.contains("USE TEMP B-TREE")),
                "{plan:?}"
            );
        }
        let scope = crate::sqlite::work::SqliteWorkScope::start();
        let result = expected_operation(&connection, "op/target").unwrap();
        let work = scope.finish();
        assert_eq!(
            result,
            legacy_expected_operation(&connection, "op/target").unwrap()
        );
        assert!(work.vm_steps > 0);
        assert_eq!(work.fullscan_steps, 0);
        work
    }
    let small = sample(16);
    let large = sample(2048);
    eprintln!("checkpoint extrema keyed work: 16={small:?},2048={large:?}");
    assert_eq!(small.statements, large.statements);
    assert!(large.vm_steps <= small.vm_steps * 2, "{small:?}->{large:?}");
}

#[test]
fn repair_checkpoint_extrema_index_is_added_on_reopen_without_receipt_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let before;
    {
        let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
        operation_claim(&store, "op/target", "b", "stored");
        checkpoint_candidate(&store.connection.write(), "op/target", "a", "cp/dropped");
        before = rows(&store);
        store
            .connection
            .write()
            .execute_batch("DROP INDEX checkpoint_claims_operation_digest")
            .unwrap();
    }
    let reopened = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert_eq!(rows(&reopened), before);
    assert_keyed_oracle(&reopened.readers.get(), "op/target");
    assert_eq!(
        reopened
            .readers
            .get()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        17
    );
    assert!(reopened.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='checkpoint_claims_operation_digest')", [], |row| row.get::<_,bool>(0)).unwrap());
}
