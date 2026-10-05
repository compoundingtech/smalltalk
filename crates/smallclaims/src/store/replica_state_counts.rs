//! Exact replica-record state totals, maintained in the same transaction as record changes.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _};

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    let initialized = connection.query_row(
        "SELECT 1 FROM meta WHERE key='replica_state_counts_v1'",
        [],
        |_| Ok(()),
    ).optional()?.is_some();
    if initialized {
        return Ok(());
    }
    // Schema objects, the legacy backfill, and the marker are one upgrade transaction.
    // A failed or interrupted installation leaves none of them visible to readers.
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS replica_state_counts (
             state TEXT PRIMARY KEY,
             count INTEGER NOT NULL CHECK(count >= 0)
         );
         CREATE TRIGGER IF NOT EXISTS replica_state_counts_insert AFTER INSERT ON replica_records
         BEGIN
             INSERT INTO replica_state_counts(state,count) VALUES (NEW.state,1)
             ON CONFLICT(state) DO UPDATE SET count=count+1;
         END;
         CREATE TRIGGER IF NOT EXISTS replica_state_counts_delete AFTER DELETE ON replica_records
         BEGIN
             UPDATE replica_state_counts SET count=count-1 WHERE state=OLD.state;
         END;
         CREATE TRIGGER IF NOT EXISTS replica_state_counts_update
         AFTER UPDATE OF state ON replica_records WHEN NEW.state<>OLD.state
         BEGIN
             UPDATE replica_state_counts SET count=count-1 WHERE state=OLD.state;
             INSERT INTO replica_state_counts(state,count) VALUES (NEW.state,1)
             ON CONFLICT(state) DO UPDATE SET count=count+1;
         END;",
    )?;
    // A legacy database pays this scan once at upgrade, before readers are opened.
    transaction.execute(
        "INSERT INTO replica_state_counts(state,count)
         SELECT state,COUNT(*) FROM replica_records GROUP BY state",
        [],
    )?;
    transaction.execute(
        "INSERT INTO meta(key,value) VALUES('replica_state_counts_v1','1')",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn count(connection: &Connection, state: &str) -> Result<u64> {
    Ok(connection.query_row(
        "SELECT count FROM replica_state_counts WHERE state=?1",
        [state],
        |row| row.get(0),
    ).optional()?.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_match_records_across_upgrade_changes_rollback_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("counts.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE replica_records(record_ref TEXT PRIMARY KEY,state TEXT NOT NULL);
             INSERT INTO replica_records VALUES('record/old','invalid');",
        ).unwrap();
        initialize(&connection).unwrap();
        let reader = Connection::open(&path).unwrap();
        let assert_exact = |connection: &Connection| {
            for state in ["pending", "valid", "unknown", "invalid", "repaired"] {
                let full: u64 = connection.query_row(
                    "SELECT COUNT(*) FROM replica_records WHERE state=?1",
                    [state], |row| row.get(0),
                ).unwrap();
                assert_eq!(count(connection, state).unwrap(), full, "{state}");
            }
        };
        assert_exact(&connection);
        assert_exact(&reader);
        connection.execute_batch(
            "INSERT INTO replica_records VALUES('record/new','unknown');
             UPDATE replica_records SET state='repaired' WHERE record_ref='record/old';
             UPDATE replica_records SET state='repaired' WHERE record_ref='record/old';
             DELETE FROM replica_records WHERE record_ref='record/new';",
        ).unwrap();
        assert_exact(&connection);
        assert_exact(&reader);
        connection.execute_batch(
            "BEGIN;
             INSERT INTO replica_records VALUES('record/rolled-back','invalid');
             UPDATE replica_records SET state='valid' WHERE record_ref='record/old';
             ROLLBACK;",
        ).unwrap();
        assert_exact(&connection);
        assert_exact(&reader);
        drop(reader);
        drop(connection);
        let connection = Connection::open(&path).unwrap();
        initialize(&connection).unwrap();
        assert_exact(&connection);
    }

    #[test]
    fn failed_installation_leaves_no_partial_counter_schema() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY CHECK(key<>'replica_state_counts_v1'),
                 value TEXT NOT NULL);
             CREATE TABLE replica_records(record_ref TEXT PRIMARY KEY,state TEXT NOT NULL);
             INSERT INTO replica_records VALUES('record/example','invalid');",
        ).unwrap();
        assert!(initialize(&connection).is_err());
        let installed: u64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE name LIKE 'replica_state_counts%'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(installed, 0);
        assert_eq!(connection.query_row(
            "SELECT COUNT(*) FROM replica_records", [], |row| row.get::<_, u64>(0),
        ).unwrap(), 1);
    }

    #[test]
    fn wal_reader_sees_matching_counter_and_record_snapshots_across_a_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("counts-wal.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE replica_records(record_ref TEXT PRIMARY KEY,state TEXT NOT NULL);
             INSERT INTO replica_records VALUES('record/one','invalid');",
        ).unwrap();
        initialize(&writer).unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let oracle = |connection: &Connection| -> u64 {
            connection.query_row(
                "SELECT COUNT(*) FROM replica_records WHERE state='invalid'",
                [], |row| row.get(0),
            ).unwrap()
        };
        assert_eq!(count(&reader, "invalid").unwrap(), oracle(&reader));
        assert_eq!(oracle(&reader), 1);
        writer.execute(
            "INSERT INTO replica_records VALUES('record/two','invalid')", [],
        ).unwrap();
        assert_eq!(count(&reader, "invalid").unwrap(), oracle(&reader));
        assert_eq!(oracle(&reader), 1);
        reader.execute_batch("COMMIT").unwrap();
        assert_eq!(count(&reader, "invalid").unwrap(), oracle(&reader));
        assert_eq!(oracle(&reader), 2);
    }
}
