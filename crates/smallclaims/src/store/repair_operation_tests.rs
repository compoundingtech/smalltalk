//! Keyed record repair and complete canonical operation reconciliation controls.
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

// Frozen complete-rebuild oracle; deliberately retains its DELETE-all behavior.
fn complete_rebuild_oracle(tx: &Transaction<'_>) -> Result<()> {
    tx.execute("DELETE FROM operations", [])?;
    for (id, (digest, canonical, state)) in expected_operations(tx)? {
        tx.execute(
            "INSERT INTO operations(id,request_digest,canonical_claim_id,state) VALUES(?1,?2,?3,?4)",
            params![id, digest, canonical, state],
        )?;
    }
    Ok(())
}

fn operation_rows_at(connection: &Connection) -> OperationRows {
    let mut statement = connection
        .prepare("SELECT id,request_digest,canonical_claim_id,state FROM operations ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?))))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// Include SQLite storage types and bytes without attempting UTF-8 decoding of damaged values.
fn raw_operation_rows_at(connection: &Connection) -> Vec<(i64, Vec<String>)> {
    let mut statement = connection.prepare(
        "SELECT rowid,typeof(id),hex(id),typeof(request_digest),hex(request_digest),
         typeof(canonical_claim_id),hex(canonical_claim_id),typeof(state),hex(state)
         FROM operations ORDER BY rowid",
    ).unwrap();
    statement.query_map([], |row| {
        Ok((row.get(0)?, (1_usize..9).map(|column| row.get(column)).collect::<rusqlite::Result<_>>()?))
    }).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

fn complete_oracle_rows_at(tx: &Transaction<'_>) -> OperationRows {
    tx.execute_batch("SAVEPOINT complete_operation_oracle").unwrap();
    complete_rebuild_oracle(tx).unwrap();
    let answer = operation_rows_at(tx);
    tx.execute_batch("ROLLBACK TO complete_operation_oracle; RELEASE complete_operation_oracle").unwrap();
    answer
}

fn operation_digest_at(connection: &Connection) -> (i64, String) {
    connection.query_row(
        "SELECT row_count,hex(accumulator) FROM projection_digest_state WHERE table_name='operations'",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap()
}

fn complete_oracle_digest_at(tx: &Transaction<'_>) -> (i64, String) {
    tx.execute_batch("SAVEPOINT complete_operation_digest_oracle").unwrap();
    complete_rebuild_oracle(tx).unwrap();
    let answer = operation_digest_at(tx);
    tx.execute_batch("ROLLBACK TO complete_operation_digest_oracle; RELEASE complete_operation_digest_oracle").unwrap();
    answer
}

// Explicit raw corrupted-cache construction: preserve/reinstall the exact operation
// trigger SQL, while leaving claim/checkpoint source identity protections untouched.
// Ordinary SQL cannot introduce BLOB cached values through the JSON digest triggers.
fn damage_operation_cache(store: &Store, damage: impl FnOnce(&Transaction<'_>)) {
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    let triggers: Vec<(String, String)> = {
        let mut statement = tx.prepare(
            "SELECT name,sql FROM sqlite_schema WHERE type='trigger' AND tbl_name='operations'",
        ).unwrap();
        statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap()
    };
    for (name, _) in &triggers {
        tx.execute_batch(&format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\""))).unwrap();
    }
    damage(&tx);
    for (_, sql) in &triggers {
        tx.execute_batch(sql).unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn complete_operation_reconciliation_retains_exact_rows_without_attempted_dml() {
    let store = node();
    operation_claim(&store, "op/a", "a", "a");
    operation_claim(&store, "op/b", "b", "b");
    operation_claim(&store, "op/b", "c", "conflict");
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    let oracle = complete_oracle_rows_at(&tx);
    let oracle_digest = complete_oracle_digest_at(&tx);
    let before = raw_operation_rows_at(&tx);
    tx.execute_batch(
        "CREATE TEMP TRIGGER exact_operation_insert BEFORE INSERT ON operations
         BEGIN SELECT RAISE(ABORT,'correct receipt insert attempted'); END;
         CREATE TEMP TRIGGER exact_operation_update BEFORE UPDATE ON operations
         BEGIN SELECT RAISE(ABORT,'correct receipt update attempted'); END;
         CREATE TEMP TRIGGER exact_operation_delete BEFORE DELETE ON operations
         BEGIN SELECT RAISE(ABORT,'correct receipt delete attempted'); END;",
    ).unwrap();
    rebuild_operations_tx(&tx).unwrap();
    assert_eq!(operation_rows_at(&tx), oracle);
    assert_eq!(raw_operation_rows_at(&tx), before); // Retains rowids as well as answers.
    assert_eq!(operation_digest_at(&tx), oracle_digest);
    tx.commit().unwrap();
}

#[test]
fn complete_operation_reconciliation_repairs_missing_stale_and_raw_corrupt_receipts() {
    let store = node();
    for id in ["missing", "digest", "canonical", "state", "null", "blob", "utf8", "text"] {
        operation_claim(&store, &format!("op/{id}"), "a", id);
    }
    let stable = operation_claim(&store, "unrelated/stable", "z", "stable");
    damage_operation_cache(&store, |connection| {
        connection.execute_batch(
            "DELETE FROM operations WHERE id='op/missing';
             UPDATE operations SET request_digest=x'80' WHERE id='op/digest';
             UPDATE operations SET state='conflict' WHERE id='op/state';
             UPDATE operations SET id=NULL WHERE id='op/null';
             UPDATE operations SET id=x'80' WHERE id='op/blob';
             UPDATE operations SET id=CAST(x'80' AS TEXT) WHERE id='op/utf8';
             UPDATE operations SET request_digest=CAST(x'80' AS TEXT) WHERE id='op/text';",
        ).unwrap();
        connection.execute("UPDATE operations SET canonical_claim_id=?1 WHERE id='op/canonical'", [&stable.id]).unwrap();
        connection.execute("INSERT INTO operations VALUES('stale/extra','z',?1,'active')", [&stable.id]).unwrap();
    });
    let oracle = {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        (complete_oracle_rows_at(&tx), complete_oracle_digest_at(&tx))
    };
    protect_unrelated(&store);
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        assert!(tx.query_row("SELECT COUNT(*) FROM operations WHERE id IS NULL", [], |r| r.get::<_, i64>(0)).unwrap() > 0);
        rebuild_operations_tx(&tx).unwrap();
        assert_eq!(operation_rows_at(&tx), oracle.0);
        assert_eq!(operation_digest_at(&tx), oracle.1);
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM operations WHERE typeof(id)<>'text'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        let foreign_key_failures: i64 = tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
        assert_eq!(foreign_key_failures, 0);
        tx.commit().unwrap();
    }
    assert_canonical(&store);
}

#[test]
fn complete_operation_reconciliation_preserves_repaired_checkpoint_and_live_precedence() {
    let store = node();
    let live = operation_claim(&store, "op/live", "c", "live");
    let dropped = operation_claim(&store, "op/live", "a", "dropped");
    let repaired = operation_claim(&store, "op/repaired", "a", "repaired");
    let checkpoint_only = operation_claim(&store, "op/checkpoint-only", "b", "checkpoint-only");
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    tombstone(&tx, &live); // Duplicate live ID must not contribute twice/as a checkpoint.
    tombstone(&tx, &dropped);
    tombstone(&tx, &checkpoint_only);
    tx.execute("DELETE FROM operations WHERE id IN ('op/live','op/checkpoint-only')", []).unwrap();
    tx.execute("DELETE FROM claims WHERE id IN (?1,?2)", params![dropped.id, checkpoint_only.id]).unwrap();
    tx.execute("INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)", [&repaired.id]).unwrap();
    let oracle = complete_oracle_rows_at(&tx);
    rebuild_operations_tx(&tx).unwrap();
    assert_eq!(operation_rows_at(&tx), oracle);
    assert_eq!(oracle["op/live"], ("a".into(), live.id, "conflict".into()));
    assert!(!oracle.contains_key("op/repaired"));
    assert!(!oracle.contains_key("op/checkpoint-only"));
    tx.commit().unwrap();
}

#[test]
fn complete_operation_reconciliation_refuses_malformed_source_before_receipt_dml() {
    let store = node();
    operation_claim(&store, "op/live", "c", "live");
    let dropped = operation_claim(&store, "op/live", "a", "dropped");
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    tombstone(&tx, &dropped);
    tx.execute("DELETE FROM operations WHERE id='op/live'", []).unwrap();
    tx.execute("DELETE FROM claims WHERE id=?1", [&dropped.id]).unwrap();
    tx.execute("UPDATE checkpoint_claims SET request_digest=CAST(x'80' AS TEXT) WHERE id=?1", [&dropped.id]).unwrap();
    let before = raw_operation_rows_at(&tx);
    tx.execute_batch("SAVEPOINT malformed_source_oracle").unwrap();
    assert!(complete_rebuild_oracle(&tx).is_err());
    tx.execute_batch("ROLLBACK TO malformed_source_oracle; RELEASE malformed_source_oracle").unwrap();
    let changes_before = tx.total_changes();
    assert!(rebuild_operations_tx(&tx).is_err());
    assert_eq!(tx.total_changes(), changes_before);
    assert_eq!(raw_operation_rows_at(&tx), before);
    tx.rollback().unwrap();
}

#[test]
fn complete_operation_reconciliation_rolls_back_late_failure_and_keeps_fk_semantics() {
    let store = node();
    operation_claim(&store, "op/a", "a", "a");
    operation_claim(&store, "op/z", "z", "z");
    {
        let connection = store.connection.write();
        connection.execute("UPDATE operations SET request_digest='wrong'", []).unwrap();
    }
    let before = raw_operation_rows_at(&store.readers.get());
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute_batch("CREATE TEMP TRIGGER fail_late_operation BEFORE INSERT ON operations
            WHEN NEW.id='op/z' BEGIN SELECT RAISE(ABORT,'late insert failure'); END;").unwrap();
        assert!(rebuild_operations_tx(&tx).is_err());
        assert_eq!(tx.query_row("SELECT request_digest FROM operations WHERE id='op/a'", [], |r| r.get::<_, String>(0)).unwrap(), "a");
        tx.rollback().unwrap();
    }
    assert_eq!(raw_operation_rows_at(&store.readers.get()), before);
    // Real deferred FK failure at the outer COMMIT must roll back receipt reconciliation too.
    {
        let mut connection = store.connection.write();
        connection.execute_batch("CREATE TABLE operation_commit_parent(id INTEGER PRIMARY KEY);
            CREATE TABLE operation_commit_child(id INTEGER REFERENCES operation_commit_parent(id) DEFERRABLE INITIALLY DEFERRED);").unwrap();
        let tx = connection.transaction().unwrap();
        rebuild_operations_tx(&tx).unwrap();
        tx.execute("INSERT INTO operation_commit_child VALUES(999)", []).unwrap();
        assert!(tx.commit().is_err());
    }
    assert_eq!(raw_operation_rows_at(&store.readers.get()), before);
}

#[test]
fn complete_operation_reconciliation_finalizer_refusal_prevents_success_ack() {
    let store = node();
    operation_claim(&store, "op/a", "a", "a");
    store.connection.write().execute("UPDATE operations SET request_digest='wrong'", []).unwrap();
    let before = raw_operation_rows_at(&store.readers.get());
    let finalized = Arc::new(AtomicBool::new(false));
    let seen = finalized.clone();
    store.connection.install_transaction_finalizer(move |tx| {
        assert_eq!(operation_rows_at(tx), expected_operations(tx)?);
        seen.store(true, Ordering::SeqCst);
        anyhow::bail!("operation fixture finalizer refuses outer commit")
    }).unwrap();
    let result = store.connection.batched(|tx| rebuild_operations_tx(tx));
    assert!(result.is_err()); // Outer commit/finalizer refusal, not a successful job ACK.
    assert!(finalized.load(Ordering::SeqCst));
    assert_eq!(raw_operation_rows_at(&store.readers.get()), before);
}

#[test]
fn complete_operation_reconciliation_characterizes_inventory_and_all_changed_dml() {
    let mut unchanged_statement_count = None;
    for count in [16, 256] {
        let store = node();
        for n in 0..count {
            operation_claim(&store, &format!("op/{n}"), "a", &format!("claim-{n}"));
        }
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        // Count operation DML separately from projection-digest trigger writes.
        tx.execute_batch("CREATE TEMP TABLE operation_dml_audit(action TEXT);
            CREATE TEMP TRIGGER operation_audit_insert AFTER INSERT ON operations
            BEGIN INSERT INTO operation_dml_audit VALUES('insert'); END;
            CREATE TEMP TRIGGER operation_audit_delete AFTER DELETE ON operations
            BEGIN INSERT INTO operation_dml_audit VALUES('delete'); END;
            CREATE TEMP TRIGGER operation_audit_update AFTER UPDATE ON operations
            BEGIN INSERT INTO operation_dml_audit VALUES('update'); END;").unwrap();
        let operation_changes = || tx.query_row("SELECT COUNT(*) FROM operation_dml_audit", [], |r| r.get::<_, i64>(0)).unwrap();
        tx.execute_batch("SAVEPOINT old_full_operation_dml").unwrap();
        let start = operation_changes();
        let scope = crate::sqlite::work::SqliteWorkScope::start();
        complete_rebuild_oracle(&tx).unwrap();
        let old_work = scope.finish();
        let old_changes = operation_changes() - start;
        tx.execute_batch("ROLLBACK TO old_full_operation_dml; RELEASE old_full_operation_dml").unwrap();
        let start = operation_changes();
        let scope = crate::sqlite::work::SqliteWorkScope::start();
        rebuild_operations_tx(&tx).unwrap();
        let unchanged_work = scope.finish();
        let unchanged_changes = operation_changes() - start;
        tx.execute("UPDATE operations SET request_digest='wrong'", []).unwrap();
        let start = operation_changes();
        let scope = crate::sqlite::work::SqliteWorkScope::start();
        rebuild_operations_tx(&tx).unwrap();
        let all_changed_work = scope.finish();
        let all_changed_changes = operation_changes() - start;
        assert_eq!(old_changes, 2 * count);
        assert_eq!(unchanged_changes, 0);
        assert_eq!(all_changed_changes, 2 * count);
        // Two complete source queries plus one joined raw/cache discovery query.
        // VM/scans remain inventory-sized; a constant statement count is not bounded work.
        assert_eq!(unchanged_work.statements, 3);
        if let Some(previous) = unchanged_statement_count {
            assert_eq!(unchanged_work.statements, previous);
        }
        unchanged_statement_count = Some(unchanged_work.statements);
        for work in [old_work, unchanged_work, all_changed_work] {
            assert!(work.vm_steps > 0);
        }
        assert!(old_work.statements > unchanged_work.statements);
        assert!(all_changed_work.statements > unchanged_work.statements);
        assert_eq!(operation_rows_at(&tx), expected_operations(&tx).unwrap());
        eprintln!("operation source characterization: inventory={count}, old_dml_rows={old_changes}, unchanged_dml_rows={unchanged_changes}, all_changed_dml_rows={all_changed_changes}; complete source/actual scans and memory remain inventory-sized");
        // Same-thread completed-statement accounting excludes setup, damage, SAVEPOINT,
        // rollback and assertion queries. DML phases include the TEMP operation audit
        // triggers as well as normal projection-digest triggers; not production timings.
        eprintln!("operation SQLite characterization: inventory={count}, old={old_work:?}, unchanged={unchanged_work:?}, all_changed={all_changed_work:?}; statements/VM/fullscan/sorts/autoindex are separate from operation-DML rows; audited helper scope excludes outer COMMIT/finalization/writer return");
        tx.rollback().unwrap();
    }
}

fn logical_operation_cache_at(connection: &Connection) -> BTreeMap<String, String> {
    let mut statement = connection.prepare(
        "SELECT operation_id,row_json FROM projection_digest_operation_rows ORDER BY operation_id",
    ).unwrap();
    statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap()
}

#[test]
fn complete_operation_reconciliation_repairs_missing_or_wrong_known_key_logical_cache() {
    for missing in [true, false] {
        let store = node();
        let claim = operation_claim(&store, "op/known", "a", "known");
        operation_claim(&store, "unrelated/stable", "z", "stable");
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        let canonical_rows = operation_rows_at(&tx);
        let old: String = tx.query_row(
            "SELECT row_json FROM projection_digest_operation_rows WHERE operation_id='op/known'",
            [], |r| r.get(0),
        ).unwrap();
        // This fixture changes a known-key logical row AND the corresponding count/sum.
        // It does not model arbitrary physical damage or an unrelated corrupt accumulator.
        if missing {
            tx.execute(
                "UPDATE projection_digest_state SET row_count=row_count-1,
                 accumulator=st_projection_change(accumulator,table_name,columns_json,?1,NULL,1)
                 WHERE table_name='operations'", [&old],
            ).unwrap();
            tx.execute("DELETE FROM projection_digest_operation_rows WHERE operation_id='op/known'", []).unwrap();
        } else {
            let wrong: String = tx.query_row(
                "SELECT json_array(?1,'op/known','wrong','conflict')", [&claim.id], |r| r.get(0),
            ).unwrap();
            tx.execute(
                "UPDATE projection_digest_state SET
                 accumulator=st_projection_change(accumulator,table_name,columns_json,?1,?2,1)
                 WHERE table_name='operations'", params![old, wrong],
            ).unwrap();
            tx.execute(
                "UPDATE projection_digest_operation_rows SET row_json=?1 WHERE operation_id='op/known'",
                [&wrong],
            ).unwrap();
        }
        assert_eq!(operation_rows_at(&tx), canonical_rows); // Actual rows were already correct.
        tx.execute_batch("SAVEPOINT known_key_cache_oracle").unwrap();
        complete_rebuild_oracle(&tx).unwrap();
        let oracle = (operation_rows_at(&tx), logical_operation_cache_at(&tx), operation_digest_at(&tx));
        tx.execute_batch("ROLLBACK TO known_key_cache_oracle; RELEASE known_key_cache_oracle").unwrap();
        tx.execute_batch(
            "CREATE TEMP TRIGGER cache_stable_delete BEFORE DELETE ON operations
             WHEN OLD.id='unrelated/stable' BEGIN SELECT RAISE(ABORT,'stable cache row deleted'); END;
             CREATE TEMP TRIGGER cache_stable_insert BEFORE INSERT ON operations
             WHEN NEW.id='unrelated/stable' BEGIN SELECT RAISE(ABORT,'stable cache row inserted'); END;
             CREATE TEMP TRIGGER cache_stable_update BEFORE UPDATE ON operations
             WHEN OLD.id='unrelated/stable' BEGIN SELECT RAISE(ABORT,'stable cache row updated'); END;",
        ).unwrap();
        rebuild_operations_tx(&tx).unwrap();
        assert_eq!(operation_rows_at(&tx), oracle.0);
        assert_eq!(logical_operation_cache_at(&tx), oracle.1);
        assert_eq!(operation_digest_at(&tx), oracle.2);
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projection_digest_operation_rows
             WHERE operation_id='op/known' AND row_json IS json_array(?1,'op/known','a','active'))",
            [&claim.id], |r| r.get(0),
        ).unwrap();
        assert!(valid);
        tx.commit().unwrap();
    }
}
