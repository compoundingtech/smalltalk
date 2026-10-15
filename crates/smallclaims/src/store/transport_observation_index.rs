//! Rebuildable transport-only seek index. Nothing here runs during Store::open.
//! Install capture before taking the backfill high-water mark, then yield the SQLite
//! writer after each bounded page. Reads keep the original query until completion.
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{canonical, canonical_sql};
use crate::sqlite::WriterConnection;

pub const NAME: &str = "transport_observation_seek";
pub const PAGE_SIZE: i64 = 128;
pub const ORIGINAL_QUERY: &str =
    "SELECT body, accepted_at_unix_ms FROM claims
     WHERE subject=?1 AND kind='transport.observed' AND origin=?2
     ORDER BY CANONICAL_DESC(claims) LIMIT 1";

static ORIGINAL_SQL: LazyLock<String> = LazyLock::new(|| canonical_sql(ORIGINAL_QUERY));

const INSTALL: &str = r#"
CREATE TABLE IF NOT EXISTS transport_observation_index (
    claim_id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    origin TEXT NOT NULL,
    accepted_at_unix_ms TEXT NOT NULL
);
-- This CREATE INDEX only ever sees the newly created, empty table. It is never
-- created over the claim log or deferred until after the historical copy.
CREATE INDEX IF NOT EXISTS transport_observation_seek
ON transport_observation_index(subject, origin, length(accepted_at_unix_ms), accepted_at_unix_ms, claim_id);
CREATE TABLE IF NOT EXISTS transport_observation_backfill (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    high_water INTEGER NOT NULL,
    cursor INTEGER NOT NULL,
    ready INTEGER NOT NULL CHECK(ready IN (0,1))
);
CREATE TRIGGER IF NOT EXISTS transport_observation_insert AFTER INSERT ON claims
WHEN NEW.kind='transport.observed' BEGIN
    INSERT OR REPLACE INTO transport_observation_index
    VALUES (NEW.id, NEW.subject, NEW.origin, NEW.accepted_at_unix_ms);
END;
CREATE TRIGGER IF NOT EXISTS transport_observation_delete AFTER DELETE ON claims
WHEN OLD.kind='transport.observed' BEGIN
    DELETE FROM transport_observation_index WHERE claim_id=OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS transport_observation_update
AFTER UPDATE OF id, subject, kind, origin, accepted_at_unix_ms ON claims
WHEN OLD.kind='transport.observed' OR NEW.kind='transport.observed' BEGIN
    DELETE FROM transport_observation_index WHERE claim_id=OLD.id;
    INSERT OR REPLACE INTO transport_observation_index
    SELECT NEW.id, NEW.subject, NEW.origin, NEW.accepted_at_unix_ms
    WHERE NEW.kind='transport.observed';
END;
-- The same write transaction installs the triggers and captures this boundary.
INSERT OR IGNORE INTO transport_observation_backfill
SELECT 1, COALESCE(MAX(store_index),0), 0, 0 FROM claims;
"#;

#[derive(Debug)]
pub struct Page {
    pub ready: bool,
    pub copied: usize,
    /// Begin-through-commit upper bound on write hold, including managed finalizers
    /// and any SQLite acquisition wait, but excluding background queue admission.
    pub write_hold: Duration,
}

/// Inspect schema presence before preparing SQL against optional migration tables.
/// The marker is committed atomically with the last page, never before it.
pub fn ready(connection: &Connection) -> Result<bool> {
    let present: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)
         AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='transport_observation_backfill')",
        [NAME], |row| row.get(0),
    )?;
    if !present {
        return Ok(false);
    }
    Ok(connection.query_row(
        "SELECT ready FROM transport_observation_backfill WHERE singleton=1",
        [], |row| row.get(0),
    ).optional()?.unwrap_or(false))
}

static INDEXED_SQL: LazyLock<String> = LazyLock::new(|| {
    let ties = canonical::components("claims").into_iter().skip(2)
        .map(|component| format!("{component} DESC"))
        .collect::<Vec<_>>().join(", ");
    // CROSS JOIN keeps the timestamp-ordered side-index seek outside the claim-id
    // lookup. Only the newest timestamp's canonical ties need partial sorting.
    format!(
        "SELECT claims.body, claims.accepted_at_unix_ms
         FROM transport_observation_index AS observation
         CROSS JOIN claims ON claims.id=observation.claim_id
         WHERE observation.subject=?1 AND observation.origin=?2
         ORDER BY length(observation.accepted_at_unix_ms) DESC,
         observation.accepted_at_unix_ms DESC, {ties} LIMIT 1"
    )
});

pub fn indexed_query() -> &'static str {
    &INDEXED_SQL
}

pub fn latest(connection: &Connection, subject: &str, origin: &str) -> Result<Option<(String, String)>> {
    let query = if ready(connection)? { indexed_query() } else { &ORIGINAL_SQL };
    Ok(connection.prepare_cached(query)?.query_row(params![subject, origin], |row| {
        Ok((row.get(0)?, row.get(1)?))
    }).optional()?)
}

pub fn advance(connection: &WriterConnection) -> Result<Page> {
    // All claim writers, including deferred read-then-write transactions, share
    // this queue. An independent SQLite writer would invalidate their snapshots.
    let mut writer = connection.write_background();
    // The first page installs empty tables and triggers; no history scan on startup.
    // Subsequent calls never redo DDL or restart the backfill after a crash.
    let installed: bool = writer.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='transport_observation_backfill')",
        [], |row| row.get(0),
    )?;
    let acquired = Instant::now();
    let transaction = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !installed {
        transaction.execute_batch(INSTALL)?;
    }
    let (high_water, cursor, complete): (i64, i64, bool) = transaction.query_row(
        "SELECT high_water, cursor, ready FROM transport_observation_backfill WHERE singleton=1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if complete {
        anyhow::ensure!(ready(&transaction)?, "transport side index is missing; original lookup remains active");
        transaction.commit()?;
        return Ok(Page { ready: true, copied: 0, write_hold: acquired.elapsed() });
    }
    let end: Option<i64> = transaction.query_row(
        "SELECT MAX(store_index) FROM (
         SELECT store_index FROM claims WHERE store_index>?1 AND store_index<=?2
         ORDER BY store_index LIMIT ?3)",
        params![cursor, high_water, PAGE_SIZE], |row| row.get(0),
    )?;
    let copied = if let Some(end) = end {
        transaction.execute(
            "INSERT OR IGNORE INTO transport_observation_index
             SELECT id, subject, origin, accepted_at_unix_ms FROM claims
             WHERE store_index>?1 AND store_index<=?2 AND kind='transport.observed'",
            params![cursor, end],
        )?
    } else { 0 };
    let done = end.is_none_or(|end| end >= high_water);
    transaction.execute(
        "UPDATE transport_observation_backfill SET cursor=?1, ready=?2 WHERE singleton=1",
        params![end.unwrap_or(high_water), done],
    )?;
    transaction.commit()?;
    Ok(Page { ready: done, copied, write_hold: acquired.elapsed() })
}
