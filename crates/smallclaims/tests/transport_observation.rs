use std::sync::{Arc, mpsc};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use smallclaims::store::{SCHEMA, Store, canonical_sql, runtime::Plain};
use smallclaims::store::transport_observation_index as index;
use smallclaims::sqlite::WriterConnection;

fn managed_writer(path: &std::path::Path) -> WriterConnection {
    WriterConnection::new(Connection::open(path).unwrap(), Arc::new(AtomicU64::new(0)))
}

fn observation(connection: &Connection, id: &str, origin: &str, sequence: i64, accepted: &str) {
    connection.execute(
        "INSERT INTO batches(id, origin, replica_sequence, hash, accepted_at_unix_ms)
         VALUES (?1, ?2, ?3, ?1, ?4)",
        params![id, origin, sequence, accepted],
    ).unwrap();
    connection.execute(
        "INSERT INTO claims(id, batch_id, subject, kind, origin, body, predecessors, accepted_at_unix_ms)
         VALUES (?1, ?1, 'host/peer', 'transport.observed', ?2, ?3, '[]', ?4)",
        params![id, origin, format!("{{\"sequence\":{sequence}}}"), accepted],
    ).unwrap();
}

fn original(connection: &Connection, origin: &str) -> Option<(String, String)> {
    connection.query_row(&canonical_sql(index::ORIGINAL_QUERY), params!["host/peer", origin], |row| {
        Ok((row.get(0)?, row.get(1)?))
    }).optional().unwrap()
}

fn finish(connection: &WriterConnection) {
    let mut max_hold = Duration::ZERO;
    loop {
        let page = index::advance(connection).unwrap();
        assert!(page.copied <= usize::try_from(index::PAGE_SIZE).unwrap());
        max_hold = max_hold.max(page.write_hold);
        if page.ready { break; }
    }
    println!("transport side-index maximum page writer hold: {max_hold:?}");
}

#[test]
fn transport_observation_latest_switches_from_original_to_bounded_seek_without_changing_results() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let mut connection = Connection::open(&path).unwrap();
    connection.execute_batch(SCHEMA).unwrap();
    let writer = managed_writer(&path);
    let transaction = connection.transaction().unwrap();
    for sequence in 0..3000 {
        for origin in ["local", "remote-a", "remote-b", "remote-c"] {
            observation(&transaction, &format!("{origin}-{sequence}"), origin, sequence, &(10000-sequence).to_string());
        }
    }
    // Reverse arrival order crosses a decimal-length boundary; the newest local
    // timestamp has three ties, so canonical batch sequence decides the winner.
    transaction.execute(
        "UPDATE claims SET accepted_at_unix_ms='10000' WHERE id IN ('local-1', 'local-2')", [],
    ).unwrap();
    transaction.commit().unwrap();
    assert!(!index::ready(&connection).unwrap());
    for origin in ["local", "remote-a", "remote-b", "remote-c", "absent"] {
        assert_eq!(index::latest(&connection, "host/peer", origin).unwrap(), original(&connection, origin));
    }
    let page = index::advance(&writer).unwrap();
    assert!(!page.ready);
    assert!(!index::ready(&connection).unwrap());
    assert_eq!(index::latest(&connection, "host/peer", "local").unwrap(), original(&connection, "local"));
    finish(&writer);
    assert!(index::ready(&connection).unwrap());
    for origin in ["local", "remote-a", "remote-b", "remote-c", "absent"] {
        assert_eq!(index::latest(&connection, "host/peer", origin).unwrap(), original(&connection, origin));
    }
    assert_eq!(index::latest(&connection, "host/peer", "local").unwrap(), Some(("{\"sequence\":2}".into(), "10000".into())));
    let plan = connection.prepare(&format!("EXPLAIN QUERY PLAN {}", index::indexed_query())).unwrap()
        .query_map(params!["host/peer", "local"], |row| row.get::<_, String>(3)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(plan.iter().any(|line| line.contains(
        "SEARCH observation USING COVERING INDEX transport_observation_seek (subject=? AND origin=?)"
    )), "expected a subject/origin seek: {plan:#?}");
    assert!(!plan.iter().any(|line| line.starts_with("SCAN claims") || line.starts_with("SCAN observation")), "unexpected history scan: {plan:#?}");
    assert!(!plan.iter().any(|line| line.contains("USE TEMP B-TREE FOR ORDER BY")), "unexpected full history sort: {plan:#?}");
}

#[test]
fn transport_observation_open_does_not_install_or_backfill_the_index() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    drop(Store::open(&path, "local", Arc::new(Plain)).unwrap());
    drop(Store::open(&path, "local", Arc::new(Plain)).unwrap());
    let connection = Connection::open(&path).unwrap();
    assert!(!index::ready(&connection).unwrap());
    let count: u32 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('transport_observation_index', 'transport_observation_seek', 'claims_transport_observed_latest_index')",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(count, 0, "opening must not create an index or scan historical observations");
}

#[test]
fn transport_observation_capture_precedes_backfill_and_readers_and_writers_work_between_pages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let mut foreground = Connection::open(&path).unwrap();
    foreground.execute_batch(SCHEMA).unwrap();
    let transaction = foreground.transaction().unwrap();
    for sequence in 0..400 {
        observation(&transaction, &format!("old-{sequence}"), "local", sequence, &sequence.to_string());
    }
    transaction.commit().unwrap();
    let (page_sent, page_received) = mpsc::channel();
    let (resume_sent, resume_received) = mpsc::channel();
    let build_path = path.clone();
    let builder = std::thread::spawn(move || {
        let writer = managed_writer(&build_path);
        assert!(!index::advance(&writer).unwrap().ready);
        page_sent.send(()).unwrap();
        resume_received.recv().unwrap();
        // Resume on a new connection, exactly as after daemon interruption.
        drop(writer);
        let writer = managed_writer(&build_path);
        finish(&writer);
    });
    page_received.recv().unwrap();
    assert!(!index::ready(&foreground).unwrap());
    observation(&foreground, "during-copy", "local", 401, "10000");
    // Capture also follows identity/kind changes and deletions before the cursor
    // reaches historical rows, so INSERT OR IGNORE cannot resurrect stale entries.
    foreground.execute("UPDATE claims SET id='renamed', origin='remote-a', subject='host/other' WHERE id='old-399'", []).unwrap();
    foreground.execute("UPDATE claims SET kind='example.note' WHERE id='old-398'", []).unwrap();
    foreground.execute("DELETE FROM claims WHERE id='old-397'", []).unwrap();
    assert_eq!(index::latest(&foreground, "host/peer", "local").unwrap(), original(&foreground, "local"));
    resume_sent.send(()).unwrap();
    builder.join().unwrap();
    assert!(index::ready(&foreground).unwrap());
    assert_eq!(index::latest(&foreground, "host/peer", "local").unwrap(), original(&foreground, "local"));
    {
        let mut writer = Connection::open(&path).unwrap();
        let transaction = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        transaction.execute(
            "UPDATE transport_observation_index SET subject=subject WHERE claim_id='during-copy'", [],
        ).unwrap();
        // WAL readers serve the committed index even while a write transaction
        // owns SQLite's writer.
        assert_eq!(index::latest(&foreground, "host/peer", "local").unwrap(), original(&foreground, "local"));
        transaction.rollback().unwrap();
    }
    let captured: u32 = foreground.query_row(
        "SELECT COUNT(*) FROM transport_observation_index WHERE claim_id='during-copy'", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(captured, 1);
    let mismatch: u32 = foreground.query_row(
        "SELECT COUNT(*) FROM (
         SELECT id, subject, origin, accepted_at_unix_ms FROM claims WHERE kind='transport.observed'
         EXCEPT SELECT claim_id, subject, origin, accepted_at_unix_ms FROM transport_observation_index)",
        [], |row| row.get(0),
    ).unwrap();
    let stale: u32 = foreground.query_row(
        "SELECT COUNT(*) FROM (
         SELECT claim_id, subject, origin, accepted_at_unix_ms FROM transport_observation_index
         EXCEPT SELECT id, subject, origin, accepted_at_unix_ms FROM claims WHERE kind='transport.observed')",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!((mismatch, stale), (0, 0));
    // Trigger maintenance continues after readiness, including changing kind back.
    foreground.execute("UPDATE claims SET kind='transport.observed' WHERE id='old-398'", []).unwrap();
    foreground.execute("UPDATE claims SET accepted_at_unix_ms='20000' WHERE id='old-398'", []).unwrap();
    foreground.execute("DELETE FROM claims WHERE id='during-copy'", []).unwrap();
    assert_eq!(index::latest(&foreground, "host/peer", "local").unwrap(), original(&foreground, "local"));
    // A physically missing index must turn readiness off rather than produce an
    // INDEXED BY preparation error. The original lookup remains usable.
    foreground.execute_batch("DROP INDEX transport_observation_seek").unwrap();
    assert!(!index::ready(&foreground).unwrap());
    assert_eq!(index::latest(&foreground, "host/peer", "local").unwrap(), original(&foreground, "local"));
}

#[test]
fn transport_observation_bootstrap_waits_for_managed_deferred_read_then_write_transactions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let mut connection = Connection::open(&path).unwrap();
    connection.execute_batch(SCHEMA).unwrap();
    let transaction = connection.transaction().unwrap();
    for sequence in 0..400 {
        observation(&transaction, &format!("queued-{sequence}"), "local", sequence, &sequence.to_string());
    }
    transaction.commit().unwrap();
    let writer = Arc::new(managed_writer(&path));
    // Exercise both the initial schema/trigger install and a subsequent copy page.
    for pass in 0..2 {
        let mut foreground = writer.write();
        let transaction = foreground.transaction().unwrap();
        let before: u32 = transaction.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0)).unwrap();
        assert_eq!(before, 400);
        let (started_sent, started_received) = mpsc::channel();
        let (completed_sent, completed_received) = mpsc::channel();
        let background_writer = writer.clone();
        let background = std::thread::spawn(move || {
            started_sent.send(()).unwrap();
            let page = index::advance(&background_writer);
            assert!(completed_sent.send(page).is_ok());
        });
        started_received.recv().unwrap();
        assert!(matches!(
            completed_received.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ), "a bootstrap page must not commit across a managed deferred snapshot");
        // An independent bootstrap writer would commit while this snapshot was
        // held, making this upgrade fail with SQLITE_BUSY_SNAPSHOT, not a retryable
        // busy wait. The background loan cannot run until the foreground returns.
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES ('deferred-upgrade', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [pass.to_string()],
        ).unwrap();
        transaction.commit().unwrap();
        drop(foreground);
        let page = completed_received.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert!(!page.ready);
        assert_eq!(page.copied, usize::try_from(index::PAGE_SIZE).unwrap());
        background.join().unwrap();
    }
    finish(&writer);
    assert!(index::ready(&connection).unwrap());
    assert_eq!(index::latest(&connection, "host/peer", "local").unwrap(), original(&connection, "local"));
}
