use super::*;
use super::super::tests::{exchange_from, receive_and_project};
use proptest::prelude::*;

const SUBJECT: &str = "resource/reuse-alder";

fn append(store: &Store, subject: &str, at: u128, fields: Value) -> String {
    store.set_write_clock_at(at).unwrap();
    let mut writer = store.connection.write();
    let transaction = writer.transaction().unwrap();
    let claim = append_claim_tx(&transaction, &store.origin, subject, "resource.observed",
        None, &json!({"fields":fields}), &[], None).unwrap();
    transaction.commit().unwrap();
    claim.id
}

fn read(store: &Store) -> (Option<Value>, u64) {
    let fence = resource_preparation::Fence::new(store);
    let prepared = fence.prepare_with_actuals(store, |connection, serial| {
        let work = smallclaims::sqlite::work::SqliteWorkScope::start();
        let value = store.smalltalk.observer_actuals.read(connection, SUBJECT, serial.unwrap()).unwrap();
        let statements = work.finish().statements;
        assert_eq!(value, latest_actual(connection, SUBJECT).unwrap());
        (value, statements)
    });
    assert!(fence.current(store, &store.connection.write(), &prepared));
    prepared.value.unwrap()
}

#[test]
fn observer_actuals_reuse_survives_other_subject_writes_without_history_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("store.db"), "alder").unwrap();
    // Establish replication health before warming reductions, as the daemon's initial
    // projection pass does. This test measures subsequent incremental replication, not replay.
    store.project_replication_backlog_with_log("observer-reuse/setup", |_| {}).unwrap();
    append(&store, SUBJECT, 10, json!({"facts":{"state":"open"},"carried":"kept"}));
    let first = read(&store);
    assert!(first.1 > 0);
    append(&store, "resource/other", 20, json!({"facts":{"state":"closed"}}));
    let second = read(&store);
    assert_eq!(second.0, first.0);
    assert_eq!(second.1, 0, "unchanged subject must execute no history SQL");
    assert_eq!(read(&store).1, 0);
    let remote = Store::open_memory("birch").unwrap();
    append(&remote, "resource/replicated-other", 30, json!({"facts":{"state":"open"}}));
    receive_and_project(&store, "birch", &exchange_from(&remote, &ReplicationInventory::default()));
    assert_eq!(read(&store).1, 0, "unrelated replication must preserve the reduction");
}

#[test]
fn observer_actuals_does_not_publish_a_reduction_from_an_invalidated_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("store.db"), "alder").unwrap();
    append(&store, SUBJECT, 10, json!({"facts":{"state":"open"}}));
    read(&store);
    let fence = resource_preparation::Fence::new(&store);
    let stale = fence.prepare_with_actuals(&store, |connection, serial| {
        std::thread::scope(|scope| {
            scope.spawn(|| append(&store, SUBJECT, 20, json!({"facts":{"state":"closed"}})))
                .join().unwrap();
        });
        store.smalltalk.observer_actuals.read(connection, SUBJECT, serial.unwrap()).unwrap()
    });
    assert!(!fence.current(&store, &store.connection.write(), &stale));
    assert_eq!(read(&store).0.unwrap()["facts"]["state"], "closed");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn observer_actuals_equal_full_fold_across_late_arrivals_repairs_and_nulls(
        reverse in any::<bool>(), nullable in any::<bool>(),
    ) {
        let directory = tempfile::tempdir().unwrap();
        let target = Store::open(&directory.path().join("store.db"), "cedar").unwrap();
        let old = Store::open_memory("alder").unwrap();
        let new = Store::open_memory("birch").unwrap();
        let old_id = append(&old, SUBJECT, 10, json!({"facts":{"state":"open"},"carried":"kept"}));
        let new_id = append(&new, SUBJECT, 20, json!({"facts":{"state":"closed"},
            "nullable":if nullable { Value::Null } else { json!("set") }}));
        prop_assert_eq!(read(&target).0, None);
        let sources = if reverse { [&new, &old] } else { [&old, &new] };
        for source in sources {
            receive_and_project(&target, &source.origin, &exchange_from(source, &ReplicationInventory::default()));
            read(&target);
            prop_assert_eq!(read(&target).1, 0);
        }
        let actual = read(&target).0.unwrap();
        prop_assert_eq!(&actual["carried"], &json!("kept"));
        prop_assert_eq!(&actual["nullable"], &if nullable { Value::Null } else { json!("set") });
        // A named repair excludes an original already represented in a warm reduction.
        target.connection.write().execute(
            "INSERT INTO projection_digest_repaired_claims(id) VALUES (?1)", [&new_id]).unwrap();
        let repaired = read(&target);
        prop_assert!(repaired.1 > 0);
        prop_assert_eq!(&repaired.0.unwrap()["facts"]["state"], &json!("open"));
        // Undo and direct source mutations are conservatively global invalidations.
        target.connection.write().execute(
            "DELETE FROM projection_digest_repaired_claims WHERE id=?1", [&new_id]).unwrap();
        read(&target);
        target.connection.write().execute(
            "UPDATE claims SET body=json_set(body,'$.fields.carried','changed') WHERE id=?1", [&old_id]).unwrap();
        prop_assert_eq!(&read(&target).0.unwrap()["carried"], &json!("changed"));
    }
}

#[test]
fn observer_actuals_external_writer_invalidates_warm_result() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.db");
    let store = Store::open(&path, "alder").unwrap();
    let id = append(&store, SUBJECT, 10, json!({"facts":{"state":"open"},"carried":"kept"}));
    read(&store);
    assert_eq!(read(&store).1, 0);
    let external = Connection::open(&path).unwrap();
    external.execute("UPDATE claims SET body=json_set(body,'$.fields.carried','external') WHERE id=?1", [&id]).unwrap();
    let changed = read(&store);
    assert!(changed.1 > 0);
    assert_eq!(changed.0.unwrap()["carried"], "external");
}

#[test]
fn observer_actuals_lru_is_bounded() {
    let cache = Actuals::default();
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE batches(id TEXT PRIMARY KEY,origin TEXT,replica_sequence INTEGER);
        CREATE TABLE claims(store_index INTEGER PRIMARY KEY,id TEXT,batch_id TEXT,subject TEXT,kind TEXT,body TEXT,accepted_at_unix_ms TEXT);
        CREATE INDEX claims_subject_kind_index ON claims(subject,kind,store_index);
        CREATE TABLE replica_records(claim_id TEXT,position INTEGER);
        CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY);").unwrap();
    let serial = cache.seed(&connection, || cache.coverage_verified()).unwrap();
    for index in 0..LIMIT + 10 {
        cache.read(&connection, &format!("resource/{index}"), serial).unwrap();
    }
    let state = cache.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(state.entries.len(), LIMIT);
    assert!(state.bytes <= BYTE_LIMIT);
}

#[test]
fn observer_actuals_unsupported_coverage_stays_disabled_until_verified_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("store.db"), "alder").unwrap();
    let id = append(&store, SUBJECT, 10, json!({"facts":{"state":"open"}}));
    read(&store);
    assert_eq!(read(&store).1, 0);
    store.connection.write().execute_batch(
        "DROP TABLE projection_digest_repaired_claims;
         CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY) WITHOUT ROWID;"
    ).unwrap();
    assert!(read(&store).1 > 0);
    assert!(read(&store).1 > 0, "seed must not clear failed native coverage");
    store.connection.write().execute(
        "INSERT INTO projection_digest_repaired_claims VALUES (?1)", [&id]).unwrap();
    let repaired = read(&store);
    assert_eq!(repaired.0, None, "WITHOUT ROWID repairs must not reuse stale state");
    assert!(repaired.1 > 0);
    store.connection.write().execute_batch(
        "DROP TABLE projection_digest_repaired_claims;
         CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY);"
    ).unwrap();
    assert!(read(&store).1 > 0);
    assert_eq!(read(&store).1, 0, "supported coverage was positively reverified");
}

// SQLite does not report the implicit deletion of a replacing write to the update hook.
// Production fold-source writers must therefore remain ordinary DML (including DO UPDATE).
fn replaces_fold_source(source: &str) -> bool {
    let watched = Actuals::default();
    let is_watched = |name: &str| watched.tables().iter().any(|(_, table)| *table == name);
    for statement in source.split(';') {
        let words = statement.split(|character: char|
            !character.is_ascii_alphanumeric() && character != '_')
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        for (position, pair) in words.windows(2).enumerate() {
            if pair[0] == "replace" && pair[1] == "into" {
                let mut target = position + 2;
                if words.get(target).is_some_and(|word| matches!(word.as_str(), "main" | "temp")) {
                    target += 1;
                }
                if words.get(target).is_some_and(|word| is_watched(word)) {
                    return true;
                }
            }
            if pair[0] == "create" && pair[1] == "table" {
                let mut target = position + 2;
                if words.get(target).is_some_and(|word| word == "if") {
                    target += 3;
                }
                if words.get(target).is_some_and(|word| word == "main") {
                    target += 1;
                }
                if words.get(target).is_some_and(|word| is_watched(word))
                    && words[target..].windows(3).any(|clause|
                        clause[0] == "on" && clause[1] == "conflict" && clause[2] == "replace")
                {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn observer_actuals_production_sources_forbid_implicit_replacement_deletes() {
    // Exercise both replacing spellings and quoted/schema-qualified names without putting
    // an actual replacing source-table SQL literal into the source being guarded.
    for (_, table) in Actuals::default().tables() {
        for prefix in [["INSERT", "OR", "REPLACE", "INTO"].join(" "), ["REPLACE", "INTO"].join(" ")] {
            assert!(replaces_fold_source(&format!("{prefix} main.\"{table}\" VALUES(1)")));
        }
        assert!(replaces_fold_source(&format!(
            "CREATE TABLE {table}(id TEXT UNIQUE ON CONFLICT REPLACE)"
        )));
        assert!(!replaces_fold_source(&format!(
            "INSERT INTO {table}(id) VALUES(1) ON CONFLICT(id) DO UPDATE SET id=excluded.id"
        )));
    }
    let manifest = std::path::Path::new(test_env!("CARGO_MANIFEST_DIR"));
    let mut pending = vec![manifest.join("src"), manifest.parent().unwrap().join("smallclaims/src")];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Adversarial mutation fixtures intentionally exercise unsupported SQL.
            if name == "tests" || name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                assert!(!replaces_fold_source(&source),
                    "implicit replacement deletes on fold-source tables are unsupported: {}", path.display());
            }
        }
    }
}

#[test]
fn observer_actuals_external_ddl_requires_verified_physical_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.db");
    let store = Store::open(&path, "alder").unwrap();
    let id = append(&store, SUBJECT, 10, json!({"facts":{"state":"open"}}));
    read(&store);
    assert_eq!(read(&store).1, 0);
    let external = Connection::open(&path).unwrap();
    external.execute_batch(
        "DROP TABLE projection_digest_repaired_claims;
         CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY) WITHOUT ROWID;"
    ).unwrap();
    assert!(read(&store).1 > 0);
    assert!(read(&store).1 > 0, "external unsupported DDL must latch reuse off");
    external.execute(
        "INSERT INTO projection_digest_repaired_claims VALUES (?1)", [&id]).unwrap();
    let repaired = read(&store);
    assert_eq!(repaired.0, None);
    assert!(repaired.1 > 0);
    assert!(read(&store).1 > 0, "external repair writes must not bypass the coverage latch");
    external.execute_batch(
        "DROP TABLE projection_digest_repaired_claims;
         CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY);"
    ).unwrap();
    assert!(read(&store).1 > 0);
    assert_eq!(read(&store).1, 0, "external recovery requires positive dispatcher verification");
}

#[test]
fn observer_actuals_sealing_preserves_ranks_but_late_rank_changes_invalidate() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("store.db"), "alder").unwrap();
    let first = append(&store, SUBJECT, 10, json!({"facts":{"state":"open"}}));
    let second = {
        let mut writer = store.connection.write();
        let batch: String = writer.query_row(
            "SELECT batch_id FROM claims WHERE id=?1", [&first], |row| row.get(0)).unwrap();
        let transaction = writer.transaction().unwrap();
        let claim = append_claim_tx(&transaction, &store.origin, SUBJECT, "resource.observed",
            None, &json!({"fields":{"facts":{"state":"closed"}}}), &[], Some(&batch)).unwrap();
        transaction.commit().unwrap();
        claim.id
    };
    assert_eq!(read(&store).0.unwrap()["facts"]["state"], "closed");
    store.seal_local_batches().unwrap();
    assert_eq!(read(&store).1, 0, "equivalent explicit ranks must preserve the reduction");
    // Synthetic canonical metadata, not admitted replication evidence: the second claim
    // gets an earlier position than the first while the claim bodies remain unchanged.
    store.connection.write().execute(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,
             claim_id,updated_at_unix_ms)
         VALUES('record/late-rank','fixture',1,'fixture',-1,X'','valid',?1,'0')",
        [&second]).unwrap();
    let changed = read(&store);
    assert!(changed.1 > 0);
    assert_eq!(changed.0.unwrap()["facts"]["state"], "open");
}

#[test]
fn observer_actuals_full_replay_invalidates_a_warm_changed_subject() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("store.db"), "alder").unwrap();
    store.project_replication_backlog_with_log("observer-reuse/setup", |_| {}).unwrap();
    append(&store, SUBJECT, 10, json!({"facts":{"state":"open"}}));
    let old = read(&store).0;
    assert_eq!(read(&store).1, 0);
    // Admit a changed source without running the runtime projector, as replication admission
    // does before projection. Warm its actual fold so replay itself must discard a live entry.
    store.set_write_clock_at(20).unwrap();
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        smallclaims::store::append_claim_record_tx(
            &transaction, &store.origin, SUBJECT, "resource.observed", None,
            &json!({"fields":{"facts":{"state":"closed"}}}), &[], None,
        ).unwrap();
        transaction.commit().unwrap();
    }
    let admitted = read(&store).0;
    assert_ne!(admitted, old);
    assert_eq!(admitted.as_ref().unwrap()["facts"]["state"], "closed");
    assert_eq!(read(&store).1, 0);
    store.connection.write().execute(
        "DELETE FROM projection_health WHERE aggregate='graph'", []).unwrap();
    assert_eq!(read(&store).1, 0, "health deletion alone does not mutate the actual fold");
    let mut logs = Vec::new();
    assert!(store.project_replication_backlog_with_log(
        "observer-reuse/forced-replay", |line| logs.push(line.to_owned()),
    ).unwrap());
    assert!(logs.iter().any(|line| line.contains("reason=missing-health")),
        "fixture must exercise full replay: {logs:?}");
    let replayed = read(&store);
    assert_eq!(replayed.0, admitted);
    assert_ne!(replayed.0, old);
    assert!(replayed.1 > 0, "full replay must discard even a warm changed-subject reduction");
    assert_eq!(read(&store).1, 0);
}
