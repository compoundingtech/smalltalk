//! Mutation markers distinguish append-only inventory growth from changes to its prefix.
use super::*;

const KEY: &str = "replication_inventory_generation";

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    // Appends are found by rowid. A deletion, replacement of an identity, or insertion below
    // the existing tail invalidates that shortcut. Tombstones also change payload availability.
    for (table, event, condition) in [
        ("replica_envelopes", "DELETE", ""),
        (
            "replica_envelopes",
            "INSERT",
            "WHEN NEW.rowid < (SELECT MAX(rowid) FROM replica_envelopes)",
        ),
        (
            "replica_envelopes",
            "UPDATE",
            "WHEN OLD.rowid IS NOT NEW.rowid OR OLD.writer IS NOT NEW.writer OR OLD.sequence IS NOT NEW.sequence OR OLD.envelope_hash IS NOT NEW.envelope_hash",
        ),
        ("checkpoint_envelopes", "DELETE", ""),
        ("checkpoint_envelopes", "INSERT", ""),
        (
            "checkpoint_envelopes",
            "UPDATE",
            "WHEN OLD.writer IS NOT NEW.writer OR OLD.sequence IS NOT NEW.sequence OR OLD.envelope_hash IS NOT NEW.envelope_hash",
        ),
    ] {
        connection.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_inventory_{event} AFTER {event} ON {table}
             {condition} BEGIN
                 INSERT INTO meta(key,value) VALUES('{KEY}','1')
                 ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1;
             END;"
        ))?;
    }
    Ok(())
}

pub(super) fn current(connection: &Connection) -> Result<i64> {
    connection
        .prepare_cached(
            "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1),0)",
        )?
        .query_row([KEY], |row| row.get(0))
        .map_err(Into::into)
}
