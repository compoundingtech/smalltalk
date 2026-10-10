//! Persistent, cut-aware invalidation across independent checkpoint page snapshots.
use super::*;

const TRIGGER_VERSION: i64 = 3;
const GUARD: &str = "checkpoint_capture_epoch WHERE id=1";
const TABLES: &[&str] = &[
    "claims",
    "batches",
    "replica_records",
    "replica_envelopes",
    "documents",
    "desired",
    "mission_definitions",
    "mission_revisions",
    "checkpoint_claims",
    "checkpoint_envelopes",
];

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    // Earlier version-18 PR heads have the singleton but neither of these columns.
    // Add them before any cut-aware trigger is installed; preserve its epoch/frontier.
    for column in ["cut_unix_ms", "trigger_version"] {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('checkpoint_capture_epoch') WHERE name=?1)",
            [column],
            |row| row.get(0),
        )?;
        if !exists {
            transaction.execute_batch(&format!(
                "ALTER TABLE checkpoint_capture_epoch ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
            ))?;
        }
    }
    let version: i64 = transaction.query_row(
        "SELECT trigger_version FROM checkpoint_capture_epoch WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(version <= TRIGGER_VERSION, "unsupported checkpoint capture trigger version {version}");
    if version != TRIGGER_VERSION {
        // IF NOT EXISTS alone would keep the previous blanket predicates. DDL and the
        // epoch bump commit together, so a capture using the old guard must restart.
        for table in TABLES {
            for event in ["INSERT", "UPDATE", "DELETE", "replace"] {
                transaction.execute_batch(&format!(
                    "DROP TRIGGER IF EXISTS {table}_checkpoint_capture_{event};"
                ))?;
            }
        }
    }
    for table in TABLES {
        let (exists, installed): (bool, bool) = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1),
                    EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?2)",
            params![table, format!("{table}_checkpoint_capture_replace")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        // Installation is atomic; the final BEFORE trigger marks that table complete.
        // A runtime can add optional protection tables on a later open.
        if exists && !installed {
            install(&transaction, table)?;
        }
    }
    if version != TRIGGER_VERSION {
        transaction.execute(
            "UPDATE checkpoint_capture_epoch SET value=value+1,trigger_version=?1 WHERE id=1",
            [TRIGGER_VERSION],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Envelope admission, including pending backfills and late-claim exclusion witnesses.
fn envelope_relevant(alias: &str) -> String {
    format!(
        "{alias}.rowid <= (SELECT envelope_frontier FROM {GUARD})
         AND CAST({alias}.accepted_at_unix_ms AS INTEGER) < (SELECT cut_unix_ms FROM {GUARD})"
    )
}

fn record_envelope_relevant(alias: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM replica_envelopes AS envelopes
         WHERE envelopes.writer={alias}.writer AND envelopes.sequence={alias}.sequence
           AND envelopes.envelope_hash={alias}.envelope_hash
           AND {})",
        envelope_relevant("envelopes")
    )
}

fn claim_envelope_relevant(id: &str, batch: &str) -> String {
    // Seek each relationship independently. Combining batch membership and record
    // membership under one envelope-side OR scans the entire frontier for every
    // deleted claim during trim. CROSS JOIN keeps record membership driven by the
    // claim-id index before the exact envelope-identity lookup.
    let envelope = envelope_relevant("envelopes");
    format!(
        "(EXISTS(SELECT 1 FROM replica_envelopes AS envelopes
          WHERE envelopes.batch_id={batch} AND {envelope})
          OR EXISTS(SELECT 1 FROM replica_records AS records
            CROSS JOIN replica_envelopes AS envelopes
              ON envelopes.writer=records.writer AND envelopes.sequence=records.sequence
                AND envelopes.envelope_hash=records.envelope_hash
            WHERE records.claim_id={id} AND {envelope}))"
    )
}

/// Protection and canonical ordering only matter for an actually below-cut target.
fn captured_claim(id: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM claims AS captured
         WHERE captured.id={id}
           AND CAST(captured.accepted_at_unix_ms AS INTEGER) < (SELECT cut_unix_ms FROM {GUARD})
           AND {})",
        claim_envelope_relevant("captured.id", "captured.batch_id")
    )
}

fn repair_relevant(alias: &str) -> String {
    let replacement = captured_claim(&format!("json_extract({alias}.body,'$.fields.replacement')"));
    let predecessor = captured_claim("predecessor.value");
    format!(
        "({alias}.kind='record.repaired' AND (
            {replacement}
            OR EXISTS(SELECT 1 FROM replica_records AS repaired
                WHERE repaired.record_ref=json_extract({alias}.body,'$.fields.record')
                  AND {})))
         OR ({alias}.kind='repair.applied' AND EXISTS(
            SELECT 1 FROM json_each({alias}.predecessors) AS predecessor WHERE {predecessor}))",
        record_envelope_relevant("repaired")
    )
}

fn relevant(table: &str, alias: &str) -> String {
    match table {
        "replica_envelopes" => envelope_relevant(alias),
        "replica_records" => format!(
            "{} OR {} OR {}",
            record_envelope_relevant(alias),
            captured_claim(&format!("{alias}.claim_id")),
            captured_claim(&format!("{alias}.replacement_claim_id"))
        ),
        "claims" => format!(
            "{} OR ({})",
            claim_envelope_relevant(&format!("{alias}.id"), &format!("{alias}.batch_id")),
            repair_relevant(alias)
        ),
        "batches" => format!(
            "EXISTS(SELECT 1 FROM claims AS batch_claim
             WHERE batch_claim.batch_id={alias}.id AND {})",
            claim_envelope_relevant("batch_claim.id", "batch_claim.batch_id")
        ),
        "documents" => captured_claim(&format!("{alias}.binding_claim_id")),
        "desired" | "mission_definitions" | "mission_revisions" => {
            captured_claim(&format!("{alias}.claim_id"))
        }
        "checkpoint_claims" | "checkpoint_envelopes" => format!(
            "{alias}.accepted_at_unix_ms < (SELECT cut_unix_ms FROM {GUARD})"
        ),
        _ => unreachable!("only audited capture tables are registered"),
    }
}

/// Only these columns feed captured bodies, keys, membership or protection.
fn columns(table: &str) -> &'static [&'static str] {
    match table {
        "claims" => &[
            "store_index", "id", "batch_id", "subject", "kind", "origin", "actor",
            "body", "predecessors", "accepted_at_unix_ms",
        ],
        "batches" => &["id", "origin", "replica_sequence"],
        "replica_records" => &[
            "record_ref", "writer", "sequence", "envelope_hash", "position", "state",
            "claim_id", "replacement_claim_id",
        ],
        "replica_envelopes" => &["writer", "sequence", "envelope_hash", "accepted_at_unix_ms"],
        "documents" => &["binding_claim_id"],
        "desired" | "mission_definitions" | "mission_revisions" => &["claim_id"],
        "checkpoint_claims" => &[
            "id", "writer", "sequence", "envelope_hash", "subject", "kind", "actor",
            "predecessors", "operation_id", "request_digest", "accepted_at_unix_ms",
        ],
        "checkpoint_envelopes" => &["writer", "sequence", "envelope_hash", "accepted_at_unix_ms"],
        _ => unreachable!("only audited capture columns are compared"),
    }
}

fn differences(table: &str, old: &str) -> String {
    columns(table)
        .iter()
        .map(|column| {
            if *column == "store_index" {
                // An omitted INTEGER PRIMARY KEY is -1 in BEFORE INSERT.
                format!("(NEW.store_index<>-1 AND {old}.store_index IS NOT NEW.store_index)")
            } else {
                format!("{old}.{column} IS NOT NEW.{column}")
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn collision(table: &str) -> &'static str {
    match table {
        "claims" => "existing.id=NEW.id OR existing.store_index=NEW.store_index",
        "batches" => "existing.id=NEW.id",
        "replica_records" => "existing.record_ref=NEW.record_ref OR (existing.writer=NEW.writer AND existing.sequence=NEW.sequence AND existing.envelope_hash=NEW.envelope_hash AND existing.position=NEW.position)",
        "replica_envelopes" => "existing.rowid=NEW.rowid OR (existing.writer=NEW.writer AND existing.sequence=NEW.sequence AND existing.envelope_hash=NEW.envelope_hash)",
        "documents" => "existing.rowid=NEW.rowid OR (existing.name=NEW.name AND existing.hash=NEW.hash)",
        "desired" => "existing.rowid=NEW.rowid OR existing.subject=NEW.subject",
        "mission_definitions" => "existing.rowid=NEW.rowid OR existing.mission_id=NEW.mission_id",
        "mission_revisions" => "existing.rowid=NEW.rowid OR (existing.mission_id=NEW.mission_id AND existing.revision=NEW.revision)",
        "checkpoint_claims" => "existing.rowid=NEW.rowid OR existing.id=NEW.id",
        "checkpoint_envelopes" => "existing.rowid=NEW.rowid OR (existing.writer=NEW.writer AND existing.sequence=NEW.sequence AND existing.envelope_hash=NEW.envelope_hash)",
        _ => unreachable!("only audited capture identities are fenced"),
    }
}

fn install(connection: &Connection, table: &str) -> Result<()> {
    let old = relevant(table, "OLD");
    let new = relevant(table, "NEW");
    // A successful below-cut envelope insertion may be an identical REPLACE that
    // relocates a captured identity above the frontier. IGNORE has no AFTER event.
    let inserted = if table == "replica_envelopes" {
        format!("CAST(NEW.accepted_at_unix_ms AS INTEGER) < (SELECT cut_unix_ms FROM {GUARD})")
    } else {
        new.clone()
    };
    for (event, condition) in [
        ("INSERT", inserted),
        ("DELETE", old.clone()),
        ("UPDATE", format!(
            "(({old}) OR ({new})) AND (OLD.rowid IS NOT NEW.rowid OR {})",
            differences(table, "OLD")
        )),
    ] {
        create_trigger(connection, table, event, "AFTER", &condition)?;
    }
    // With recursive_triggers=OFF, REPLACE's implicit DELETE is invisible. Inspect
    // each colliding OLD identity before it disappears, but do not fence identical
    // OR IGNORE re-offers. NEW.rowid=-1 means SQLite has not assigned a rowid yet.
    let changed = format!(
        "(NEW.rowid<>-1 AND existing.rowid IS NOT NEW.rowid) OR {}",
        differences(table, "existing")
    );
    let condition = format!(
        "EXISTS(SELECT 1 FROM {table} AS existing WHERE ({}) AND ({}) AND ({changed}))",
        collision(table),
        relevant(table, "existing")
    );
    create_trigger(connection, table, "replace", "BEFORE", &condition)?;
    Ok(())
}

fn create_trigger(
    connection: &Connection,
    table: &str,
    event: &str,
    timing: &str,
    condition: &str,
) -> Result<()> {
    let operation = if event == "replace" { "INSERT" } else { event };
    connection.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS {table}_checkpoint_capture_{event} {timing} {operation} ON {table}
         WHEN (SELECT cut_unix_ms FROM {GUARD})>0 AND ({condition}) BEGIN
             UPDATE checkpoint_capture_epoch SET value=value+1 WHERE id=1;
         END;"
    ))?;
    Ok(())
}
