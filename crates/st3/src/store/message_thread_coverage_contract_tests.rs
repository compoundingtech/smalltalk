//! A source-only protocol fixture for a future thread candidate certificate. These tables and
//! triggers exist only in an in-memory test connection. They do not install a thread source.
//!
//! The fixture tests publication ordering, not parent extraction, canonical selection, or cost.
//! A production certificate needs a separately proved capture of every contributing mutation.

use rusqlite::{Connection, params};
use std::collections::BTreeMap;

const SCHEMA: &str = r#"
CREATE TABLE coverage (
    id INTEGER PRIMARY KEY CHECK (id=1),
    ready INTEGER NOT NULL,
    mutation_seq INTEGER NOT NULL,
    certified_seq INTEGER NOT NULL
);
INSERT INTO coverage VALUES (1,1,0,0);
CREATE TABLE source (id TEXT PRIMARY KEY, parent TEXT);
CREATE TABLE candidate (id TEXT PRIMARY KEY, parent TEXT);
CREATE TABLE dirty (seq INTEGER PRIMARY KEY, old_id TEXT, new_id TEXT);
CREATE TABLE tx_gate (base_ready INTEGER NOT NULL);
CREATE TRIGGER source_insert AFTER INSERT ON source BEGIN
    UPDATE coverage SET mutation_seq=mutation_seq+1,ready=0 WHERE id=1;
    INSERT INTO dirty SELECT mutation_seq,NULL,NEW.id FROM coverage WHERE id=1;
END;
CREATE TRIGGER source_update AFTER UPDATE ON source BEGIN
    UPDATE coverage SET mutation_seq=mutation_seq+1,ready=0 WHERE id=1;
    INSERT INTO dirty SELECT mutation_seq,OLD.id,NEW.id FROM coverage WHERE id=1;
END;
CREATE TRIGGER source_delete AFTER DELETE ON source BEGIN
    UPDATE coverage SET mutation_seq=mutation_seq+1,ready=0 WHERE id=1;
    INSERT INTO dirty SELECT mutation_seq,OLD.id,NULL FROM coverage WHERE id=1;
END;
"#;

fn store() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(SCHEMA).unwrap();
    connection
}

fn state(connection: &Connection) -> (bool, u64, u64, u64) {
    connection
        .query_row(
            "SELECT ready,mutation_seq,certified_seq,(SELECT COUNT(*) FROM dirty)
             FROM coverage WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

fn begin(connection: &Connection) {
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO tx_gate(base_ready) SELECT ready FROM coverage WHERE id=1;",
        )
        .unwrap();
}

/// Discharge exactly one captured mutation, after applying its OLD and NEW source identities.
/// A later unsupported mutation retains its own dirty token and cannot be hidden by this one.
fn apply_token(connection: &Connection, seq: u64) {
    let (old_id, new_id): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT old_id,new_id FROM dirty WHERE seq=?1",
            [seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    if let Some(old_id) = old_id {
        connection
            .execute("DELETE FROM candidate WHERE id=?1", [old_id])
            .unwrap();
    }
    if let Some(new_id) = new_id {
        let parent: Option<String> = connection
            .query_row("SELECT parent FROM source WHERE id=?1", [&new_id], |row| {
                row.get(0)
            })
            .unwrap();
        connection
            .execute(
                "INSERT INTO candidate(id,parent) VALUES (?1,?2)
                 ON CONFLICT(id) DO UPDATE SET parent=excluded.parent",
                params![new_id, parent],
            )
            .unwrap();
    }
    assert_eq!(
        connection
            .execute("DELETE FROM dirty WHERE seq=?1", [seq])
            .unwrap(),
        1
    );
}

fn rows(connection: &Connection, table: &str) -> BTreeMap<String, Option<String>> {
    assert!(matches!(table, "source" | "candidate"));
    connection
        .prepare(&format!("SELECT id,parent FROM {table} ORDER BY id"))
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The independent full-row comparison is an oracle in this fixture, not a production writer
/// algorithm. A real source must prove completeness with bounded per-mutation work.
fn finish(connection: &Connection) -> bool {
    let base_ready: bool = connection
        .query_row("SELECT base_ready FROM tx_gate", [], |row| row.get(0))
        .unwrap();
    let (_, seq, _, pending) = state(connection);
    let publish = base_ready && pending == 0;
    if publish {
        assert_eq!(rows(connection, "source"), rows(connection, "candidate"));
        connection
            .execute(
                "UPDATE coverage SET ready=1,certified_seq=?1 WHERE id=1",
                [seq],
            )
            .unwrap();
    }
    connection
        .execute_batch("DELETE FROM tx_gate; COMMIT;")
        .unwrap();
    publish
}

#[test]
fn a_supported_write_publishes_only_at_the_end_of_a_previously_ready_transaction() {
    let connection = store();
    begin(&connection);
    connection
        .execute("INSERT INTO source VALUES ('child','message/root')", [])
        .unwrap();
    assert_eq!(state(&connection), (false, 1, 0, 1));
    apply_token(&connection, 1);
    assert_eq!(state(&connection), (false, 1, 0, 0));
    assert!(finish(&connection));
    assert_eq!(state(&connection), (true, 1, 1, 0));
}

#[test]
fn a_later_typed_write_cannot_certify_prior_incomplete_or_unsupported_work() {
    let connection = store();
    connection
        .execute("UPDATE coverage SET ready=0 WHERE id=1", [])
        .unwrap();
    begin(&connection);
    connection
        .execute("INSERT INTO source VALUES ('first','message/root')", [])
        .unwrap();
    apply_token(&connection, 1);
    assert!(!finish(&connection));
    assert_eq!(state(&connection), (false, 1, 0, 0));

    let connection = store();
    begin(&connection);
    connection
        .execute("INSERT INTO source VALUES ('first','message/root')", [])
        .unwrap();
    apply_token(&connection, 1);
    connection
        .execute(
            "UPDATE source SET parent='message/new' WHERE id='first'",
            [],
        )
        .unwrap(); // Unsupported mutation: token 2 stays dirty.
    connection
        .execute("INSERT INTO source VALUES ('later','message/root')", [])
        .unwrap();
    apply_token(&connection, 3);
    assert_eq!(state(&connection), (false, 3, 0, 1));
    assert!(!finish(&connection));
    assert_eq!(state(&connection), (false, 3, 0, 1));
}

#[test]
fn nested_savepoint_rollback_restores_candidates_mutation_sequence_and_publication() {
    let connection = store();
    begin(&connection);
    connection
        .execute("INSERT INTO source VALUES ('child','message/root')", [])
        .unwrap();
    apply_token(&connection, 1);
    connection.execute_batch("SAVEPOINT branch;").unwrap();
    connection
        .execute(
            "UPDATE source SET parent='message/other' WHERE id='child'",
            [],
        )
        .unwrap();
    apply_token(&connection, 2);
    assert_eq!(
        rows(&connection, "candidate").get("child"),
        Some(&Some("message/other".to_owned()))
    );
    connection
        .execute("INSERT INTO source VALUES ('unhandled','message/root')", [])
        .unwrap();
    assert_eq!(state(&connection), (false, 3, 0, 1));
    connection
        .execute_batch("ROLLBACK TO branch; RELEASE branch;")
        .unwrap();
    assert_eq!(state(&connection), (false, 1, 0, 0));
    assert_eq!(
        rows(&connection, "candidate").get("child"),
        Some(&Some("message/root".to_owned()))
    );
    assert_eq!(rows(&connection, "source"), rows(&connection, "candidate"));
    assert!(finish(&connection));
    assert_eq!(state(&connection), (true, 1, 1, 0));
}
