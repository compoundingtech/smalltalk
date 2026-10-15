//! Invented sources for the production actual-state reader. Synthetic SQL rows are not
//! admitted replication evidence; the existing actual-purity replication controls remain.
use super::*;

const SUBJECT: &str = "resource/sorter-alder";

fn synthetic_source() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            /* sqlite-large-value: allow-for-now lv-hd810e77f1aef owner=st3::store::actual_state_sorter_tests TODO=chunk-or-reference-variable-columns */ "CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT, replica_sequence INTEGER);
         CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT,
             id TEXT UNIQUE, batch_id TEXT, subject TEXT, kind TEXT, body TEXT,
             accepted_at_unix_ms TEXT);
         CREATE INDEX claims_subject_kind_index ON claims(subject, kind, store_index);
         CREATE TABLE replica_records(claim_id TEXT, position INTEGER);
         CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY);
         INSERT INTO batches VALUES ('batch/alder', 'alder', 7), ('batch/birch', 'birch', 1);",
        )
        .unwrap();
    connection
}

fn synthetic_row(
    connection: &Connection,
    id: &str,
    batch: &str,
    time: &str,
    position: u64,
    kind: &str,
    body: &Value,
) -> u64 {
    connection
        .execute(
            /* sqlite-large-value: allow-for-now lv-h2cb420656a31 owner=st3::store::actual_state_sorter_tests TODO=prepare-body-and-predecessor-values */ "INSERT INTO claims(id,batch_id,subject,kind,body,accepted_at_unix_ms)
         VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, batch, SUBJECT, kind, body.to_string(), time],
        )
        .unwrap();
    let index = connection.last_insert_rowid() as u64;
    connection
        .execute(
            /* sqlite-large-value: allow-for-now lv-hc58ae0f1778c owner=st3::store::actual_state_sorter_tests TODO=chunk-or-reference-exact-wire-values */ "INSERT INTO replica_records(claim_id,position) VALUES (?1,?2)",
            params![id, position],
        )
        .unwrap();
    index
}

#[test]
fn actual_identity_stream_keeps_canonical_wire_order_and_field_carriers() {
    let values = [
        (
            "old",
            "batch/alder",
            "9",
            0,
            json!({"fields":{"phase":"old","carried":"kept"}}),
        ),
        (
            "middle",
            "batch/birch",
            "10",
            0,
            json!({"fields":{"phase":"middle"}}),
        ),
        (
            "new",
            "batch/birch",
            "10",
            1,
            json!({"fields":{"phase":"new","nullable":null}}),
        ),
    ];
    for order in [[0, 1, 2], [2, 1, 0]] {
        let connection = synthetic_source();
        for n in order {
            let (id, batch, time, position, body) = &values[n];
            synthetic_row(
                &connection,
                id,
                batch,
                time,
                *position,
                "resource.observed",
                body,
            );
        }
        let arrival = connection
            .prepare("SELECT id FROM claims ORDER BY store_index")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let canonical = connection
            .prepare(/* sqlite-large-value: allow-for-now lv-h7c3401e41e3b owner=st3::store::actual_state_sorter_tests TODO=trace-statement-and-value-representation */ &format!(
                "SELECT claims.id FROM claims ORDER BY {CANONICAL_ORDER}"
            ))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(canonical, ["old", "middle", "new"]);
        if order == [2, 1, 0] {
            assert_eq!(arrival, ["new", "middle", "old"]);
            assert_ne!(
                arrival, canonical,
                "reverse arrival must exercise canonical reordering"
            );
        }
        assert_eq!(
            latest_actual_at(&connection, SUBJECT, None).unwrap(),
            Some(json!({"phase":"new","carried":"kept","nullable":null}))
        );
    }
}

#[test]
fn actual_identity_stream_keeps_historical_cut_and_repaired_exclusion() {
    let connection = synthetic_source();
    let old = synthetic_row(
        &connection,
        "old",
        "batch/alder",
        "9",
        0,
        "resource.observed",
        &json!({"fields":{"carried":"kept","phase":"old"}}),
    );
    synthetic_row(
        &connection,
        "new",
        "batch/birch",
        "10",
        0,
        "resource.observed",
        &json!({"fields":{"phase":"new"}}),
    );
    synthetic_row(
        &connection,
        "repaired",
        "batch/birch",
        "11",
        1,
        "resource.observed",
        &json!({"fields":{"phase":"retired","carried":"wrong"}}),
    );
    connection
        .execute(
            /* sqlite-large-value: allow-for-now lv-hcca02d38ae2f owner=st3::store::actual_state_sorter_tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO projection_digest_repaired_claims VALUES ('repaired')",
            [],
        )
        .unwrap();
    // Neither upper-case harness reports nor desired state enters the body fold.
    synthetic_row(
        &connection,
        "harness",
        "batch/birch",
        "12",
        2,
        "HARNESS.observed",
        &json!({"fields":{"phase":"harness"}}),
    );
    synthetic_row(
        &connection,
        "desired",
        "batch/birch",
        "13",
        3,
        "intent.desired",
        &json!({"fields":{"phase":"desired"}}),
    );
    assert_eq!(
        latest_actual_at(&connection, SUBJECT, Some(old)).unwrap(),
        Some(json!({"carried":"kept","phase":"old"}))
    );
    assert_eq!(
        latest_actual_at(&connection, SUBJECT, None).unwrap(),
        Some(json!({"carried":"kept","phase":"new"}))
    );
    connection
        .execute(/* sqlite-large-value: allow-for-now lv-h906a12172412 owner=st3::store::actual_state_sorter_tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET body='invalid' WHERE id='new'", [])
        .unwrap();
    assert!(latest_actual_at(&connection, SUBJECT, None).is_err());
    assert_eq!(
        latest_actual_at(&connection, "resource/absent", None).unwrap(),
        None
    );
}

fn append_observation(store: &Store, time: u128, fields: Value) -> ClaimRecord {
    store.set_write_clock_at(time).unwrap();
    let mut writer = store.connection.write();
    let transaction = writer.transaction().unwrap();
    let claim = append_claim_tx(
        &transaction,
        "alder",
        SUBJECT,
        "resource.observed",
        None,
        &json!({"fields":fields}),
        &[],
        None,
    )
    .unwrap();
    transaction.commit().unwrap();
    claim
}

#[test]
fn actual_identity_stream_reads_the_pinned_native_reader_cut() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("claims.sqlite3"), "alder").unwrap();
    let old = append_observation(&store, 9, json!({"phase":"old","carried":"kept"}));
    let reader = store.readers.get();
    reader.execute_batch(/* sqlite-large-value: allow-for-now lv-h4baa5b2b3311 owner=st3::store::actual_state_sorter_tests TODO=trace-statement-and-value-representation */ "BEGIN").unwrap();
    assert_eq!(
        latest_actual_at(&reader, SUBJECT, None).unwrap(),
        Some(json!({"phase":"old","carried":"kept"}))
    );
    append_observation(&store, 10, json!({"phase":"new"}));
    assert_eq!(
        latest_actual_at(&reader, SUBJECT, None).unwrap(),
        Some(json!({"phase":"old","carried":"kept"}))
    );
    reader.execute_batch(/* sqlite-large-value: allow-for-now lv-he3c6d972b45d owner=st3::store::actual_state_sorter_tests TODO=trace-statement-and-value-representation */ "ROLLBACK").unwrap();
    assert_eq!(
        latest_actual_at(&reader, SUBJECT, Some(old.store_index)).unwrap(),
        Some(json!({"phase":"old","carried":"kept"}))
    );
    assert_eq!(
        store.latest_actual_value(SUBJECT).unwrap(),
        Some(json!({"phase":"new","carried":"kept"}))
    );
}

// The former body-bearing sorter, retained only as an invented-fixture comparison.
#[cfg(target_os = "linux")]
fn body_sort_reference(connection: &Connection) -> Option<Value> {
    let mut statement = connection
        .prepare(/* sqlite-large-value: allow-for-now lv-h7ac11010d620 owner=st3::store::actual_state_sorter_tests TODO=trace-statement-and-value-representation */ &format!(
            "SELECT claims.kind, claims.body FROM claims INDEXED BY claims_subject_kind_index
         JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND {ACTUAL_STATE_CLAIM}
           AND claims.kind NOT LIKE 'harness.%' AND claims.store_index<=?2
           AND {UNREPAIRED_CLAIM} ORDER BY {CANONICAL_ORDER}"
        ))
        .unwrap();
    let mut rows = statement.query(params![SUBJECT, i64::MAX]).unwrap();
    fold_latest_values(std::iter::from_fn(|| {
        rows.next().unwrap().map(|row| {
            let kind: String = row.get(0).unwrap();
            let body: String = row.get(1).unwrap();
            (kind, serde_json::from_str::<Value>(&body).unwrap())
        })
    }))
    .unwrap()
}

#[cfg(target_os = "linux")]
fn written_bytes() -> u64 {
    std::fs::read_to_string("/proc/thread-self/io")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("wchar: "))
        .unwrap()
        .parse()
        .unwrap()
}

#[cfg(target_os = "linux")]
fn byte_comparison(connection: &Connection, count: usize, width: usize, caller: &str) -> u64 {
    connection
        .execute_batch("PRAGMA temp_store=FILE; PRAGMA cache_size=-64")
        .unwrap();
    let before = written_bytes();
    let started = std::time::Instant::now();
    let expected = body_sort_reference(connection);
    let reference_us = started.elapsed().as_micros();
    let reference_bytes = written_bytes() - before;
    let before = written_bytes();
    let started = std::time::Instant::now();
    let actual = latest_actual_at(connection, SUBJECT, None).unwrap();
    let identity_us = started.elapsed().as_micros();
    let identity_bytes = written_bytes() - before;
    assert_eq!(actual, expected);
    assert_eq!(actual.as_ref().unwrap()["rank"], json!(0));
    // Compact keys for at most 256 rows stay comfortably inside this byte budget even
    // when payload width grows eightfold. The old query fails this same assertion.
    assert!(
        identity_bytes < 2 * 1024 * 1024,
        "{caller}: {count}x{width} actual-state read wrote {identity_bytes} bytes"
    );
    if count == 256 && width == 128 * 1024 {
        assert!(
            reference_bytes > 8 * 1024 * 1024,
            "reference did not exercise a body sorter spill: {reference_bytes}"
        );
    }
    println!(
        "actual sorter {caller}: rows={count} width={width} body_sort_bytes={reference_bytes} identity_sort_bytes={identity_bytes} body_sort_read_us={reference_us} identity_sort_read_us={identity_us} point_lookups={count}"
    );
    identity_bytes
}

#[cfg(target_os = "linux")]
#[test]
fn actual_identity_stream_bounds_native_reader_and_writer_temp_bytes() {
    // Two independent growth axes; at most 32 MiB of body data in one file-backed fixture.
    // Setup is excluded from thread-local syscall-byte measurements. No copied live data.
    for count in [128, 256] {
        let mut narrow = None;
        for width in [16 * 1024, 128 * 1024] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(&directory.path().join("claims.sqlite3"), "alder").unwrap();
            {
                let mut writer = store.connection.write();
                let transaction = writer.transaction().unwrap();
                transaction.execute(
                    /* sqlite-large-value: allow-for-now lv-h1239f38bcb3b owner=st3::store::actual_state_sorter_tests TODO=chunk-or-reference-exact-wire-values */ "INSERT INTO batches(id,origin,replica_sequence,previous_hash,hash,accepted_at_unix_ms)
                     VALUES ('batch/alder/sorter','alder',1,NULL,'sorter','1')", [],
                ).unwrap();
                transaction.execute(
                    /* sqlite-large-value: allow-for-now lv-he6cd1d81ffbd owner=st3::store::actual_state_sorter_tests TODO=prepare-body-and-predecessor-values */ "WITH RECURSIVE history(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM history WHERE n+1<?1)
                     INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                     SELECT printf('%064x',n),'batch/alder/sorter',?2,'resource.observed','alder',
                            json_object('fields',json_object('rank',n,'cursor',?3)), '[]',CAST(?1-n AS TEXT)
                     FROM history",
                    params![count, SUBJECT, "x".repeat(width)],
                ).unwrap();
                transaction.commit().unwrap();
            }
            let reader_bytes = {
                let reader = store.readers.get();
                byte_comparison(&reader, count, width, "reader")
            };
            // Observer ingestion calls the same fold while holding the native writer.
            let writer = store.connection.write();
            let writer_bytes = byte_comparison(&writer, count, width, "writer");
            if let Some((narrow_reader, narrow_writer)) = narrow {
                assert_eq!(
                    reader_bytes, narrow_reader,
                    "reader sorter writes must not grow with payload width at {count} identities"
                );
                assert_eq!(
                    writer_bytes, narrow_writer,
                    "writer sorter writes must not grow with payload width at {count} identities"
                );
            } else {
                narrow = Some((reader_bytes, writer_bytes));
            }
        }
    }
}
