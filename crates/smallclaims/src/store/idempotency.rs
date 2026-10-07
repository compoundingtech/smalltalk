//! Bounded retention of local, completed response receipts. Durable operation identities are
//! in claims and checkpoint tombstones and are deliberately outside this cleanup.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Completed new receipts last seven days; their claims retain the operation identity.
pub const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
/// Limit both candidate inspection and mutations, including responses still in use.
pub const CLEANUP_CHUNK: usize = 64;
/// An RTC alone cannot advance the retention clock beyond recent durable evidence.
pub const CLOCK_SLACK_MS: i64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    pub examined: usize,
    pub deleted: usize,
    pub extended: usize,
}

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    // Capture once, before any new request. Existing rows have no recoverable caller
    // association and remain until the separately scheduled deployment+30d follow-up.
    connection.execute(
        "INSERT OR IGNORE INTO meta(key,value)
         SELECT 'idempotency_legacy_rowid',CAST(COALESCE(MAX(rowid),0) AS TEXT) FROM idempotency",
        [],
    )?;
    let has_expiry: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('idempotency')
                       WHERE name='expires_at_unix_ms')",
        [],
        |row| row.get(0),
    )?;
    if !has_expiry {
        connection.execute_batch(
            "ALTER TABLE idempotency ADD COLUMN expires_at_unix_ms INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    let has_replay_safe: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('idempotency') WHERE name='replay_safe')",
        [],
        |row| row.get(0),
    )?;
    if !has_replay_safe {
        connection.execute_batch(
            "ALTER TABLE idempotency ADD COLUMN replay_safe INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    connection.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS idempotency_completed_expiry AFTER INSERT ON idempotency
         BEGIN
             UPDATE idempotency SET expires_at_unix_ms=
                 CAST(unixepoch('subsec') * 1000 AS INTEGER) + {RETENTION_MS}
             WHERE operation_id=NEW.operation_id;
         END;"
    ))?;
    Ok(())
}

pub fn next_expiry(connection: &Connection) -> Result<Option<i64>> {
    Ok(connection.query_row(
        "SELECT expires_at_unix_ms FROM idempotency
         WHERE rowid > (SELECT CAST(value AS INTEGER) FROM meta WHERE key='idempotency_legacy_rowid')
         ORDER BY rowid LIMIT 1", [], |row| row.get(0),
    ).optional()?)
}

/// Store the association on an actual effect claim before hashing/admission. One primary
/// claim identifies the whole atomic request; secondary effects retain their own identities.
pub fn attach(body: &mut serde_json::Value, key: &str) -> Result<()> {
    anyhow::ensure!(
        body.get("_operation").is_none(),
        "effect claim already has an operation identity"
    );
    let digest = super::canonical_hash(&("st3.local-request.v1", &*body))?;
    body["_operation"] = serde_json::json!({
        "id": super::opaque_cache_key(key), "request_digest": digest
    });
    Ok(())
}

/// Seek only the existing expression index, then existing checkpoint operation tombstones.
pub fn original_claim(connection: &Connection, key: &str) -> Result<Option<String>> {
    let operation = super::opaque_cache_key(key);
    let claim = connection
        .query_row(
            "SELECT id FROM claims INDEXED BY claims_operation_index
         WHERE json_extract(body,'$._operation.id')=?1
           AND json_extract(body,'$._operation.id') IS NOT NULL ORDER BY id LIMIT 1",
            [&operation],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if claim.is_some() {
        return Ok(claim);
    }
    Ok(
        super::checkpoint::checkpointed_operation(connection, &operation)?
            .first()
            .map(|(_, claim)| claim.clone()),
    )
}

pub fn cached_response(connection: &Connection, key: &str) -> Result<Option<String>, crate::Error> {
    let cached = connection
        .query_row(
            "SELECT response FROM idempotency WHERE operation_id=?1",
            [super::opaque_cache_key(key)],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(crate::error::internal)?;
    if cached.is_some() {
        return Ok(cached);
    }
    if let Some(claim) = original_claim(connection, key).map_err(crate::error::internal)? {
        return Err(crate::Error::new(
            "idempotency-key-expired",
            "this request was already committed; its saved response has expired",
        )
        .with_detail("idempotency_key", key.to_owned())
        .with_detail("claim_id", claim));
    }
    Ok(None)
}

/// One seek of the existing accepted-time index, with no receipt or claim-history scan.
/// With no committed claim there is no durable clock anchor; automatic expiry defers.
pub fn clock_limit(connection: &Connection) -> Result<Option<i64>> {
    let latest: Option<String> = connection
        .query_row(
            "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_accepted_order_index
             ORDER BY length(accepted_at_unix_ms) DESC,accepted_at_unix_ms DESC,store_index DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    latest
        .map(|latest| {
            let latest = latest.parse::<u128>()?.min(i64::MAX as u128) as i64;
            Ok(latest.saturating_add(CLOCK_SLACK_MS))
        })
        .transpose()
}

/// Inspect at most 64 rows in completion order through the table's existing rowid tree.
/// Stop at the first young row. Move pinned responses to the tail without deleting them:
/// companion DELETE triggers must not run for a live response.
pub fn cleanup_tx(
    transaction: &Transaction<'_>,
    now_unix_ms: i64,
    limit: usize,
    mut in_use: impl FnMut(&Transaction<'_>, &str) -> Result<bool>,
) -> Result<Cleanup> {
    let candidates = transaction.prepare_cached(
        "SELECT rowid,operation_id,response,expires_at_unix_ms,replay_safe FROM idempotency
         WHERE rowid > (SELECT CAST(value AS INTEGER) FROM meta WHERE key='idempotency_legacy_rowid')
         ORDER BY rowid LIMIT ?1",
    )?.query_map([limit.min(CLEANUP_CHUNK) as i64], |row|
        Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,bool>(4)?))
    )?.collect::<Result<Vec<_>,_>>()?;
    let mut result = Cleanup::default();
    for (rowid, key, response, expiry, replay_safe) in candidates {
        if expiry > now_unix_ms {
            break;
        }
        result.examined += 1;
        // Unknown/new writers fail closed: never discard their only effect protection.
        let durable: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_operation_index
             WHERE json_extract(body,'$._operation.id')=?1
               AND json_extract(body,'$._operation.id') IS NOT NULL)",
            [&key],
            |row| row.get(0),
        )?;
        let checkpointed =
            !durable && !super::checkpoint::checkpointed_operation(transaction, &key)?.is_empty();
        if (!replay_safe && !durable && !checkpointed) || in_use(transaction, &response)? {
            transaction.execute(
                "UPDATE idempotency SET rowid=(SELECT MAX(rowid)+1 FROM idempotency),
                 expires_at_unix_ms=?2 WHERE rowid=?1",
                params![rowid, now_unix_ms.saturating_add(RETENTION_MS)],
            )?;
            result.extended += 1;
        } else {
            result.deleted +=
                transaction.execute("DELETE FROM idempotency WHERE operation_id=?1", [key])?;
        }
    }
    Ok(result)
}
