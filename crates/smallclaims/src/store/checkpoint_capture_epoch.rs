//! Persistent invalidation for checkpoint pages read across independent SQLite snapshots.
use super::*;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    // Updates/deletes can change captured bodies, admission, canonical keys or membership.
    // They are deliberately conservative: no caller may bypass the capture fence.
    for table in ["claims", "batches", "replica_records", "replica_envelopes"] {
        for event in ["UPDATE", "DELETE"] {
            create_trigger(&transaction, table, event, "")?;
        }
    }
    // Protection is not bounded by the envelope seal: a later repair or projection can
    // protect an earlier claim. Tombstone identity and metadata are captured independently.
    for table in [
        "documents",
        "desired",
        "mission_definitions",
        "mission_revisions",
        "checkpoint_claims",
        "checkpoint_envelopes",
    ] {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            for event in ["INSERT", "UPDATE", "DELETE"] {
                create_trigger(&transaction, table, event, "")?;
            }
        }
    }
    create_trigger(
        &transaction,
        "replica_envelopes",
        "INSERT",
        "WHEN NEW.rowid <= (SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1)",
    )?;
    create_trigger(
        &transaction,
        "replica_records",
        "INSERT",
        "WHEN NEW.replacement_claim_id IS NOT NULL OR NEW.state='repaired'
         OR EXISTS (
             SELECT 1 FROM replica_envelopes
             WHERE writer=NEW.writer AND sequence=NEW.sequence AND envelope_hash=NEW.envelope_hash
               AND rowid <= (SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1))
         OR EXISTS (
             SELECT 1 FROM replica_records AS captured
             JOIN replica_envelopes AS envelopes
               ON envelopes.writer=captured.writer AND envelopes.sequence=captured.sequence
                 AND envelopes.envelope_hash=captured.envelope_hash
             WHERE captured.claim_id=NEW.claim_id
               AND envelopes.rowid <= (SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1))",
    )?;
    create_trigger(
        &transaction,
        "claims",
        "INSERT",
        "WHEN NEW.kind IN ('record.repaired','repair.applied')
         OR EXISTS (
             SELECT 1 FROM replica_envelopes
             WHERE batch_id=NEW.batch_id
               AND rowid <= (SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1))
         OR EXISTS (
             SELECT 1 FROM replica_records AS records
             JOIN replica_envelopes AS envelopes
               ON envelopes.writer=records.writer AND envelopes.sequence=records.sequence
                 AND envelopes.envelope_hash=records.envelope_hash
             WHERE records.claim_id=NEW.id
               AND envelopes.rowid <= (SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1))",
    )?;
    create_trigger(
        &transaction,
        "batches",
        "INSERT",
        "WHEN EXISTS(SELECT 1 FROM claims WHERE batch_id=NEW.id)",
    )?;
    // REPLACE's implicit deletion does not run DELETE triggers with SQLite's default
    // recursive_triggers=OFF. Fence identity collisions before that deletion can happen.
    for (table, identity) in [
        ("claims", "id=NEW.id OR store_index=NEW.store_index"),
        ("batches", "id=NEW.id"),
        ("replica_records", "record_ref=NEW.record_ref OR (writer=NEW.writer AND sequence=NEW.sequence AND envelope_hash=NEW.envelope_hash AND position=NEW.position)"),
        ("replica_envelopes", "rowid=NEW.rowid OR (writer=NEW.writer AND sequence=NEW.sequence AND envelope_hash=NEW.envelope_hash)"),
    ] {
        transaction.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_checkpoint_capture_replace BEFORE INSERT ON {table}
             WHEN EXISTS(SELECT 1 FROM {table} WHERE {identity}) BEGIN
                 UPDATE checkpoint_capture_epoch SET value=value+1 WHERE id=1;
             END;"
        ))?;
    }
    transaction.commit()?;
    Ok(())
}

fn create_trigger(connection: &Connection, table: &str, event: &str, condition: &str) -> Result<()> {
    connection.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS {table}_checkpoint_capture_{event} AFTER {event} ON {table}
         {condition} BEGIN
             UPDATE checkpoint_capture_epoch SET value=value+1 WHERE id=1;
         END;"
    ))?;
    Ok(())
}
