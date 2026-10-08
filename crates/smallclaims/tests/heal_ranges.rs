use std::collections::BTreeMap;

use rusqlite::{Connection, StatementStatus, params};
use sha2::{Digest, Sha256};
use smallclaims::replication::ClaimRange;
use smallclaims::store::{SCHEMA, heal};

// The deployed query before the identity indexes, retained as an independent row oracle.
const OLD_ROWS: &str = "SELECT batches.origin,batches.replica_sequence,claims.id
    FROM claims JOIN batches ON batches.id=claims.batch_id
    WHERE NOT EXISTS (SELECT 1 FROM replica_records
        WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired')
    ORDER BY batches.origin,batches.replica_sequence,claims.id";

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(SCHEMA).unwrap();
    connection
}

fn batch(connection: &Connection, id: &str, writer: &str, sequence: u64) {
    connection
        .execute(
            "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
         VALUES (?1,?2,?3,?1,'1')",
            params![id, writer, sequence],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
         payload,batch_id,relay,receipt_state,received_at_unix_ms)
         VALUES (?1,?2,?3,'1',x'',?3,'relay','validated','1')",
            params![writer, sequence, id],
        )
        .unwrap();
}

fn claim(connection: &Connection, id: &str, batch: &str, body: &str) {
    connection.execute(
        "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
         VALUES (?1,?2,'note/test','example.note','relay',?3,'[]','1')",
        params![id,batch,body],
    ).unwrap();
}

fn record(connection: &Connection, id: &str, claim: Option<&str>, state: &str, raw: &[u8]) {
    connection
        .execute(
            "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
         raw,state,claim_id,updated_at_unix_ms)
         VALUES (?1,'record-writer',1,?1,0,?2,?3,?4,'1')",
            params![id, raw, state, claim],
        )
        .unwrap();
}

type Identity = (String, u64, String);

fn rows(connection: &Connection, sql: &str) -> (Vec<Identity>, i32, String) {
    let plan = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    let mut statement = connection.prepare(sql).unwrap();
    let identities = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    (
        identities,
        statement.get_status(StatementStatus::VmStep),
        plan,
    )
}

fn new_rows() -> String {
    format!(
        "SELECT batches.origin,batches.replica_sequence,claims.id {}
        ORDER BY batches.origin,batches.replica_sequence,claims.id",
        heal::PROJECTED_CLAIMS
    )
}

fn assert_digests(connection: &Connection) {
    let identities = rows(connection, OLD_ROWS).0;
    let mut expected = BTreeMap::<(String, u64), (u64, Sha256)>::new();
    for (writer, sequence, id) in identities {
        let start = smallclaims::store::replication_bucket_start(sequence);
        let (count, digest) = expected.entry((writer, start)).or_insert_with(|| {
            let mut digest = Sha256::new();
            digest.update(b"st3-heal-claim-range-v1\0");
            (0, digest)
        });
        *count += 1;
        digest.update(sequence.to_be_bytes());
        digest.update((id.len() as u64).to_be_bytes());
        digest.update(id.as_bytes());
    }
    let expected = expected
        .into_iter()
        .map(|((writer, start), (count, digest))| {
            (writer, start, count, hex::encode(digest.finalize()))
        })
        .collect::<Vec<_>>();
    let actual = heal::claim_ranges(connection)
        .unwrap()
        .into_iter()
        .map(|range| (range.writer, range.start, range.count, range.digest))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert_eq!(
        rows(connection, &new_rows()).0,
        rows(connection, OLD_ROWS).0
    );
}

#[test]
fn heal_identity_indexes_preserve_repaired_duplicates_and_range_order() {
    let mut connection = connection();
    // Batch IDs and insertion order disagree with writer/sequence/claim order. Two batches
    // share a writer and sequence; their claims must still sort together by claim ID.
    for (id, writer, sequence) in [
        ("late", "z", 513),
        ("tie-a", "a", 512),
        ("early", "a", 1),
        ("tie-b", "a", 512),
        ("edge", "a", 513),
    ] {
        batch(&connection, id, writer, sequence);
    }
    for (id, batch) in [
        ("z", "tie-a"),
        ("a", "tie-b"),
        ("no-record", "early"),
        ("original", "edge"),
        ("replacement", "edge"),
        ("last", "late"),
    ] {
        claim(&connection, id, batch, "{}");
    }
    record(
        &connection,
        "valid-copy",
        Some("original"),
        "valid",
        b"original",
    );
    record(
        &connection,
        "repaired-copy",
        Some("original"),
        "repaired",
        b"original",
    );
    record(
        &connection,
        "replacement-copy",
        Some("replacement"),
        "valid",
        b"replacement",
    );
    record(&connection, "unclaimed-quarantine", None, "invalid", b"bad");
    assert_digests(&connection);
    let range = ClaimRange {
        writer: "a".into(),
        start: smallclaims::store::replication_bucket_start(512),
    };
    let narrowed = heal::range_claims(&connection, &range).unwrap();
    let expected = rows(&connection, OLD_ROWS)
        .0
        .into_iter()
        .filter(|(writer, sequence, _)| {
            writer == "a"
                && *sequence >= range.start
                && *sequence < range.start + smallclaims::store::REPLICATION_BUCKET_WIDTH
        })
        .map(|(_, _, id)| id)
        .collect::<Vec<_>>();
    assert_eq!(
        narrowed
            .into_iter()
            .map(|claim| claim.claim_id)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        !rows(&connection, &new_rows())
            .0
            .iter()
            .any(|(_, _, id)| id == "original")
    );
    {
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "UPDATE replica_records SET state='valid' WHERE record_ref='repaired-copy'",
                [],
            )
            .unwrap();
        assert_digests(&transaction);
        assert!(
            rows(&transaction, &new_rows())
                .0
                .iter()
                .any(|(_, _, id)| id == "original")
        );
        // Dropping the transaction restores the partial-index exclusion.
    }
    assert_digests(&connection);
    assert!(
        !rows(&connection, &new_rows())
            .0
            .iter()
            .any(|(_, _, id)| id == "original")
    );
    connection
        .execute(
            "UPDATE replica_records SET claim_id='replacement' WHERE record_ref='repaired-copy'",
            [],
        )
        .unwrap();
    assert_digests(&connection);
    connection
        .execute(
            "DELETE FROM replica_records WHERE record_ref='repaired-copy'",
            [],
        )
        .unwrap();
    connection
        .execute("DELETE FROM claims WHERE id='a'", [])
        .unwrap();
    assert_digests(&connection);
}

#[test]
fn heal_range_work_avoids_payload_tables_and_unrelated_repaired_growth() {
    let mut costs = Vec::new();
    for (unrelated, payload_bytes) in [(512, 32), (8192, 16_384)] {
        let mut connection = connection();
        // Populate the old schema first: opening an existing store must build the indexes
        // correctly, without changing its retained records or range answers.
        connection
            .execute_batch(
                "DROP INDEX claims_batch_claim_id;
            DROP INDEX replica_records_repaired_claim",
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        batch(&transaction, "batch", "writer", 1);
        let body = serde_json::json!({"text":"x".repeat(payload_bytes)}).to_string();
        for index in 0..256 {
            let id = format!("claim-{index:04}");
            claim(&transaction, &id, "batch", &body);
            record(
                &transaction,
                &format!("record-{index}"),
                Some(&id),
                "valid",
                body.as_bytes(),
            );
        }
        for index in 0..unrelated {
            record(
                &transaction,
                &format!("other-record-{index}"),
                Some(&format!("other-{index}")),
                "repaired",
                b"{}",
            );
        }
        transaction.commit().unwrap();
        let (old, old_vm, old_plan) = rows(&connection, OLD_ROWS);
        assert!(old_plan.contains("SCAN claims"), "{old_plan}");
        connection.execute_batch(SCHEMA).unwrap();
        let (actual, vm, plan) = rows(&connection, &new_rows());
        assert_eq!(actual, old);
        assert!(
            plan.contains("USING COVERING INDEX claims_batch_claim_id"),
            "{plan}"
        );
        assert!(
            plan.contains("USING COVERING INDEX replica_records_repaired_claim"),
            "{plan}"
        );
        assert!(vm < old_vm, "old={old_vm}, new={vm}");
        assert_digests(&connection);
        costs.push(vm);
    }
    assert_eq!(
        costs[0], costs[1],
        "payload/unrelated-record growth changed range VM work: {costs:?}"
    );
}

#[test]
fn opening_a_populated_store_builds_identity_indexes_without_rewriting_claims() {
    use smallclaims::{
        claim::ClaimInput,
        store::{Store, runtime::Plain},
    };
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let store = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    let input = ClaimInput {
        subject: "note/test".into(),
        kind: "example.note".into(),
        actor: None,
        fields: BTreeMap::from([("text".into(), serde_json::json!("first"))]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    };
    let first = store.append_claim(&input).unwrap();
    let before = heal::claim_ranges(&store.readers.get()).unwrap();
    store
        .connection
        .write()
        .execute_batch(
            "DROP INDEX claims_batch_claim_id;
        DROP INDEX replica_records_repaired_claim",
        )
        .unwrap();
    drop(store);
    let reopened = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    assert_eq!(heal::claim_ranges(&reopened.readers.get()).unwrap(), before);
    assert_eq!(
        reopened.claims_for("note/test", None).unwrap()[0].id,
        first.id
    );
    let connection = reopened.readers.get();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 17);
    let (_, _, plan) = rows(&connection, &new_rows());
    assert!(
        plan.contains("USING COVERING INDEX claims_batch_claim_id"),
        "{plan}"
    );
    assert!(
        plan.contains("USING COVERING INDEX replica_records_repaired_claim"),
        "{plan}"
    );
    drop(connection);
    let second = reopened
        .append_claim(&ClaimInput {
            fields: BTreeMap::from([("text".into(), serde_json::json!("second"))]),
            ..input
        })
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_digests(&reopened.readers.get());
}
