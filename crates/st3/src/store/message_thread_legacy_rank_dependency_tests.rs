//! Synthetic canonical arithmetic only. This fixture does not construct an admitted Store,
//! lawful legacy envelope, checkpoint proof, live trim, cache or ready thread source.
use rusqlite::Connection;
use smallclaims::store::canonical::claim_key;

type MessageSource = (String, String, String, u64, String);

fn candidates(connection: &Connection) -> Vec<(String, String, String)> {
    connection
        .prepare("SELECT source_id, subject, parent FROM raw_parent_candidates ORDER BY source_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn message_sources(connection: &Connection) -> Vec<MessageSource> {
    connection
        .prepare(
            "SELECT id, batch_id, accepted_at_unix_ms, store_index, body
             FROM claims WHERE subject LIKE 'message/%' ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn synthetic_same_batch_nonmessage_deletion_changes_legacy_rank_with_fixed_candidates() {
    let connection = Connection::open_in_memory().unwrap();
    // Direct synthetic input: no modern receive followed by metadata deletion. The tables
    // supply the real canonical::claim_key expression's columns, not a complete Store schema.
    connection
        .execute_batch(
            /* sqlite-large-value: allow-for-now lv-h7fd2d18a4fc3 owner=st3::store::message_thread_legacy_rank_dependency_tests TODO=chunk-or-reference-variable-columns */ r#"
CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT, replica_sequence INTEGER);
CREATE TABLE claims(
    id TEXT PRIMARY KEY, batch_id TEXT, accepted_at_unix_ms TEXT,
    store_index INTEGER UNIQUE, subject TEXT, kind TEXT, body TEXT
);
CREATE TABLE replica_records(claim_id TEXT, position INTEGER);
CREATE TABLE raw_parent_candidates(source_id TEXT PRIMARY KEY, subject TEXT, parent TEXT);
INSERT INTO batches VALUES ('shared', 'writer', 1), ('distinct', 'writer', 2);
INSERT INTO claims VALUES
    ('same-earlier', 'shared', '10', 1, 'resource/same', 'resource.observed', '{}'),
    ('distinct-earlier', 'distinct', '10', 2, 'resource/distinct', 'resource.observed', '{}'),
    ('legacy-message', 'shared', '10', 3, 'message/legacy', 'message.sent',
     '{"fields":{"in_reply_to":"message/root"}}'),
    ('recorded-message', 'shared', '10', 4, 'message/recorded', 'message.sent',
     '{"fields":{"in_reply_to":"message/root"}}');
INSERT INTO replica_records VALUES ('recorded-message', 9), ('recorded-message', 7);
INSERT INTO raw_parent_candidates VALUES
    ('legacy-message', 'message/legacy', 'message/root'),
    ('recorded-message', 'message/recorded', 'message/root');
"#,
        )
        .unwrap();

    let candidate_rows = candidates(&connection);
    let source_rows = message_sources(&connection);
    assert_eq!(candidate_rows.len(), 2);
    assert_eq!(source_rows.len(), 2);
    let legacy_record_count: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM replica_records WHERE claim_id='legacy-message'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy_record_count, 0, "this arm must use the COUNT fallback");

    // Use the actual production key helper. The expected positions are independently
    // enumerated from the fixture: one earlier shared-batch row, and recorded MIN(9, 7).
    let legacy_before = claim_key(&connection, "legacy-message").unwrap();
    let recorded_before = claim_key(&connection, "recorded-message").unwrap();
    assert_eq!(legacy_before.4, 1);
    assert_eq!(recorded_before.4, 7);

    assert_eq!(
        connection
            .execute(/* sqlite-large-value: allow-for-now lv-hed7878fb85c9 owner=st3::store::message_thread_legacy_rank_dependency_tests TODO=prepare-body-and-predecessor-values */ "DELETE FROM claims WHERE id='distinct-earlier'", [])
            .unwrap(),
        1
    );
    assert_eq!(
        claim_key(&connection, "legacy-message").unwrap(),
        legacy_before
    );
    assert_eq!(
        claim_key(&connection, "recorded-message").unwrap(),
        recorded_before
    );
    assert_eq!(candidates(&connection), candidate_rows);
    assert_eq!(message_sources(&connection), source_rows);

    assert_eq!(
        connection
            .execute(/* sqlite-large-value: allow-for-now lv-ha1a4dd0ce93d owner=st3::store::message_thread_legacy_rank_dependency_tests TODO=prepare-body-and-predecessor-values */ "DELETE FROM claims WHERE id='same-earlier'", [])
            .unwrap(),
        1
    );
    let mut expected_legacy = legacy_before;
    expected_legacy.4 = 0;
    assert_eq!(
        claim_key(&connection, "legacy-message").unwrap(),
        expected_legacy
    );
    assert_eq!(
        claim_key(&connection, "recorded-message").unwrap(),
        recorded_before
    );
    assert_eq!(candidates(&connection), candidate_rows);
    assert_eq!(message_sources(&connection), source_rows);
    // These assertions concern only the synthetic order dependency. They do not select
    // a canonical parent, certify candidate coverage, establish lawful trim or make a thread ready.
}
