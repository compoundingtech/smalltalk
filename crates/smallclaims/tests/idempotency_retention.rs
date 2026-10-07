use rusqlite::{Connection, StatementStatus, params};
use smallclaims::store::{
    Store,
    idempotency::{self, CLEANUP_CHUNK, RETENTION_MS},
    runtime::Plain,
};
use std::sync::{Arc, Barrier};

fn commit_clock_anchor(store: &Store, time: i64) {
    store.set_write_clock_at(time as u128).unwrap();
    store
        .append_claim(&smallclaims::ClaimInput {
            subject: "custom/example/clock".into(),
            kind: "custom.example.clock".into(),
            actor: None,
            fields: Default::default(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn memory() -> Store {
    Store::open_memory("alder", Arc::new(Plain)).unwrap()
}

fn expiry(store: &Store) -> i64 {
    store
        .connection
        .write()
        .query_row(
            "SELECT MIN(expires_at_unix_ms) FROM idempotency",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn retention_document_concurrent_retry_restart_and_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite");
    let store = Arc::new(Store::open(&path, "alder", Arc::new(Plain)).unwrap());
    let barrier = Arc::new(Barrier::new(12));
    let workers = (0..12)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .put_document("doc/example/retention", b"first", &None, "request-one")
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let receipts = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.binding_claim_id == receipts[0].binding_claim_id)
    );
    assert_eq!(
        store.index().unwrap(),
        1,
        "concurrent retries append one binding"
    );
    let boundary = expiry(&store);
    assert_eq!(store.cleanup_idempotency(boundary - 1).unwrap().deleted, 0);
    drop(store);
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let retried = store
        .put_document("doc/example/retention", b"first", &None, "request-one")
        .unwrap();
    assert_eq!(retried.binding_claim_id, receipts[0].binding_claim_id);
    assert_eq!(expiry(&store), boundary, "retry does not renew the window");
    assert_eq!(
        store
            .cleanup_idempotency(boundary + RETENTION_MS)
            .unwrap()
            .deleted,
        0,
        "an RTC jump cannot expire a recently committed response"
    );
    commit_clock_anchor(&store, boundary);
    assert_eq!(store.cleanup_idempotency(boundary).unwrap().deleted, 1);
    let replay = store
        .put_document(
            "doc/example/retention",
            b"second",
            &Some(receipts[0].binding_claim_id.clone()),
            "request-one",
        )
        .unwrap();
    assert_eq!(
        replay.binding_claim_id, receipts[0].binding_claim_id,
        "same expired key cannot bind different bytes even with a valid current token"
    );
    assert_eq!(replay.hash, receipts[0].hash);
    assert_eq!(
        store.index().unwrap(),
        2,
        "cleanup retains durable authority"
    );
}

#[test]
fn retention_committed_request_crash_child() {
    let Ok(path) = std::env::var("ST_RETENTION_CRASH_FIXTURE") else {
        return;
    };
    let store = Store::open(std::path::Path::new(&path), "alder", Arc::new(Plain)).unwrap();
    store
        .put_document("doc/example/crash", b"committed", &None, "crash-request")
        .unwrap();
    // End without Store destructors, after commit and before the parent receives a response.
    std::process::exit(23);
}

#[test]
fn retention_recovers_after_process_exit_before_response_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retention_committed_request_crash_child",
            "--nocapture",
        ])
        .env("ST_RETENTION_CRASH_FIXTURE", &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(23));
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let receipt = store
        .put_document("doc/example/crash", b"committed", &None, "crash-request")
        .unwrap();
    assert_eq!(receipt.created_index, 1);
    assert_eq!(store.index().unwrap(), 1);
    assert_eq!(
        store
            .cleanup_idempotency(expiry(&store) - 1)
            .unwrap()
            .deleted,
        0
    );
}

#[test]
fn retention_migration_preserves_legacy_keys_and_old_insert_shape() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite");
    let original = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let receipt = original
        .put_document("doc/example/upgrade", b"old", &None, "old-key")
        .unwrap();
    drop(original);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TRIGGER idempotency_completed_expiry;
         ALTER TABLE idempotency DROP COLUMN expires_at_unix_ms;
         DELETE FROM meta WHERE key='idempotency_legacy_rowid';",
        )
        .unwrap();
    drop(connection);
    let before = smallclaims::store::now_ms() as i64;
    let upgraded = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert_eq!(upgraded.next_idempotency_expiry().unwrap(), None);
    commit_clock_anchor(&upgraded, before + RETENTION_MS * 8);
    assert_eq!(
        upgraded
            .cleanup_idempotency(before + RETENTION_MS * 8)
            .unwrap()
            .deleted,
        0
    );
    assert_eq!(
        upgraded
            .put_document("doc/example/upgrade", b"old", &None, "old-key")
            .unwrap()
            .binding_claim_id,
        receipt.binding_claim_id
    );
    upgraded
        .connection
        .write()
        .execute(
            "INSERT INTO idempotency(operation_id,response) VALUES('legacy-insert','{}')",
            [],
        )
        .unwrap();
    let inserted: i64 = upgraded
        .connection
        .write()
        .query_row(
            "SELECT expires_at_unix_ms FROM idempotency WHERE operation_id='legacy-insert'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(inserted >= before + RETENTION_MS);
}

#[test]
fn retention_cleanup_work_depends_on_chunk_not_total_population() {
    for population in [1_000, 10_000, 100_000] {
        let store = memory();
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        transaction.execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
             INSERT INTO idempotency(operation_id,response,replay_safe) SELECT printf('key-%09d',x),'{}',1 FROM n",
            [population],
        ).unwrap();
        transaction
            .execute(
                "UPDATE meta SET value=?1 WHERE key='idempotency_legacy_rowid'",
                [(population - 200).to_string()],
            )
            .unwrap();
        transaction
            .execute(
                "UPDATE idempotency SET expires_at_unix_ms=100 WHERE rowid>?1",
                [population - 200],
            )
            .unwrap();
        let mut selection = transaction
            .prepare(
                "SELECT operation_id,response FROM idempotency
                 WHERE rowid>(SELECT CAST(value AS INTEGER) FROM meta WHERE key='idempotency_legacy_rowid')
                 ORDER BY rowid LIMIT ?1",
            )
            .unwrap();
        assert_eq!(
            selection
                .query_map([CLEANUP_CHUNK], |row| row.get::<_, String>(0))
                .unwrap()
                .count(),
            CLEANUP_CHUNK
        );
        assert_eq!(selection.get_status(StatementStatus::FullscanStep), 0);
        assert!(selection.get_status(StatementStatus::VmStep) < 1_000);
        drop(selection);
        let began = std::time::Instant::now();
        let result =
            idempotency::cleanup_tx(&transaction, 100, usize::MAX, |_, _| Ok(false)).unwrap();
        eprintln!(
            "retention population={population} chunk={} held_us={}",
            result.examined,
            began.elapsed().as_micros()
        );
        assert_eq!(result.deleted, CLEANUP_CHUNK);
        assert_eq!(result.examined, CLEANUP_CHUNK);
        let remaining: usize = transaction
            .query_row("SELECT COUNT(*) FROM idempotency", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, population as usize - CLEANUP_CHUNK);
        transaction.commit().unwrap();
    }
}

#[test]
fn retention_unique_key_flood_stabilizes_at_the_published_window() {
    const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
    const DAILY_REQUESTS: usize = 96;
    let store = memory();
    for day in 0..90 {
        let now = day * DAY_MS;
        commit_clock_anchor(&store, now);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            for request in 0..DAILY_REQUESTS {
                let key = format!("flood-{day:03}-{request:03}");
                transaction
                    .execute(
                        "INSERT INTO idempotency(operation_id,response,replay_safe) VALUES(?1,'{}',1)",
                        [&key],
                    )
                    .unwrap();
                // Move the fixture clock forward without waiting three months. The separate
                // migration and boundary controls verify production deadline assignment.
                transaction
                    .execute(
                        "UPDATE idempotency SET expires_at_unix_ms=?2 WHERE operation_id=?1",
                        params![key, now + RETENTION_MS],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        loop {
            let cleanup = store.cleanup_idempotency(now).unwrap();
            assert!(cleanup.examined <= CLEANUP_CHUNK);
            if cleanup.examined < CLEANUP_CHUNK {
                break;
            }
        }
        let count: usize = store
            .connection
            .write()
            .query_row("SELECT COUNT(*) FROM idempotency", [], |row| row.get(0))
            .unwrap();
        let retained_days = (day + 1).min(RETENTION_MS / DAY_MS) as usize;
        assert_eq!(count, retained_days * DAILY_REQUESTS, "day {day}");
    }
    assert_eq!(
        store.index().unwrap(),
        90,
        "cleanup leaves the fixture's clock claims unchanged"
    );
}

#[test]
fn retention_live_receipts_do_not_starve_expired_keys_and_failure_rolls_back() {
    let store = memory();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    for index in 0..128 {
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id,response,replay_safe) VALUES(?1,?2,1)",
                params![
                    format!("key-{index:03}"),
                    if index < 64 { "live" } else { "finished" }
                ],
            )
            .unwrap();
    }
    transaction
        .execute("UPDATE idempotency SET expires_at_unix_ms=100", [])
        .unwrap();
    transaction.commit().unwrap();
    {
        let transaction = connection.transaction().unwrap();
        let result = idempotency::cleanup_tx(&transaction, 100, CLEANUP_CHUNK, |_, response| {
            Ok(response == "live")
        })
        .unwrap();
        assert_eq!(result.extended, CLEANUP_CHUNK);
        transaction.commit().unwrap();
    }
    {
        let transaction = connection.transaction().unwrap();
        let result =
            idempotency::cleanup_tx(&transaction, 100, CLEANUP_CHUNK, |_, _| Ok(false)).unwrap();
        assert_eq!(result.deleted, CLEANUP_CHUNK);
        // Drop without commit models interruption while a cleanup chunk is in flight.
    }
    let count: usize = connection
        .query_row("SELECT COUNT(*) FROM idempotency", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 128);
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        idempotency::cleanup_tx(&transaction, 100, CLEANUP_CHUNK, |_, _| Ok(false))
            .unwrap()
            .deleted,
        CLEANUP_CHUNK
    );
    transaction.commit().unwrap();
}

#[test]
fn retention_unknown_writer_receipts_are_not_reclaimed() {
    let store = memory();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO idempotency(operation_id,response) VALUES('unclassified','{}')",
            [],
        )
        .unwrap();
    transaction
        .execute("UPDATE idempotency SET expires_at_unix_ms=0", [])
        .unwrap();
    let result =
        idempotency::cleanup_tx(&transaction, 100, CLEANUP_CHUNK, |_, _| Ok(false)).unwrap();
    assert_eq!(result.deleted, 0);
    assert_eq!(result.extended, 1);
}

#[test]
fn retention_schema_keeps_existing_indexes_and_adds_none() {
    let store = memory();
    let connection = store.connection.write();
    let names = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        !names
            .iter()
            .any(|name| name == "idempotency_expiry" || name == "event_positions_subject_index")
    );
    // Both initialization and an ordinary receipt insertion leave the index inventory intact.
    connection
        .execute(
            "INSERT INTO idempotency(operation_id,response) VALUES('new-no-index','{}')",
            [],
        )
        .unwrap();
    let after = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(after, names);
}

#[test]
fn retention_expired_document_keeps_recognition_after_claim_tombstoning() {
    let store = memory();
    let first = store
        .put_document(
            "doc/example/checkpoint",
            b"original",
            &None,
            "checkpoint-key",
        )
        .unwrap();
    let boundary = expiry(&store);
    commit_clock_anchor(&store, boundary);
    assert_eq!(store.cleanup_idempotency(boundary).unwrap().deleted, 1);
    // Model the existing canonical deletion seam using the actual operation metadata,
    // not a new retry marker. Checkpoint policy/sealing is tested separately.
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    transaction.execute(
        "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,actor,predecessors,
             operation_id,request_digest,accepted_at_unix_ms,checkpoint)
         SELECT id,origin,1,'example-envelope',subject,kind,actor,predecessors,
             json_extract(body,'$._operation.id'),json_extract(body,'$._operation.request_digest'),
             accepted_at_unix_ms,'example-checkpoint' FROM claims WHERE id=?1",
        [&first.binding_claim_id],
    ).unwrap();
    smallclaims::store::events::remove_claim_tx(&transaction, &first.binding_claim_id).unwrap();
    transaction
        .execute(
            "DELETE FROM operations WHERE canonical_claim_id=?1",
            [&first.binding_claim_id],
        )
        .unwrap();
    transaction
        .execute(
            "DELETE FROM documents WHERE binding_claim_id=?1",
            [&first.binding_claim_id],
        )
        .unwrap();
    transaction
        .execute("DELETE FROM claims WHERE id=?1", [&first.binding_claim_id])
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);
    let before = store.index().unwrap();
    for _ in 0..2 {
        let error = store
            .put_document("doc/example/checkpoint", b"second", &None, "checkpoint-key")
            .unwrap_err();
        assert_eq!(error.code, "idempotency-key-expired");
        assert_eq!(error.details["claim_id"], first.binding_claim_id);
    }
    assert_eq!(store.index().unwrap(), before);
}
