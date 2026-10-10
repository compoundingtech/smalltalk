//! Current observations are registers, never graph facts or queued writer jobs.
use super::*;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS latest_values (
    subject TEXT NOT NULL, kind TEXT NOT NULL, slot TEXT NOT NULL,
    origin TEXT NOT NULL, source_at INTEGER NOT NULL, source_id TEXT NOT NULL,
    local_id INTEGER NOT NULL, store_index INTEGER NOT NULL,
    actor TEXT, body TEXT NOT NULL,
    PRIMARY KEY(subject, kind, slot)
);
CREATE INDEX IF NOT EXISTS latest_values_kind_index ON latest_values(kind);
CREATE INDEX IF NOT EXISTS latest_values_kind_key_index ON latest_values(kind,subject,slot);
CREATE INDEX IF NOT EXISTS latest_values_local_index ON latest_values(local_id);
CREATE INDEX IF NOT EXISTS latest_values_agent_local_index ON latest_values(local_id)
WHERE subject LIKE 'agent/%';
CREATE UNIQUE INDEX IF NOT EXISTS latest_values_source_index ON latest_values(source_id);
-- A register replacement retracts a cleared login candidate from this index immediately.
-- Match the legacy positive-evidence predicate used by attention discovery.
CREATE INDEX IF NOT EXISTS latest_values_harness_login_candidate_index ON latest_values(subject)
WHERE (kind='harness.observed' AND (
    json_type(body, '$.fields.provider_auth')='false'
    OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
    OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
        THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
    OR (kind='harness.diagnostic'
        AND json_extract(body, '$.fields.code')='provider-auth-expired');
CREATE TABLE IF NOT EXISTS latest_readiness (
    subject TEXT PRIMARY KEY, incarnation TEXT NOT NULL, ready INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS current_value_frontiers (
    subject TEXT PRIMARY KEY, revision INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS current_value_frontiers_revision ON current_value_frontiers(revision);
CREATE INDEX IF NOT EXISTS current_value_frontiers_agents ON current_value_frontiers(revision)
WHERE subject LIKE 'agent/%';
CREATE VIEW IF NOT EXISTS current_value_revisions AS
SELECT subject, revision AS local_id FROM current_value_frontiers;
CREATE TABLE IF NOT EXISTS current_value_retirements (
    subject TEXT NOT NULL, kind TEXT NOT NULL, cursor INTEGER NOT NULL,
    cutoff INTEGER NOT NULL, PRIMARY KEY(subject,kind)
);
CREATE VIEW IF NOT EXISTS current_claims AS
SELECT store_index, id, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
FROM main.claims
WHERE kind NOT IN ('harness.observed','harness.usage','harness.todo.observed','workspace.observed','transport.observed')
   OR (kind='harness.usage' AND coalesce(json_extract(body,'$.fields.semantics'),'')!='context_occupancy')
   OR NOT EXISTS (SELECT 1 FROM latest_values v WHERE v.subject=claims.subject AND v.kind=claims.kind
       AND (claims.kind!='transport.observed' OR v.origin=claims.origin)
       AND v.source_at>=CAST(claims.accepted_at_unix_ms AS INTEGER))
UNION ALL
SELECT store_index, source_id, source_id, subject, kind, origin, actor, body, '[]', CAST(source_at AS TEXT)
FROM latest_values;
CREATE VIEW IF NOT EXISTS current_batches AS
SELECT * FROM main.batches
UNION ALL
SELECT source_id, origin, source_at, NULL, '', CAST(source_at AS TEXT) FROM latest_values;
CREATE VIEW IF NOT EXISTS registered_claims AS
SELECT store_index, source_id AS id, source_id AS batch_id, subject, kind, origin, actor,
    body, '[]' AS predecessors, CAST(source_at AS TEXT) AS accepted_at_unix_ms FROM latest_values;
CREATE VIEW IF NOT EXISTS registered_batches AS
SELECT source_id AS id, origin, source_at AS replica_sequence, NULL AS actor,
    '' AS idempotency_key, CAST(source_at AS TEXT) AS accepted_at_unix_ms FROM latest_values;
"#;

/// Positive registers use their partial index. Legacy evidence is probed by each live
/// agent's subject, so retired subjects and repeated history cannot add request work.
/// An indexed existence probe skips the live-fleet pass when no legacy positive evidence
/// exists. Keep the current-claim shadow rule; the final fold checks runtime and ownership.
pub(super) const LOGIN_CANDIDATES_SQL: &str = "
WITH candidates(subject) AS (
    SELECT subject FROM latest_values INDEXED BY latest_values_harness_login_candidate_index
    WHERE (kind='harness.observed' AND (
        json_type(body, '$.fields.provider_auth')='false'
        OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
        OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
        OR (kind='harness.diagnostic'
            AND json_extract(body, '$.fields.code')='provider-auth-expired')
    UNION
    SELECT desired.subject
    FROM (SELECT 1 FROM main.claims INDEXED BY claims_harness_login_candidate_index
          WHERE ((kind='harness.observed' AND (
              json_type(body, '$.fields.provider_auth')='false'
              OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                  THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
              OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                  THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
          OR (kind='harness.diagnostic'
              AND json_extract(body, '$.fields.code')='provider-auth-expired')) LIMIT 1) AS legacy_present
    CROSS JOIN desired
    WHERE desired.kind='agent'
      AND EXISTS (SELECT 1 FROM main.claims INDEXED BY claims_harness_login_candidate_index
          WHERE claims.subject=desired.subject AND ((kind='harness.observed' AND (
        json_type(body, '$.fields.provider_auth')='false'
        OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
        OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
            THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
        OR (kind='harness.diagnostic'
            AND json_extract(body, '$.fields.code')='provider-auth-expired'))
        AND (kind='harness.diagnostic' OR NOT EXISTS (
            SELECT 1 FROM latest_values v
            WHERE v.subject=claims.subject AND v.kind=claims.kind
                AND v.source_at>=CAST(claims.accepted_at_unix_ms AS INTEGER))))
)
SELECT desired.subject, desired.kind, desired.body, desired.member,
       desired.owner_run, desired.owner_generation, desired.owner_step
FROM candidates CROSS JOIN desired
WHERE desired.subject=candidates.subject AND desired.kind='agent'
ORDER BY desired.subject";

/// A database generation fences sequence numbers after a reset. Ordinary reopen retains it.
/// Generation birth uses the owner clock; restore/reset requires that clock to move forward.
pub(super) fn initialize_epoch(connection: &Connection) -> Result<()> {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    connection.execute(
        "INSERT OR IGNORE INTO meta(key,value) VALUES ('current-value-epoch',?1)",
        [epoch.to_string()],
    )?;
    Ok(())
}

/// Current values retain a snapshot and bounded feed, not a replayable telemetry series.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentObservationBoundary {
    pub epoch: String,
    pub local_cursor: u64,
    pub retired_through_local_cursor: u64,
}

impl CurrentObservationBoundary {
    pub fn requires_resync(&self, epoch: &str, local_cursor: u64) -> bool {
        epoch != self.epoch
            || local_cursor < self.retired_through_local_cursor
            || local_cursor > self.local_cursor
    }
}

fn retire_feed_through(tx: &Transaction<'_>, cursor: i64) -> Result<(), St3Error> {
    tx.execute(
        "INSERT INTO meta(key,value) VALUES ('current-value-retired-through',?1)
         ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),?1) AS TEXT)",
        [cursor],
    )
    .map_err(internal)?;
    Ok(())
}

pub(crate) const CURRENT_VALUE_KINDS: [&str; 5] = [
    "harness.observed",
    "harness.usage",
    "harness.todo.observed",
    "workspace.observed",
    "transport.observed",
];

pub fn is_current_value(kind: &str) -> bool {
    CURRENT_VALUE_KINDS.contains(&kind)
}

pub fn is_current_input(input: &ClaimInput) -> bool {
    is_current_value(&input.kind)
        && (input.kind != "harness.usage"
            || input.fields.get("semantics").and_then(Value::as_str) == Some("context_occupancy"))
}

fn permission_blocked(fields: &Value) -> bool {
    seat_status::permission_blocked(
        fields["state"].as_str(),
        fields["blocked_on"].as_str(),
        fields["ask"].as_str(),
    )
}

// Source timestamps refresh liveness but do not change the value a collection folds.
// Keep ownership, state, reasons, credential axes and episode starts in the comparison.
fn semantic_fields(kind: &str, fields: &Value) -> Value {
    let mut fields = fields.clone();
    if let Some(fields) = fields.as_object_mut() {
        for name in [
            "observed_at_ms",
            "observed_at_unix_ms",
            "observed_at",
            "status_transition",
        ] {
            fields.remove(name);
        }
        if kind == "transport.observed" {
            fields.remove("last_success_at");
        }
    }
    fields
}

fn stale_observation(kind: &str, fields: &Value, source_at: u128, now: u128) -> bool {
    kind == "harness.observed"
        && now.saturating_sub(
            fields["observed_at_ms"]
                .as_u64()
                .map(u128::from)
                .unwrap_or(source_at)
                .min(source_at),
        ) > 90_000
}

/// Current folds use the historical column contract and canonical tie breakers, without
/// putting mutable values into signed graph envelopes.
pub(super) fn current_sql(sql: &str) -> String {
    let mut query = sql
        .replace("current_claims", "claims")
        .replace("current_batches", "batches");
    while let Some(start) = query.find("INDEXED BY ") {
        let end = query[start + 11..]
            .find(char::is_whitespace)
            .map_or(query.len(), |end| start + 11 + end);
        query.replace_range(start..end, "");
    }
    // Do not materialize a whole-fleet CTE for a single-seat lookup. The UNION views let
    // SQLite push each subject/kind/id predicate down to the indexed sources.
    // Canonical positions count only immutable predecessors; registers have no graph batch.
    query = query.replace(
        "FROM claims legacy_position",
        "FROM main.claims legacy_position",
    );
    let mut query = query
        .replace("claims.", "current_claims.")
        .replace("batches.", "current_batches.")
        .replace("FROM claims", "FROM current_claims")
        .replace("JOIN claims", "JOIN current_claims")
        .replace("FROM batches", "FROM current_batches")
        .replace("JOIN batches", "JOIN current_batches");
    // Joining a UNION batches view materializes the whole fleet. Scalar indexed lookups
    // provide the same canonical metadata for a graph claim or a register source.
    query = query.replace(
        "JOIN current_batches ON current_batches.id=current_claims.batch_id",
        "",
    );
    for (column, local) in [("origin", "origin"), ("replica_sequence", "source_at")] {
        let lookup = format!(
            "coalesce((SELECT {column} FROM main.batches WHERE id=current_claims.batch_id),\
            (SELECT {local} FROM latest_values WHERE source_id=current_claims.batch_id))"
        );
        query = query
            .replace(
                &format!("(SELECT {column} FROM current_batches WHERE id=current_claims.batch_id)"),
                &lookup,
            )
            .replace(&format!("current_batches.{column}"), &lookup);
    }
    for n in 1..=4 {
        for column in [
            "+current_claims.store_index",
            "current_claims.store_index",
            "+store_index",
            "store_index",
        ] {
            let bound = format!("{column}<=?{n}");
            let allowed = format!(
                "({bound} OR EXISTS(SELECT 1 FROM latest_values WHERE source_id=current_claims.id))"
            );
            // Use a sentinel to avoid repeatedly expanding the bound inside our own predicate.
            query = query.replace(
                &bound,
                &allowed.replace(&bound, "CURRENT_VALUE_INDEX_BOUND"),
            );
            query = query.replace("CURRENT_VALUE_INDEX_BOUND", &bound);
            if query.contains(&allowed) {
                break;
            }
        }
    }
    query
}

/// Legacy-only readers retain their partial indexes. A modern seat reads only its register.
pub(super) fn harness_sql(connection: &Connection, subject: &str, sql: &str) -> Result<String> {
    let current: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM latest_values v WHERE subject=?1 AND kind='harness.observed'
         AND COALESCE((SELECT CAST(c.accepted_at_unix_ms AS INTEGER)
             FROM main.claims c INDEXED BY claims_subject_kind_accepted_index
             WHERE c.subject=v.subject AND c.kind='harness.observed'
             ORDER BY length(c.accepted_at_unix_ms) DESC,c.accepted_at_unix_ms DESC LIMIT 1),0)<=v.source_at)",
        [subject],
        |row| row.get(0),
    )?;
    Ok(if current {
        current_sql(sql)
            .replace("current_claims", "registered_claims")
            .replace("current_batches", "registered_batches")
    } else {
        sql.to_owned()
    })
}

#[cfg(test)]
thread_local! {
    static CURRENT_TRANSACTION_ELAPSED: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
    static CURRENT_TRANSACTION_STEPS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static NATIVE_ADMISSION_CONTROL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// Compare the rejected native busy policy in one synchronous benchmark sample. Other
/// threads and all production entrypoints retain the production admission policy.
#[cfg(any(test, feature = "test-support"))]
pub fn with_native_current_admission_for_test<T>(native: bool, work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) { NATIVE_ADMISSION_CONTROL.with(|flag| flag.set(self.0)); }
    }
    let _reset = Reset(NATIVE_ADMISSION_CONTROL.with(|flag| flag.replace(native)));
    work()
}

// SQLite admission uses only time remaining in this attempt, reserving a short write.
// Schema work, validation, mutation and commit share the unchanged 100 ms deadline.
const CURRENT_WRITE_RESERVE: std::time::Duration = std::time::Duration::from_millis(10);
const CURRENT_WRITER_WAIT: std::time::Duration =
    crate::client::LATEST_VALUE_TIMEOUT.saturating_sub(CURRENT_WRITE_RESERVE);

/// Only this fresh connection's progress deadline can interrupt a current transaction.
/// Preserve ownership/protocol errors; do not infer interruption from an error's text.
fn current_transaction<T>(
    connection: &mut Connection,
    writer: Option<(&smallclaims::sqlite::WriterConnection, bool)>,
    work: impl FnOnce(&Transaction<'_>) -> Result<T, St3Error>,
) -> Result<T, St3Error> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let expired = Arc::new(AtomicBool::new(false));
    let interrupted = expired.clone();
    let deadline = std::time::Instant::now() + crate::client::LATEST_VALUE_TIMEOUT;
    #[cfg(test)]
    let steps = Arc::new(std::sync::atomic::AtomicU64::new(0));
    #[cfg(test)]
    let observed_steps = steps.clone();
    #[cfg(test)]
    let interval =
        CURRENT_TRANSACTION_STEPS.with(|value| if value.get().is_some() { 1 } else { 100 });
    #[cfg(not(test))]
    let interval = 100;
    connection.progress_handler(
        interval,
        Some(move || {
            #[cfg(test)]
            observed_steps.fetch_add(interval as u64, Ordering::Relaxed);
            let due = std::time::Instant::now() >= deadline;
            if due {
                interrupted.store(true, Ordering::Release);
            }
            due
        }),
    );
    let result = (|| {
        // A fresh SQLite connection loads and parses the database schema on its first
        // table access. Do that before acquiring the write lock, under the same deadline.
        // Preparing without stepping writes nothing and leaves no read transaction open.
        drop(
            connection
                .prepare("SELECT local_id FROM latest_values LIMIT 0")
                .map_err(internal)?,
        );
        if std::time::Instant::now() >= deadline {
            return Err(St3Error::new(
                "current-value-deadline",
                "the current value exceeded its write deadline",
            ));
        }
        // SQLite's escalating busy sleeps can skip brief release windows between managed
        // transactions. Retry only BEGIN at short intervals within this same attempt; no
        // mutation or ownership validation runs until admission, and no sample is queued.
        let admission_deadline = deadline - CURRENT_WRITE_RESERVE;
        #[cfg(any(test, feature = "test-support"))]
        let native_control = NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get);
        #[cfg(not(any(test, feature = "test-support")))]
        let native_control = false;
        // Reserve only this attempt's turn, with no sample or mutation queued. Foreground
        // traffic stays FIFO and maintenance keeps the existing background lane. Schema
        // preparation precedes admission, and every validation still runs inside the fresh tx.
        let _reservation = if !native_control {
            writer.map(|(writer, background)| {
                let result = if background { writer.reserve_background_writer_until(admission_deadline) }
                    else { writer.reserve_writer_until(admission_deadline) };
                result.map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout =>
                        St3Error::new("database-busy", "current writer admission exceeded its remaining bound"),
                    std::sync::mpsc::RecvTimeoutError::Disconnected =>
                        St3Error::new("internal", "the managed writer stopped during current admission"),
                })
            }).transpose()?
        } else { None };
        let tx = if native_control {
            connection.busy_timeout(admission_deadline.saturating_duration_since(std::time::Instant::now())).map_err(internal)?;
            Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate).map_err(internal)?
        } else {
            connection.busy_timeout(std::time::Duration::ZERO).map_err(internal)?;
            loop {
                if std::time::Instant::now() >= deadline {
                    return Err(St3Error::new("current-value-deadline", "the current value exceeded its write deadline"));
                }
                match Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate) {
                    Ok(tx) => break tx,
                    Err(error) => {
                        let retryable = matches!(error.sqlite_error_code(),
                            Some(rusqlite::ErrorCode::DatabaseBusy));
                        let remaining = admission_deadline.saturating_duration_since(std::time::Instant::now());
                        if !retryable || remaining.is_zero() {
                            return Err(internal(error));
                        }
                        std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
                        if std::time::Instant::now() >= admission_deadline {
                            return Err(internal(error));
                        }
                    }
                }
            }
        };
        #[cfg(test)]
        let started = std::time::Instant::now();
        if std::time::Instant::now() >= deadline {
            return Err(St3Error::new(
                "current-value-deadline",
                "the current value exceeded its write deadline",
            ));
        }
        let value = work(&tx)?;
        if std::time::Instant::now() >= deadline {
            return Err(St3Error::new(
                "current-value-deadline",
                "the current value exceeded its write deadline",
            ));
        }
        tx.commit().map_err(internal)?;
        #[cfg(test)]
        CURRENT_TRANSACTION_STEPS.with(|value| {
            if value.get().is_some() {
                value.set(Some(steps.load(Ordering::Relaxed)));
            }
        });
        #[cfg(test)]
        CURRENT_TRANSACTION_ELAPSED.with(|elapsed| {
            if elapsed.get().is_some() {
                elapsed.set(Some(started.elapsed()));
            }
        });
        Ok(value)
    })();
    connection.progress_handler(0, None::<fn() -> bool>);
    result.map_err(|error: St3Error| {
        if expired.load(Ordering::Acquire) && error.code == "internal" {
            St3Error::new(
                "current-value-deadline",
                "the current value exceeded its write deadline",
            )
            .with_detail("sqlite_extended_code", rusqlite::ffi::SQLITE_INTERRUPT)
        } else {
            error
        }
    })
}

type LocalValue = (i64, String, String, Option<String>, u64);
type ReceivedValue = (u64, String, i64, String, String, Option<String>);

// Keep one small clock per observed subject, including a tombstone after collection. This
// prevents a deleted value or a source restamp from rewinding an in-flight reader's key.
fn seed_semantic_frontier(connection: &Connection, subject: &str) -> Result<(), St3Error> {
    connection
        .execute(
            "INSERT OR IGNORE INTO current_value_frontiers
        SELECT subject,MAX(local_id) FROM latest_values WHERE subject=?1 GROUP BY subject",
            [subject],
        )
        .map_err(internal)?;
    Ok(())
}

fn record_semantic_frontier(
    connection: &Connection,
    subject: &str,
    source_id: i64,
    changed: bool,
) -> Result<(), St3Error> {
    if changed {
        connection
            .execute(
                "INSERT INTO current_value_frontiers VALUES(?1,?2)
            ON CONFLICT(subject) DO UPDATE SET revision=excluded.revision",
                params![subject, source_id],
            )
            .map_err(internal)?;
    }
    Ok(())
}

// Collection consumes the same AUTOINCREMENT clock as timeline and register inserts.
// One id covers a deletion batch; no payload row is retained for this tombstone.
fn consume_observation_clock(connection: &Connection) -> Result<i64, St3Error> {
    connection
        .execute(
            "UPDATE sqlite_sequence SET seq=max(seq,
        coalesce((SELECT MAX(revision) FROM current_value_frontiers),0))+1
        WHERE name='local_observations'",
            [],
        )
        .map_err(internal)?;
    if connection.changes() == 0 {
        connection.execute("INSERT INTO sqlite_sequence(name,seq)
            VALUES('local_observations',coalesce((SELECT MAX(revision) FROM current_value_frontiers),0)+1)", [])
            .map_err(internal)?;
    }
    connection
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='local_observations'",
            [],
            |r| r.get(0),
        )
        .map_err(internal)
}

pub(crate) struct CurrentMaintenance {
    pub removed: usize,
    pub history_visited: usize,
    pub after: String,
}

/// Prime clocks for an older register database, without scanning claims or parsing payloads.
/// Each transaction has at most 64 keys and the same 100 ms progress deadline.
pub(super) fn initialize_semantic_frontiers(connection: &Connection) -> Result<()> {
    let done: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='current-value-frontiers')",
        [],
        |row| row.get(0),
    )?;
    // Repair clock drift from older collectors before any new observation can be inserted.
    // Both reads use indexed maxima; this does not scan observation payloads.
    let frontier: i64 = connection.query_row(
        "SELECT coalesce(MAX(revision),0) FROM current_value_frontiers",
        [],
        |r| r.get(0),
    )?;
    let clock: i64 = connection.query_row(
        "SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name='local_observations'),0)",
        [],
        |r| r.get(0),
    )?;
    if frontier > clock {
        let deadline = std::time::Instant::now() + crate::client::LATEST_VALUE_TIMEOUT;
        connection.progress_handler(100, Some(move || std::time::Instant::now() >= deadline));
        let result = (|| -> Result<()> {
            let tx =
                Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE sqlite_sequence SET seq=?1 WHERE name='local_observations'",
                [frontier],
            )?;
            if tx.changes() == 0 {
                tx.execute(
                    "INSERT INTO sqlite_sequence(name,seq) VALUES('local_observations',?1)",
                    [frontier],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        connection.progress_handler(0, None::<fn() -> bool>);
        result?;
    }
    if done {
        return Ok(());
    }
    let mut after = (String::new(), String::new(), String::new());
    loop {
        let rows = connection
            .prepare_cached(
                "SELECT subject,kind,slot,local_id FROM latest_values
            WHERE (subject,kind,slot)>(?1,?2,?3) ORDER BY subject,kind,slot LIMIT 64",
            )?
            .query_map(params![after.0, after.1, after.2], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            break;
        }
        let deadline = std::time::Instant::now() + crate::client::LATEST_VALUE_TIMEOUT;
        connection.progress_handler(100, Some(move || std::time::Instant::now() >= deadline));
        let result = (|| -> Result<()> {
            let tx =
                Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
            for (subject, _, _, id) in &rows {
                tx.execute(
                    "INSERT INTO current_value_frontiers VALUES(?1,?2)
                    ON CONFLICT(subject) DO UPDATE SET revision=max(revision,excluded.revision)",
                    params![subject, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        connection.progress_handler(0, None::<fn() -> bool>);
        result?;
        let last = rows.last().unwrap();
        after = (last.0.clone(), last.1.clone(), last.2.clone());
    }
    connection.execute(
        "INSERT INTO meta(key,value) VALUES('current-value-frontiers','1')",
        [],
    )?;
    Ok(())
}

struct HistoryBatch {
    subject: String,
    kind: String,
    cursor: i64,
    cutoff: i64,
    visited: usize,
    last: i64,
    delete: String,
    parse_error: Option<String>,
}

// Payload inspection is bounded and read-only; the writer only rechecks the job and
// live-row fence, deletes selected IDs, and advances its durable cursor.
fn prepare_history_batch(reader: &Connection) -> Result<Option<HistoryBatch>, St3Error> {
    let job: Option<(String,String,i64,i64)> = reader.query_row(
        "SELECT subject,kind,cursor,cutoff FROM current_value_retirements ORDER BY subject,kind LIMIT 1",
        [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(internal)?;
    let Some((subject, kind, cursor, cutoff)) = job else {
        return Ok(None);
    };
    let rows = reader
        .prepare_cached(
            "SELECT id,body,EXISTS(SELECT 1 FROM latest_values v WHERE v.local_id=o.id)
        FROM local_observations o WHERE subject=?1 AND kind=?2 AND id>?3 AND id<=?4
        ORDER BY id LIMIT 256",
        )
        .map_err(internal)?
        .query_map(params![subject, kind, cursor, cutoff], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    let mut delete = Vec::new();
    let mut parse_error = None;
    for (id, body, live) in &rows {
        if *live {
            continue;
        }
        let categorical = if kind != "harness.usage" {
            true
        } else {
            match serde_json::from_str::<Value>(body) {
                Ok(body) => body["fields"]["semantics"] == "context_occupancy",
                Err(error) => {
                    parse_error = Some(format!("row {id}: {error}"));
                    break;
                }
            }
        };
        if categorical {
            delete.push(*id);
        }
    }
    Ok(Some(HistoryBatch {
        subject,
        kind,
        cursor,
        cutoff,
        visited: rows.len(),
        last: rows.last().map_or(cutoff, |r| r.0),
        delete: serde_json::to_string(&delete).map_err(internal)?,
        parse_error,
    }))
}

struct CurrentKey {
    subject: String,
    kind: String,
    slot: String,
    origin: String,
    local_id: i64,
    source_id: String,
}

fn obsolete_key(
    connection: &Connection,
    key: &CurrentKey,
    hosts: &BTreeSet<String>,
) -> Result<bool, St3Error> {
    if key.kind == "transport.observed" {
        return Ok(!hosts.contains(&key.origin)
            || !key
                .subject
                .strip_prefix("host/")
                .is_some_and(|host| hosts.contains(host)));
    }
    let owner: Option<String> = connection
        .query_row(
            "SELECT json_extract(member,'$.host') FROM desired
        WHERE subject=?1 AND kind='agent' AND member IS NOT NULL",
            [&key.subject],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    // A still-declared seat keeps its one slot through a placement change; the existing
    // host guard hides an old owner until the new owner replaces it. Only withdrawn
    // declarations are garbage.
    Ok(owner.is_none())
}

pub(super) fn append(
    graph: &GraphStore,
    input: &ClaimInput,
    now: u128,
    event_runtime: Option<&str>,
) -> Result<(ClaimRecord, bool), St3Error> {
    validate_local_observation(input)?;
    // This connection waits only for bounded admission inside this current attempt. A
    // longer collision drops it; only a subsequent observation can replace this value.
    let mut connection = Connection::open_with_flags(
        &graph.path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(internal)?;
    smallclaims::sqlite::observe(&mut connection);
    connection
        .busy_timeout(CURRENT_WRITER_WAIT)
        .map_err(internal)?;
    let mut semantic_changed = false;
    let result = current_transaction(&mut connection, Some((&graph.connection, false)), |tx| {
        // A bound driver announces starting before reconciliation records runtime.running.
        // This hint only permits mailbox startup to wait; it grants no delivery or ready authority.
        if input.fields.get("state").and_then(Value::as_str) != Some("starting") {
            check_harness_event_runtime(tx, &input.subject, event_runtime)?;
        }
        if let Some(runtime) = event_runtime
            && input.fields.get("incarnation_id").and_then(Value::as_str) != Some(runtime)
        {
            return Err(St3Error::new(
                "stale-harness-event-session",
                "a current value must bind to its publishing runtime",
            ));
        }
        if event_runtime.is_none()
            && input.kind != "transport.observed"
            && input.fields.get("state").and_then(Value::as_str) != Some("starting")
            && let Some(incarnation) = input.fields.get("incarnation_id").and_then(Value::as_str)
        {
            let runtime: Option<String> = tx
                .query_row(
                    &format!(
                        "{} LIMIT 1",
                        newest_claims_of_kind_query("claims.body", "runtime.observed")
                    ),
                    params![input.subject, i64::MAX],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            if let Some(runtime) = runtime {
                let runtime: Value = serde_json::from_str(&runtime).map_err(internal)?;
                if runtime["fields"]["status"] == "running"
                    && runtime["fields"]["incarnation_id"] != incarnation
                {
                    return Err(St3Error::new(
                        "stale-harness-event-session",
                        "this is not the seat's running incarnation",
                    ));
                }
            }
        }
        // A new incarnation replaces the previous seat's context, rather than accumulating slots.
        let slot = if input.kind == "transport.observed" {
            graph.origin.clone()
        } else {
            String::new()
        };
        let previous: Option<LocalValue> = tx
            .query_row(
                "SELECT local_id,body,origin,actor,source_at FROM latest_values WHERE subject=?1 AND kind=?2 AND slot=?3",
                params![input.subject, input.kind, slot],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(internal)?;
        // Idempotency belongs to durable operations. Registers keep no retry keys or receipts.
        let mut input = input.clone();
        input.idempotency_key = None;
        if input.kind == "harness.observed" {
            input
                .fields
                .entry("observed_at_ms".into())
                .or_insert(json!(now));
            if input
                .fields
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|state| matches!(state, "ended" | "indeterminate"))
            {
                input.fields.insert("blocked_on".into(), Value::Null);
                input.fields.insert("ask".into(), Value::Null);
            }
            let mut transition = true;
            let source_at = input.fields["observed_at_ms"]
                .as_u64()
                .map(u128::from)
                .unwrap_or(now)
                .min(now);
            let mut since = input
                .fields
                .get("observed_since_ms")
                .cloned()
                .unwrap_or(json!(source_at));
            if let Some((_, body, _, _, _)) = &previous {
                let old: Value = serde_json::from_str(body).map_err(internal)?;
                if old["fields"]["incarnation_id"]
                    == input
                        .fields
                        .get("incarnation_id")
                        .cloned()
                        .unwrap_or(Value::Null)
                {
                    if let (Some(old_at), Some(new_at)) = (
                        old["fields"]["observed_at_ms"].as_u64(),
                        input.fields.get("observed_at_ms").and_then(Value::as_u64),
                    ) && new_at < old_at
                    {
                        let id: String = tx.query_row("SELECT source_id FROM latest_values WHERE subject=?1 AND kind=?2 AND slot=?3",
                        params![input.subject, input.kind, slot], |r| r.get(0)).map_err(internal)?;
                        return Ok((claim_by_id_tx(tx, &id).map_err(internal)?.unwrap(), false));
                    }
                    // Old producers can send sparse activity fields. Carry their known credential
                    // and source axes forward; explicit nulls in modern snapshots clear an axis.
                    for name in [
                        "driver",
                        "transport",
                        "provider_auth",
                        "provider_auth_sequence",
                        "reason",
                        "blocked_on",
                        "ask",
                        "input_buffer",
                        "exit",
                    ] {
                        if (!input.fields.contains_key(name)
                            || (name == "provider_auth" && input.fields[name].is_null()))
                            && let Some(value) = old["fields"].get(name)
                        {
                            input.fields.insert(name.into(), value.clone());
                        }
                    }
                }
                if (old["fields"]["state"] == input.fields["state"]
                    || (old["fields"]["provider_auth"] == false
                        && input.fields.get("provider_auth") == Some(&json!(false))))
                    && old["fields"]["incarnation_id"]
                        == input
                            .fields
                            .get("incarnation_id")
                            .cloned()
                            .unwrap_or(Value::Null)
                    && old["fields"].get("provider_auth").and_then(Value::as_bool)
                        == input.fields.get("provider_auth").and_then(Value::as_bool)
                    && permission_blocked(&old["fields"])
                        == (input.fields.get("blocked_on") == Some(&json!("human"))
                            && input.fields.get("ask") == Some(&json!("permission")))
                    && (old["fields"]["provider_auth"] == false
                        || (old["fields"]["reason"] == "providerAuth")
                            == (input.fields.get("reason") == Some(&json!("providerAuth"))))
                {
                    since = old["fields"]["observed_since_ms"].clone();
                    transition = false;
                } else {
                    // Native activity may retain its raw working episode's start while an
                    // approval prompt changes the effective state. Date that transition from
                    // its own source observation, including an explicit clearing snapshot.
                    since = json!(source_at);
                }
            }
            input.fields.insert("observed_since_ms".into(), since);
            input
                .fields
                .insert("status_transition".into(), json!(transition));
        }
        semantic_changed = previous
            .as_ref()
            .is_none_or(|(_, body, origin, actor, source_at)| {
                let old: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                origin != &graph.origin
                    || actor != &input.actor
                    || semantic_fields(&input.kind, &old["fields"])
                        != semantic_fields(&input.kind, &json!(input.fields))
                    || stale_observation(&input.kind, &old["fields"], u128::from(*source_at), now)
                        != stale_observation(&input.kind, &json!(input.fields), now, now)
            });
        seed_semantic_frontier(tx, &input.subject)?;
        let (mut local, _) = insert_local_observation_tx(tx, &graph.origin, &input, now)?;
        let epoch: String = tx
            .query_row(
                "SELECT value FROM meta WHERE key='current-value-epoch'",
                [],
                |row| row.get(0),
            )
            .map_err(internal)?;
        local.body["_source_epoch"] = json!(epoch);
        local.body["_semantic_transition"] = json!(semantic_changed);
        let sequence = local_observation_position(&local).unwrap();
        local.id = format!(
            "{LOCAL_OBSERVATION_ID_PREFIX}{}/{epoch}/{sequence}",
            graph.origin
        );
        tx.execute(
            "UPDATE local_observations SET body=?1 WHERE id=?2",
            params![
                canonical_json_text(&local.body).map_err(internal)?,
                sequence
            ],
        )
        .map_err(internal)?;
        // Retire the old current-value retry slots and local history at the first modern sample.
        // Numeric usage series keep their slots and observations.
        let retired_slots = if input.kind == "harness.usage" {
            tx.execute(
                "DELETE FROM local_latest_slots WHERE subject=?1 AND kind='harness.usage'
            AND json_extract(published_fields,'$.semantics')='context_occupancy'",
                [&input.subject],
            )
            .map_err(internal)?
        } else {
            tx.execute(
                "DELETE FROM local_latest_slots WHERE subject=?1 AND kind=?2",
                params![input.subject, input.kind],
            )
            .map_err(internal)?
        };
        // A legacy producer may have replaced the effective value since our last register.
        // Carry that transition to receivers without making them scan numeric claim history.
        if retired_slots != 0 && !semantic_changed {
            semantic_changed = true;
            local.body["_semantic_transition"] = json!(true);
            tx.execute(
                "UPDATE local_observations SET body=?1 WHERE id=?2",
                params![
                    canonical_json_text(&local.body).map_err(internal)?,
                    sequence
                ],
            )
            .map_err(internal)?;
        }
        // Queue legacy history retirement; each background pass visits at most 256 rows.
        // The current transaction never parses the retained numeric series.
        if (previous.is_none() || retired_slots != 0) && tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_observations WHERE subject=?1 AND kind=?2 AND id<?3)",
            params![input.subject,input.kind,sequence],|r|r.get::<_,bool>(0)).map_err(internal)? {
            tx.execute(
                "INSERT INTO current_value_retirements VALUES(?1,?2,0,?3)
                ON CONFLICT(subject,kind) DO UPDATE SET cutoff=max(cutoff,excluded.cutoff)",
                params![input.subject, input.kind, sequence - 1],
            )
            .map_err(internal)?;
        }
        record_semantic_frontier(tx, &input.subject, sequence as i64, semantic_changed)?;
        if let Some((old_id, _, _, _, _)) = previous {
            retire_feed_through(tx, old_id)?;
            tx.execute("DELETE FROM local_observations WHERE id=?1", [old_id])
                .map_err(internal)?;
        }
        tx.execute(
            "INSERT INTO latest_values VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(subject,kind,slot) DO UPDATE SET origin=excluded.origin,
         source_at=excluded.source_at, source_id=excluded.source_id, local_id=excluded.local_id,
         store_index=excluded.store_index, actor=excluded.actor, body=excluded.body",
            params![
                input.subject,
                input.kind,
                slot,
                graph.origin,
                now.min(i64::MAX as u128) as i64,
                local.id,
                local_observation_position(&local),
                local.store_index,
                local.actor,
                canonical_json_text(&local.body).map_err(internal)?
            ],
        )
        .map_err(internal)?;
        update_readiness(tx, &input)?;
        Ok((local, true))
    })?;
    if result.1 {
        graph
            .runtime
            .current_observation_committed(&input.kind, semantic_changed);
    }
    Ok(result)
}

fn update_readiness(tx: &Transaction<'_>, input: &ClaimInput) -> Result<(), St3Error> {
    if input.kind == "harness.observed" {
        let incarnation = input
            .fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ready = matches!(
            input.fields.get("state").and_then(Value::as_str),
            Some("ready" | "working" | "idle")
        ) && input.fields.get("reason").and_then(Value::as_str) != Some("providerAuth")
            && input.fields.get("provider_auth") != Some(&json!(false))
            && !(input.fields.get("blocked_on") == Some(&json!("human"))
                && input.fields.get("ask") == Some(&json!("permission")));
        tx.execute("INSERT INTO latest_readiness VALUES (?1,?2,?3)
            ON CONFLICT(subject) DO UPDATE SET incarnation=excluded.incarnation,
            ready=excluded.ready OR (latest_readiness.incarnation=excluded.incarnation AND latest_readiness.ready)",
            params![input.subject,incarnation,ready]).map_err(internal)?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn take_current_capacity_kind(&self) -> Option<String> {
        self.smalltalk
            .current_value_capacity_kinds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
    pub(crate) fn current_value_maintenance_wake(&self) -> &tokio::sync::Notify {
        &self.smalltalk.current_value_maintenance_wake
    }

    /// The explicit reconnect boundary for consumers of current-observation transitions.
    /// Read this alongside the feed/snapshot in a pinned read transaction. A cursor behind
    /// retired evidence must resync from current values rather than promise missing history.
    pub fn current_observation_boundary(&self) -> Result<CurrentObservationBoundary> {
        Ok(self.readers.get().query_row(
            "SELECT (SELECT value FROM meta WHERE key='current-value-epoch'),
                    COALESCE((SELECT seq FROM sqlite_sequence WHERE name='local_observations'),0),
                    COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key='current-value-retired-through'),0)",
            [], |row| Ok(CurrentObservationBoundary {
                epoch: row.get(0)?, local_cursor: row.get(1)?,
                retired_through_local_cursor: row.get(2)?,
            }),
        )?)
    }

    /// Refusals and peer errors are durable evidence and queue through the managed writer.
    /// Only the accompanying replaceable transport observation may be dropped on contention.
    pub fn record_peer_failure(&self, peer: &str, status: &str, error: &str) -> Result<bool> {
        self.graph.record_peer_failure(peer, status, error)
    }

    /// Connectivity is a separate register for each observer, not replication inventory.
    pub fn record_transport_observation(
        &self,
        peer: &str,
        status: &str,
        reason: Option<&str>,
        last_success_at: Option<u128>,
    ) -> Result<()> {
        // Use the graph's observer-scoped refresh and clock-skew rules. Its runtime
        // routes current observations to the same bounded register writer, so an
        // unchanged success keeps its original source time and identity until refresh.
        // A busy daemon drops connectivity just like a driver drops status.
        let _ = self
            .graph
            .record_transport_observation(peer, status, reason, last_success_at);
        Ok(())
    }

    /// A read-only lifecycle check for a kernel-bound native launch awaiting publication.
    /// The API additionally requires matching live PTY ancestry. This grants no ownership.
    pub(crate) fn mailbox_bootstrap_pending(
        &self,
        request: &crate::mailbox::Fence,
    ) -> Result<bool, St3Error> {
        if request.epoch != 0 || request.token.is_empty() || request.token.len() > 128 {
            return Ok(false);
        }
        self.read_snapshot(|_| {
            let connection = self.readers.get();
            let prior: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM local_mailbox_bindings WHERE token=?1)",
                [&request.token],
                |row| row.get(0),
            )?;
            if prior {
                return Ok(false);
            }
            let runtime: Option<String> = connection
                .query_row(
                    &format!(
                        "{} LIMIT 1",
                        newest_claims_of_kind_query("claims.body", "runtime.observed")
                    ),
                    params![request.subject, i64::MAX],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(runtime) = runtime {
                let body: Value = serde_json::from_str(&runtime)?;
                let fields = body.get("fields").unwrap_or(&body);
                // A predecessor may still be marked running. Only this incarnation's
                // running/terminal evidence closes bootstrap; the API's double-checked
                // kernel/PTY proof must establish any successor before it receives a retry.
                if fields["incarnation_id"].as_str() == Some(request.incarnation.as_str())
                    && !matches!(fields["status"].as_str(), None | Some("starting"))
                {
                    return Ok(false);
                }
            }
            let harness: Option<String> = connection
                .query_row(
                    &harness_sql(
                        &connection,
                        &request.subject,
                        &format!(
                            "{} LIMIT 1",
                            newest_claims_of_kind_query("claims.body", "harness.observed")
                        ),
                    )?,
                    params![request.subject, i64::MAX],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(harness) = harness {
                let body: Value = serde_json::from_str(&harness)?;
                let fields = body.get("fields").unwrap_or(&body);
                if fields["incarnation_id"].as_str() == Some(request.incarnation.as_str())
                    && fields["state"] == "ended"
                {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .map_err(internal)
    }

    pub fn transport_links(&self) -> Result<Vec<(String, String)>> {
        // The graph's source-clock, skew, refresh and dial-out fences also apply to
        // registers through Runtime::current_observation_sql. Keep one route reducer.
        self.graph.transport_links()
    }

    /// Housekeeping is off the request path: inspect at most 64 keys and retire at most
    /// 256 history rows. Empty passes never take SQLite's write lock.
    #[cfg(test)]
    pub(crate) fn maintain_current_values(
        &self,
        hosts: &BTreeSet<String>,
        after: &str,
    ) -> Result<CurrentMaintenance, St3Error> {
        self.maintain_current_values_of_kind(hosts, after, None)
    }

    pub(crate) fn maintain_current_values_of_kind(
        &self,
        hosts: &BTreeSet<String>,
        after: &str,
        only_kind: Option<&str>,
    ) -> Result<CurrentMaintenance, St3Error> {
        let (subject, kind, slot): (String, String, String) =
            serde_json::from_str(after).unwrap_or_default();
        let reader = self.readers.get();
        let sql = if only_kind.is_some() {
            "SELECT subject,kind,slot,origin,local_id,source_id FROM latest_values
             WHERE kind=?4 AND (subject,slot)>(?1,?3) ORDER BY subject,slot LIMIT 64"
        } else {
            "SELECT subject,kind,slot,origin,local_id,source_id FROM latest_values
             WHERE (subject,kind,slot)>(?1,?2,?3) ORDER BY subject,kind,slot LIMIT 64"
        };
        let mut statement = reader.prepare_cached(sql).map_err(internal)?;
        // The kind scan uses the same tuple cursor, seeking only that kind's index.
        let values = [
            Some(subject.as_str()),
            Some(kind.as_str()),
            Some(slot.as_str()),
            only_kind,
        ];
        let count = if only_kind.is_some() { 4 } else { 3 };
        let keys = statement
            .query_map(rusqlite::params_from_iter(&values[..count]), |row| {
                Ok(CurrentKey {
                    subject: row.get(0)?,
                    kind: row.get(1)?,
                    slot: row.get(2)?,
                    origin: row.get(3)?,
                    local_id: row.get(4)?,
                    source_id: row.get(5)?,
                })
            })
            .map_err(internal)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?;
        drop(statement);
        let next = if keys.len() == 64 {
            let key = keys.last().unwrap();
            serde_json::to_string(&(&key.subject, &key.kind, &key.slot)).map_err(internal)?
        } else {
            String::new()
        };
        let mut obsolete = Vec::new();
        for key in keys {
            if obsolete_key(&reader, &key, hosts)? {
                obsolete.push(key);
            }
        }
        let history_work = prepare_history_batch(&reader);
        drop(reader);
        if obsolete.is_empty() && matches!(history_work, Ok(None)) {
            return Ok(CurrentMaintenance {
                removed: 0,
                history_visited: 0,
                after: next,
            });
        }
        let mut connection = Connection::open_with_flags(
            &self.graph.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(internal)?;
        smallclaims::sqlite::observe(&mut connection);
        connection
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(internal)?;
        let (removed, kinds) = if obsolete.is_empty() {
            (0, BTreeSet::new())
        } else {
            current_transaction(&mut connection, Some((&self.graph.connection, true)), |tx| {
                let mut removed = 0;
                let mut kinds = BTreeSet::new();
                let mut deleted_subjects = BTreeSet::new();
                for key in &obsolete {
                    if !obsolete_key(tx, key, hosts)? {
                        continue;
                    }
                    let count=tx.execute("DELETE FROM latest_values WHERE subject=?1 AND kind=?2 AND slot=?3 AND source_id=?4",
                    params![key.subject,key.kind,key.slot,key.source_id]).map_err(internal)?;
                    if count == 0 {
                        continue;
                    }
                    retire_feed_through(tx, key.local_id)?;
                    tx.execute("DELETE FROM local_observations WHERE id=?1", [key.local_id])
                        .map_err(internal)?;
                    if key.kind == "harness.observed" {
                        tx.execute(
                            "DELETE FROM latest_readiness WHERE subject=?1",
                            [&key.subject],
                        )
                        .map_err(internal)?;
                    }
                    deleted_subjects.insert(key.subject.clone());
                    kinds.insert(key.kind.clone());
                    removed += count;
                }
                if !deleted_subjects.is_empty() {
                    let clock = consume_observation_clock(tx)?;
                    for subject in deleted_subjects {
                        record_semantic_frontier(tx, &subject, clock, true)?;
                    }
                    retire_feed_through(tx, clock)?;
                }
                Ok((removed, kinds))
            })?
        };
        for kind in kinds {
            self.graph
                .runtime
                .current_observation_committed(&kind, true);
        }
        // History retirement owns a separate transaction: a bad job cannot roll back
        // already committed collection or prevent its cursor from advancing.
        let skipped_job = history_work
            .as_ref()
            .ok()
            .and_then(|job| job.as_ref())
            .and_then(|job| {
                job.parse_error
                    .as_ref()
                    .map(|error| (job.subject.clone(), job.kind.clone(), error.clone()))
            });
        let history = match history_work {
            Err(error) => Err(error),
            Ok(None) => Ok(0),
            Ok(Some(job)) => current_transaction(&mut connection, Some((&self.graph.connection, true)), |tx| {
                let bounds: Option<(i64, i64)> = tx
                    .query_row(
                        "SELECT cursor,cutoff FROM current_value_retirements
                    WHERE subject=?1 AND kind=?2",
                        params![job.subject, job.kind],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                let Some((cursor, cutoff)) = bounds else {
                    return Ok(0);
                };
                if cursor != job.cursor {
                    return Ok(0);
                };
                if job.parse_error.is_some() {
                    if cutoff != job.cutoff {
                        return Ok(0);
                    }
                    tx.execute(
                        "DELETE FROM current_value_retirements WHERE subject=?1 AND kind=?2",
                        params![job.subject, job.kind],
                    )
                    .map_err(internal)?;
                    return Ok(job.visited);
                }
                let deleted=tx.prepare_cached("DELETE FROM local_observations
                    WHERE id IN (SELECT value FROM json_each(?1))
                    AND NOT EXISTS(SELECT 1 FROM latest_values v WHERE v.local_id=local_observations.id)
                    RETURNING id").map_err(internal)?
                    .query_map([&job.delete],|r|r.get::<_,i64>(0)).map_err(internal)?
                    .collect::<rusqlite::Result<Vec<_>>>().map_err(internal)?;
                if let Some(last) = deleted.iter().max() {
                    retire_feed_through(tx, *last)?;
                }
                if job.visited < 256 && cutoff == job.cutoff {
                    tx.execute(
                        "DELETE FROM current_value_retirements WHERE subject=?1 AND kind=?2",
                        params![job.subject, job.kind],
                    )
                    .map_err(internal)?;
                } else {
                    tx.execute("UPDATE current_value_retirements SET cursor=?3 WHERE subject=?1 AND kind=?2",
                        params![job.subject,job.kind,job.last]).map_err(internal)?;
                }
                Ok(job.visited)
            }),
        };
        let history_visited = match history {
            Ok(visited) => {
                if visited != 0
                    && let Some((subject, kind, error)) = skipped_job
                {
                    eprintln!(
                        "st3: skipped malformed current history retirement {subject} {kind}: {error}; payloads retained"
                    );
                }
                visited
            }
            Err(error) => {
                eprintln!("st3: current history retirement deferred: {error:?}");
                0
            }
        };
        Ok(CurrentMaintenance {
            removed,
            history_visited,
            after: next,
        })
    }

    pub(crate) fn own_transport_value(&self, peer: &str) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        let id: Option<String> = connection.query_row(
            "SELECT source_id FROM latest_values WHERE subject=?1 AND kind='transport.observed' AND slot=?2",
            params![format!("host/{peer}"),self.origin], |row| row.get(0)).optional()?;
        id.map(|id| claim_by_id_tx(&connection, &id))
            .transpose()
            .map(Option::flatten)
    }

    /// A signed fleet sender supplies its own current observation. Imported registers never
    /// enter replication inventories, and an older delivery cannot overwrite a newer value.
    pub fn receive_current_value(&self, record: &ClaimRecord) -> Result<bool, St3Error> {
        let view = self.fleet_view_sealed().map_err(internal)?;
        let mut hosts = view
            .members
            .iter()
            .filter(|member| member.state == "current")
            .map(|member| member.name.clone())
            .collect::<BTreeSet<_>>();
        hosts.insert(self.origin().into());
        if view.anchor.is_none() {
            hosts.insert(record.origin.clone());
        }
        self.receive_current_value_for_hosts(record, &hosts)
            .map(|outcome| outcome.0)
    }

    pub(crate) fn receive_current_value_for_hosts(
        &self,
        record: &ClaimRecord,
        hosts: &BTreeSet<String>,
    ) -> Result<(bool, bool), St3Error> {
        if record.accepted_at_unix_ms > now_ms().saturating_add(60_000) {
            return Err(St3Error::new(
                "invalid-current-value",
                "current value sender clock is too far in the future",
            ));
        }
        if !is_current_value(&record.kind)
            || (record.kind == "harness.usage"
                && record.body["fields"]["semantics"] != "context_occupancy")
        {
            return Err(St3Error::new(
                "invalid-current-value",
                "this kind is not a current value",
            ));
        }
        let fields = schema_fields_for_body(&record.kind, &record.body).map_err(internal)?;
        st3_schema::registry()
            .validate_claim(&record.subject, &record.kind, &fields)
            .map_err(|e| St3Error::new(e.code, e.message))?;
        let slot = if record.kind == "transport.observed" {
            record.origin.clone()
        } else {
            String::new()
        };
        let mut connection = Connection::open_with_flags(
            &self.graph.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(internal)?;
        smallclaims::sqlite::observe(&mut connection);
        connection
            .busy_timeout(CURRENT_WRITER_WAIT)
            .map_err(internal)?;
        let mut semantic_changed = false;
        let changed = current_transaction(&mut connection, Some((&self.graph.connection, false)), |tx| {
            let mut incarnation_bound = false;
            if record.kind != "transport.observed" {
                let owner: Option<String> = tx
                    .query_row(
                        "SELECT json_extract(member,'$.host') FROM desired
                     WHERE subject=?1 AND kind='agent' AND member IS NOT NULL",
                        [&record.subject],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if owner.as_deref() != Some(record.origin.as_str()) {
                    return Err(St3Error::new(
                        "stale-harness-event-session",
                        "current values require a declared seat on the publishing host",
                    ));
                }
            } else if !record.subject.strip_prefix("host/").is_some_and(|host| hosts.contains(host))
                || !hosts.contains(&record.origin) {
                return Err(St3Error::new(
                    "invalid-current-value",
                    "transport values require an active fleet host and observer",
                ));
            }
            if record.subject.starts_with("agent/") {
                let runtime: Option<(String, String)> = tx
                    .query_row(
                        &format!(
                            "{} LIMIT 1",
                            newest_claims_of_kind_query(
                                "claims.origin,claims.body",
                                "runtime.observed"
                            )
                        ),
                        params![record.subject, i64::MAX],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(internal)?;
                if let Some((owner, body)) = runtime {
                    let body: Value = serde_json::from_str(&body).map_err(internal)?;
                    let running = body["fields"]["status"] == "running";
                    // Workspace availability belongs to the host reconciler, not a native
                    // harness incarnation. It must still come from the running seat's host.
                    incarnation_bound = running && record.kind != "workspace.observed";
                    if running
                        && (owner != record.origin
                            || (incarnation_bound
                                && body["fields"]["incarnation_id"]
                                    != record.body["fields"]["incarnation_id"]))
                    {
                        return Err(St3Error::new(
                            "stale-harness-event-session",
                            "current value owner or incarnation is not the running seat",
                        ));
                    }
                }
            }
            let previous: Option<ReceivedValue> = tx.query_row(
            "SELECT source_at,source_id,local_id,origin,body,actor FROM latest_values WHERE subject=?1 AND kind=?2 AND slot=?3",
            params![record.subject,record.kind,slot], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
            .optional().map_err(internal)?;
            let source_sequence = |id: &str| {
                id.rsplit('/')
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or_default()
            };
            if previous
                .as_ref()
                .is_some_and(|(at, id, _, origin, body, _)| {
                    if origin == &record.origin
                        && id.starts_with(LOCAL_OBSERVATION_ID_PREFIX)
                        && record.id.starts_with(LOCAL_OBSERVATION_ID_PREFIX)
                    {
                        let previous: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                        // A restored database may rewind its generation and counter. A newly running
                        // incarnation, already validated against durable owner evidence above, cuts over.
                        if incarnation_bound
                            && previous["fields"]["incarnation_id"]
                                != record.body["fields"]["incarnation_id"]
                        {
                            return false;
                        }
                        let epoch = |body: &Value| {
                            body["_source_epoch"]
                                .as_str()
                                .and_then(|value| value.parse::<u128>().ok())
                                .unwrap_or(0)
                        };
                        return (epoch(&previous), source_sequence(id))
                            >= (epoch(&record.body), source_sequence(&record.id));
                    }
                    (*at, source_sequence(id), id.as_str())
                        >= (
                            record.accepted_at_unix_ms as u64,
                            source_sequence(&record.id),
                            record.id.as_str(),
                        )
                })
            {
                return Ok(false);
            }
            // Existing keys are always replaceable. Bound new keys even if a trusted fleet
            // member floods host names or declarations have accumulated since retirement.
            if previous.is_none()
                && tx
                    .query_row("SELECT count(*) FROM latest_values WHERE kind=?1", [&record.kind], |row| {
                        row.get::<_, u64>(0)
                    })
                    .map_err(internal)?
                    >= 10_000
            {
                return Err(St3Error::new(
                    "current-value-capacity",
                    "current register capacity reached",
                ));
            }
            semantic_changed =
                previous
                    .as_ref()
                    .is_none_or(|(source_at, _, _, origin, body, actor)| {
                        let old: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                        // Older senders lack semantic metadata; preserve their conservative
                        // invalidation during the mixed-version window.
                        record.body["_semantic_transition"] != false
                            || origin != &record.origin
                            || actor != &record.actor
                            || semantic_fields(&record.kind, &old["fields"])
                                != semantic_fields(&record.kind, &record.body["fields"])
                            || stale_observation(
                                &record.kind,
                                &old["fields"],
                                u128::from(*source_at),
                                now_ms(),
                            ) != stale_observation(
                                &record.kind,
                                &record.body["fields"],
                                record.accepted_at_unix_ms,
                                now_ms(),
                            )
                    });
            let input = ClaimInput {
                subject: record.subject.clone(),
                kind: record.kind.clone(),
                actor: record.actor.clone(),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            };
            seed_semantic_frontier(tx, &record.subject)?;
            let (local, _) =
                insert_local_observation_tx(tx, &self.origin, &input, record.accepted_at_unix_ms)?;
            if let Some((_, _, old_id, _, _, _)) = previous {
                retire_feed_through(tx, old_id)?;
                tx.execute("DELETE FROM local_observations WHERE id=?1", [old_id])
                    .map_err(internal)?;
            }
            record_semantic_frontier(tx, &record.subject, local_observation_position(&local).unwrap() as i64, semantic_changed)?;
            tx.execute(
            "INSERT INTO latest_values VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(subject,kind,slot) DO UPDATE SET origin=excluded.origin,source_at=excluded.source_at,
             source_id=excluded.source_id,local_id=excluded.local_id,store_index=excluded.store_index,actor=excluded.actor,body=excluded.body",
            params![record.subject,record.kind,slot,record.origin,record.accepted_at_unix_ms as u64,
                record.id,local_observation_position(&local),local.store_index,record.actor,canonical_json_text(&record.body).map_err(internal)?]
        ).map_err(internal)?;
            update_readiness(tx, &input)?;
            Ok(true)
        }).inspect_err(|error| {
            if error.code == "current-value-capacity" {
                let count = self.smalltalk.current_value_refusals.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                {
                    let mut kinds = self.smalltalk.current_value_capacity_kinds.lock().unwrap_or_else(PoisonError::into_inner);
                    if !kinds.iter().any(|kind| kind == &record.kind) { kinds.push_back(record.kind.clone()); }
                }
                self.smalltalk.current_value_maintenance_wake.notify_one();
                if count == 1 || count.is_multiple_of(100) {
                    eprintln!("st3: current register capacity refused {} {} (refusal {count}); collector will reclaim obsolete keys", record.subject, record.kind);
                }
            }
        })?;
        if changed {
            self.graph
                .runtime
                .current_observation_committed(&record.kind, semantic_changed);
        }
        Ok((changed, changed && semantic_changed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_native_admission_is_thread_local_and_restored_after_unwind() {
        assert!(!NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get));
        let result = std::panic::catch_unwind(|| with_native_current_admission_for_test(true, || {
            assert!(NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get));
            std::thread::spawn(|| assert!(!NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get)))
                .join().unwrap();
            with_native_current_admission_for_test(false, || {
                assert!(!NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get));
            });
            assert!(NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get));
            panic!("restore diagnostic flag");
        }));
        assert!(result.is_err());
        assert!(!NATIVE_ADMISSION_CONTROL.with(std::cell::Cell::get));
    }


    fn state(state: &str, incarnation: &str, at: u64) -> ClaimInput {
        ClaimInput {
            subject: "agent/example/cedar".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/example/cedar".into()),
            fields: serde_json::from_value(json!({
                "state":state, "driver":"codex", "incarnation_id":incarnation,
                "observed_at_ms":at, "observed_since_ms":at,
            }))
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        }
    }

    fn declare(store: &Store, owner: &str) {
        let intent = crate::graph::parse_intent(
            "version 2\nagent \"example/cedar\" { workspace \"/tmp\"; argv \"fixture\"; }",
            owner,
        )
        .unwrap();
        store
            .apply_internal(&intent, "current-value-declaration")
            .unwrap();
    }

    fn drain_maintenance(store: &Store) -> usize {
        let hosts = BTreeSet::from([store.origin().to_owned()]);
        let mut after = String::new();
        let mut visited = 0;
        for _ in 0..200 {
            let batch = store.maintain_current_values(&hosts, &after).unwrap();
            assert!(batch.removed <= 64 && batch.history_visited <= 256);
            after = batch.after;
            visited += batch.history_visited;
            let jobs: u64 = store
                .readers
                .get()
                .query_row("SELECT count(*) FROM current_value_retirements", [], |r| {
                    r.get(0)
                })
                .unwrap();
            if jobs == 0 {
                return visited;
            }
        }
        panic!("bounded retirement did not finish");
    }

    #[test]
    fn cached_full_status_refreshes_restamps_without_mixing_semantic_sources() {
        let store = Store::open_memory("owner").unwrap();
        declare(&store, "owner");
        let mut runtime = state("idle", "one", 1);
        runtime.kind = "runtime.observed".into();
        runtime.fields =
            serde_json::from_value(json!({"status":"running","incarnation_id":"one"})).unwrap();
        store.append_claim(&runtime).unwrap();
        let first = store
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        let kept = store
            .status_for_subject_prefix_at("agent/", None, false)
            .unwrap();
        let revision = store.current_cache_revision().unwrap();
        let mut old = kept
            .subjects
            .iter()
            .find(|s| s.subject == first.subject)
            .unwrap()
            .clone();
        assert_eq!(old.harness.as_ref().unwrap().claim, first.id);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let restamp = store
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        assert_ne!(restamp.id, first.id);
        assert_eq!(store.current_cache_revision().unwrap(), revision);
        let prefix = store
            .status_for_subject_prefix_at("agent/", None, false)
            .unwrap();
        let kinds = store
            .status_for_claim_kind_at("runtime.observed", None, false)
            .unwrap();
        for answer in [&prefix, &kinds] {
            let harness = answer
                .subjects
                .iter()
                .find(|s| s.subject == first.subject)
                .unwrap()
                .harness
                .as_ref()
                .unwrap();
            assert_eq!(harness.claim, restamp.id);
            assert_eq!(harness.state, "idle");
        }
        store.forget_current_views();
        assert_eq!(
            serde_json::to_value(&prefix).unwrap(),
            serde_json::to_value(
                store
                    .status_for_subject_prefix_at("agent/", None, false)
                    .unwrap()
            )
            .unwrap()
        );
        assert_eq!(
            serde_json::to_value(&kinds).unwrap(),
            serde_json::to_value(
                store
                    .status_for_claim_kind_at("runtime.observed", None, false)
                    .unwrap()
            )
            .unwrap()
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        let working = store
            .append_claim(&state("working", "one", now_ms() as u64))
            .unwrap();
        assert!(store.current_cache_revision().unwrap() > revision);
        // Deterministically place a semantic commit between cache lookup and source overlay.
        store
            .refresh_kept_harness_source(&store.readers.get(), &mut old, revision)
            .unwrap();
        assert_eq!(old.harness.as_ref().unwrap().claim, first.id);
        assert_eq!(old.harness.as_ref().unwrap().state, "idle");
        let current = store
            .status_for_subject_prefix_at("agent/", None, false)
            .unwrap();
        let harness = current
            .subjects
            .iter()
            .find(|s| s.subject == first.subject)
            .unwrap()
            .harness
            .as_ref()
            .unwrap();
        assert_eq!(harness.claim, working.id);
        assert_eq!(harness.state, "working");
    }

    #[test]
    fn first_context_sample_with_twenty_thousand_numeric_rows_has_bounded_request_work() {
        let store = Store::open_memory("owner").unwrap();
        declare(&store, "owner");
        let mut input = state("idle", "one", 1);
        input.kind = "harness.usage".into();
        input.fields = serde_json::from_value(json!({"driver":"codex","incarnation_id":"one",
            "semantics":"context_occupancy","context_used_tokens":10}))
        .unwrap();
        append_legacy_graph_observation_fenced(&store.graph, &input, 1, None).unwrap();
        store
            .connection
            .write()
            .execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<20000)
            INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            SELECT 0,'agent/example/cedar','harness.usage',?1,1 FROM n",
                [
                    json!({"fields":{"semantics":"session_cumulative","total_tokens":1}})
                        .to_string(),
                ],
            )
            .unwrap();
        CURRENT_TRANSACTION_STEPS.with(|v| v.set(Some(0)));
        CURRENT_TRANSACTION_ELAPSED.with(|v| v.set(Some(std::time::Duration::ZERO)));
        append(&store.graph, &input, 2, None).unwrap();
        let steps = CURRENT_TRANSACTION_STEPS.with(|v| v.replace(None).unwrap());
        let hold = CURRENT_TRANSACTION_ELAPSED.with(|v| v.replace(None).unwrap());
        println!("20k first context: request VM steps={steps}, writer hold={hold:?}");
        assert!(
            steps > 0 && steps < 4000,
            "first publication must not scan numeric history: {steps}"
        );
        assert!(hold < crate::client::LATEST_VALUE_TIMEOUT);
        let revision = store.graph.runtime.current_observation_revision("");
        let frontier = store.current_cache_revision().unwrap();
        assert_eq!(drain_maintenance(&store), 20_001);
        assert_eq!(
            store.graph.runtime.current_observation_revision(""),
            revision
        );
        assert_eq!(store.current_cache_revision().unwrap(), frontier);
        let reader = store.readers.get();
        let counts: (u64, u64) = reader
            .query_row(
                "SELECT
            sum(json_extract(body,'$.fields.semantics')='session_cumulative'),
            sum(json_extract(body,'$.fields.semantics')='context_occupancy')
            FROM local_observations WHERE kind='harness.usage'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (20_000, 1));
    }

    #[test]
    fn login_evidence_respects_newer_legacy_clearing_observations() {
        let store = Store::open_memory("owner").unwrap();
        let mut input = state("idle", "one", 1);
        input.fields.insert("provider_auth".into(), json!(false));
        append(&store.graph, &input, 1, None).unwrap();
        assert!(
            login_evidence_in_epoch(
                &store.readers.get(),
                &input.subject,
                store.index().unwrap(),
                "one",
                "0"
            )
            .unwrap()
        );
        input.fields.insert("provider_auth".into(), json!(true));
        store.append_legacy_claim(&input).unwrap();
        assert!(
            !login_evidence_in_epoch(
                &store.readers.get(),
                &input.subject,
                store.index().unwrap(),
                "one",
                "0"
            )
            .unwrap()
        );
        input.fields.insert("provider_auth".into(), json!(false));
        store.append_legacy_claim(&input).unwrap();
        assert!(
            login_evidence_in_epoch(
                &store.readers.get(),
                &input.subject,
                store.index().unwrap(),
                "one",
                "0"
            )
            .unwrap()
        );
    }

    #[test]
    fn existing_registers_initialize_semantic_frontiers_in_bounded_pages() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("frontiers.sqlite3");
        let baseline;
        {
            let store = Store::open(&path, "owner").unwrap();
            for n in 0..130 {
                let mut input = state("idle", "one", 1);
                input.subject = format!("agent/old/{n}");
                append(&store.graph, &input, 1, None).unwrap();
            }
            baseline = store.current_cache_revision().unwrap();
            store
                .connection
                .write()
                .execute_batch(
                    "DELETE FROM current_value_frontiers;
                DELETE FROM meta WHERE key='current-value-frontiers';",
                )
                .unwrap();
        }
        let store = Store::open(&path, "owner").unwrap();
        assert_eq!(store.current_cache_revision().unwrap(), baseline);
        assert_eq!(
            store
                .readers
                .get()
                .query_row("SELECT count(*) FROM current_value_frontiers", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            130
        );
        let mut input = state("idle", "one", 1);
        input.subject = "agent/old/0".into();
        append(&store.graph, &input, 2, None).unwrap();
        assert_eq!(store.current_cache_revision().unwrap(), baseline);
        assert!(store.changed_current_agents(baseline).unwrap().is_empty());
    }

    #[test]
    fn reopen_repairs_an_older_collectors_clock_drift() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("clock.sqlite3");
        let old_frontier;
        {
            let store = Store::open(&path, "owner").unwrap();
            append(&store.graph, &state("idle", "one", 1), 1, None).unwrap();
            old_frontier = store.current_cache_revision().unwrap() + 64;
            store
                .connection
                .write()
                .execute(
                    "UPDATE current_value_frontiers SET revision=?1",
                    [old_frontier],
                )
                .unwrap();
        }
        let store = Store::open(&path, "owner").unwrap();
        assert_eq!(
            store.current_observation_boundary().unwrap().local_cursor,
            old_frontier
        );
        append(&store.graph, &state("working", "one", 2), 2, None).unwrap();
        assert_eq!(store.current_cache_revision().unwrap(), old_frontier + 1);
    }

    #[test]
    fn categorical_history_retires_without_a_legacy_slot() {
        let store = Store::open_memory("owner").unwrap();
        declare(&store, "owner");
        let input = state("idle", "one", 1);
        store.connection.write().execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            VALUES(0,?1,?2,?3,1)",params![input.subject,input.kind,json!({"fields":input.fields}).to_string()]).unwrap();
        append(&store.graph, &input, 2, None).unwrap();
        assert_eq!(drain_maintenance(&store), 1);
        let remaining: u64 = store
            .readers
            .get()
            .query_row(
                "SELECT count(*) FROM local_observations WHERE kind='harness.observed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 1,
            "the live register is protected while old history is collected"
        );
    }

    #[test]
    fn retired_register_capacity_is_reclaimed_for_a_live_seat() {
        let source = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "owner");
        peer.connection.write().execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<20000)
            INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            SELECT 0,CASE WHEN i<=10000 THEN 'agent/retired/'||i ELSE 'host/retired-'||(i-10000) END,
                CASE WHEN i<=10000 THEN 'harness.observed' ELSE 'transport.observed' END,'{}',1 FROM n",[]).unwrap();
        peer.connection
            .write()
            .execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000)
            INSERT INTO latest_values SELECT 'agent/retired/'||i,'harness.observed','','owner',1,
            'retired/'||i,i,0,NULL,'{}' FROM n",
                [],
            )
            .unwrap();
        peer.connection.write().execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000)
            INSERT INTO latest_values SELECT 'host/retired-'||i,'transport.observed','owner','owner',1,
            'transport/'||i,10000+i,0,NULL,'{}' FROM n",[]).unwrap();
        let hosts = BTreeSet::from(["owner".into(), "peer".into()]);
        let record = source.append_claim(&state("idle", "one", 10)).unwrap();
        assert_eq!(
            peer.receive_current_value_for_hosts(&record, &hosts)
                .unwrap_err()
                .code,
            "current-value-capacity"
        );
        assert_eq!(
            peer.smalltalk
                .current_value_refusals
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let batch = peer.maintain_current_values(&hosts, "").unwrap();
        assert_eq!(batch.removed, 64);
        assert!(
            peer.receive_current_value_for_hosts(&record, &hosts)
                .unwrap()
                .0,
            "transport keys reserve separate capacity and obsolete seat keys are reclaimed"
        );
        let frontier = peer.current_cache_revision().unwrap();
        let mut after = batch.after;
        let mut removed = batch.removed;
        for _ in 0..400 {
            let batch = peer.maintain_current_values(&hosts, &after).unwrap();
            assert!(batch.removed <= 64);
            removed += batch.removed;
            after = batch.after;
            if after.is_empty() {
                break;
            }
        }
        assert_eq!(removed, 20_000);
        assert_eq!(
            peer.readers
                .get()
                .query_row("SELECT count(*) FROM latest_values", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert!(
            peer.current_cache_revision().unwrap() > frontier,
            "deletion must invalidate without rewinding clocks"
        );
        assert_eq!(
            peer.readers
                .get()
                .query_row("SELECT count(*) FROM local_observations", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1,
            "collection must preserve the live seat's feed row"
        );
        assert!(
            peer.latest_claim(&record.subject, Some(&record.kind))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    #[ignore = "whole-process SQLite counters: run this cost probe alone"]
    fn context_register_cost_is_flat_with_twenty_thousand_numeric_rows() {
        use smallclaims::sqlite::work;
        let store = Store::open_memory("owner").unwrap();
        declare(&store, "owner");
        let mut input = state("idle", "one", 1);
        input.kind = "harness.usage".into();
        input.fields = serde_json::from_value(json!({"driver":"codex", "incarnation_id":"one",
            "semantics":"context_occupancy", "context_used_tokens":10}))
        .unwrap();
        let seed = |count: usize| {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?1)
                INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                SELECT 0,'agent/example/cedar','harness.usage',?2,1 FROM n", params![count,
                json!({"fields":{"semantics":"session_cumulative","total_tokens":1}}).to_string()]).unwrap();
            tx.commit().unwrap();
        };
        append_legacy_graph_observation_fenced(&store.graph, &input, 1, None).unwrap();
        seed(2_000);
        append(&store.graph, &input, 2, None).unwrap();
        drain_maintenance(&store);
        let measure = |at| {
            let before = work::total();
            append(&store.graph, &input, at, None).unwrap();
            work::total() - before
        };
        let small = measure(3);
        seed(18_000);
        let big = measure(4);
        println!("context-cost small={small:?} big={big:?}");
        assert_eq!(small.statements, big.statements);
        assert!(big.vm_steps <= small.vm_steps + 100, "{small:?} -> {big:?}");
        assert_eq!(small.fullscan_steps, big.fullscan_steps);
        assert!(big.fullscan_steps <= 1, "only constant setup work may scan");
        let connection = store.readers.get();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM local_observations WHERE
            kind='harness.usage' AND json_extract(body,'$.fields.semantics')='session_cumulative'",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            20_000
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM local_observations WHERE
            kind='harness.usage' AND json_extract(body,'$.fields.semantics')='context_occupancy'",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn broken_history_job_does_not_roll_back_register_collection() {
        let store = Store::open_memory("owner").unwrap();
        let mut input = state("idle", "one", 1);
        input.subject = "agent/retired".into();
        append(&store.graph, &input, 1, None).unwrap();
        store.connection.write().execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            VALUES(0,'agent/broken','harness.usage','invalid-json',1)",[]).unwrap();
        let cutoff = store.current_observation_boundary().unwrap().local_cursor;
        store
            .connection
            .write()
            .execute(
                "INSERT INTO current_value_retirements VALUES('agent/broken','harness.usage',0,?1)",
                [cutoff],
            )
            .unwrap();
        let frontier = store.current_cache_revision().unwrap();
        let batch = store
            .maintain_current_values(&BTreeSet::from(["owner".into()]), "")
            .unwrap();
        assert_eq!(batch.removed, 1);
        assert_eq!(batch.history_visited, 1);
        assert!(store.current_cache_revision().unwrap() > frontier);
        assert_eq!(
            store
                .readers
                .get()
                .query_row("SELECT count(*) FROM latest_values", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        let reader = store.readers.get();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM current_value_retirements", [], |r| {
                    r.get::<_, u64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            reader
                .query_row(
                    "SELECT body FROM local_observations WHERE subject='agent/broken'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "invalid-json"
        );
        drop(reader);
        store.connection.write().execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            VALUES(0,'agent/zz-history','harness.usage','{\"fields\":{\"semantics\":\"context_occupancy\"}}',1)",[]).unwrap();
        let cutoff = store.current_observation_boundary().unwrap().local_cursor;
        store.connection.write().execute("INSERT INTO current_value_retirements VALUES('agent/zz-history','harness.usage',0,?1)",[cutoff]).unwrap();
        let next = store
            .maintain_current_values(&BTreeSet::from(["owner".into()]), "")
            .unwrap();
        assert_eq!(next.history_visited, 1);
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT count(*) FROM local_observations WHERE subject='agent/zz-history'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn cap_refusal_wakes_collection_and_recovers_retired_keys_sorting_last() {
        let source = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "owner");
        // Live workspace keys consume two pages; retired seat keys sort afterwards.
        let intent = crate::graph::parse_intent(
            "version 2\nagent \"example/aaa\" { workspace \"/tmp\"; argv \"fixture\"; }\nagent \"example/cedar\" { workspace \"/tmp\"; argv \"fixture\"; }",
            "owner",
        )
        .unwrap();
        peer.apply_internal(&intent, "live-prefix").unwrap();
        peer.connection
            .write()
            .execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<128)
            INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            SELECT 0,'agent/example/aaa','workspace.observed','{}',1 FROM n",
                [],
            )
            .unwrap();
        peer.connection.write().execute("INSERT INTO latest_values
            SELECT subject,kind,printf('%03d',id),'owner',1,'live/'||id,id,0,NULL,'{}' FROM local_observations",[]).unwrap();
        peer.connection
            .write()
            .execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000)
            INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            SELECT 0,'agent/zzz-retired/'||i,'harness.observed','{}',1 FROM n",
                [],
            )
            .unwrap();
        peer.connection
            .write()
            .execute(
                "INSERT INTO latest_values
            SELECT subject,kind,'','owner',1,'retired/'||id,id,0,NULL,'{}'
            FROM local_observations WHERE kind='harness.observed'",
                [],
            )
            .unwrap();
        let hosts = BTreeSet::from(["owner".into(), "peer".into()]);
        let record = source.append_claim(&state("idle", "one", 10)).unwrap();
        assert_eq!(
            peer.receive_current_value_for_hosts(&record, &hosts)
                .unwrap_err()
                .code,
            "current-value-capacity"
        );
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            peer.current_value_maintenance_wake().notified(),
        )
        .await
        .unwrap();
        let kind = peer.take_current_capacity_kind().unwrap();
        assert_eq!(kind, "harness.observed");
        let batch = peer
            .maintain_current_values_of_kind(&hosts, "", Some(&kind))
            .unwrap();
        assert_eq!(
            batch.removed, 64,
            "a refused kind must skip unrelated live pages"
        );
        assert_eq!(
            peer.readers
                .get()
                .query_row(
                    "SELECT count(*) FROM latest_values WHERE kind='workspace.observed'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            128
        );
        assert!(
            peer.receive_current_value_for_hosts(&record, &hosts)
                .unwrap()
                .0
        );
    }

    #[test]
    fn collected_tombstones_and_later_timeline_rows_share_one_clock() {
        let store = Store::open_memory("owner").unwrap();
        for n in 0..64 {
            let mut input = state("idle", "one", 1);
            input.subject = format!("agent/retired/{n}");
            append(&store.graph, &input, 1, None).unwrap();
        }
        let before = store.current_observation_boundary().unwrap();
        let batch = store
            .maintain_current_values(&BTreeSet::from(["owner".into()]), "")
            .unwrap();
        assert_eq!(batch.removed, 64);
        let collected = store.current_observation_boundary().unwrap();
        assert_eq!(collected.local_cursor, before.local_cursor + 1);
        assert!(collected.requires_resync(&before.epoch, before.local_cursor));
        let mut frontier =
            roster_local_frontier(&store.readers.get(), store.index().unwrap()).unwrap();
        assert_eq!(frontier, collected.local_cursor);
        for n in 0..3 {
            store.connection.write().execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                VALUES(?1,'agent/live','harness.timeline','{}',?2)",params![store.index().unwrap(),n]).unwrap();
            let next = roster_local_frontier(&store.readers.get(), store.index().unwrap()).unwrap();
            assert_eq!(
                next,
                frontier + 1,
                "each timeline event must invalidate the roster immediately"
            );
            frontier = next;
        }
    }

    #[test]
    fn stale_heartbeat_recovery_advances_semantic_clock_and_revisions() {
        let store = Store::open_memory("owner").unwrap();
        let now = now_ms() as u64;
        append(
            &store.graph,
            &state("working", "one", now - 90_001),
            u128::from(now - 90_001),
            None,
        )
        .unwrap();
        let frontier = store.current_cache_revision().unwrap();
        let aggregate = store.graph.runtime.current_observation_revision("");
        let kind = store
            .graph
            .runtime
            .current_observation_revision("harness.observed");
        let fresh = append(
            &store.graph,
            &state("working", "one", now),
            u128::from(now),
            None,
        )
        .unwrap()
        .0;
        assert_eq!(fresh.body["_semantic_transition"], true);
        assert!(store.current_cache_revision().unwrap() > frontier);
        assert!(store.graph.runtime.current_observation_revision("") > aggregate);
        assert!(
            store
                .graph
                .runtime
                .current_observation_revision("harness.observed")
                > kind
        );
        assert_eq!(
            store.changed_current_agents(frontier).unwrap(),
            BTreeSet::from(["agent/example/cedar".into()])
        );
    }

    #[test]
    fn heartbeat_restamps_refresh_source_without_semantic_revision_or_history_growth() {
        let store = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "owner");
        let first = append(&store.graph, &state("working", "one", 10), 10, None)
            .unwrap()
            .0;
        peer.receive_current_value(&first).unwrap();
        let before = store
            .graph
            .runtime
            .current_observation_revision("harness.observed");
        let imported = peer
            .graph
            .runtime
            .current_observation_revision("harness.observed");
        let aggregate = store.graph.runtime.current_observation_revision("");
        let frontier = store.current_cache_revision().unwrap();
        let roster_frontier =
            roster_local_frontier(&store.readers.get(), store.index().unwrap()).unwrap();
        let second = append(&store.graph, &state("working", "one", 20), 20, None)
            .unwrap()
            .0;
        assert_ne!(second.id, first.id);
        assert_eq!(second.body["fields"]["observed_at_ms"], 20);
        assert_eq!(second.body["fields"]["observed_since_ms"], 10);
        assert_eq!(
            store
                .graph
                .runtime
                .current_observation_revision("harness.observed"),
            before
        );
        assert_eq!(
            store.graph.runtime.current_observation_revision(""),
            aggregate
        );
        assert_eq!(store.current_cache_revision().unwrap(), frontier);
        assert_eq!(
            roster_local_frontier(&store.readers.get(), store.index().unwrap()).unwrap(),
            roster_frontier
        );
        assert!(store.changed_current_agents(frontier).unwrap().is_empty());
        assert!(peer.receive_current_value(&second).unwrap());
        assert_eq!(
            peer.graph
                .runtime
                .current_observation_revision("harness.observed"),
            imported
        );
        let third = append(&store.graph, &state("idle", "one", 30), 30, None)
            .unwrap()
            .0;
        assert!(
            store
                .graph
                .runtime
                .current_observation_revision("harness.observed")
                > before
        );
        assert!(peer.receive_current_value(&third).unwrap());
        assert!(
            peer.graph
                .runtime
                .current_observation_revision("harness.observed")
                > imported
        );
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT count(*) FROM local_observations
            WHERE subject='agent/example/cedar' AND kind='harness.observed'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn fleet_registers_require_a_declared_host_even_without_running_evidence_and_have_a_cap() {
        let source = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        let record = source.append_claim(&state("idle", "one", 10)).unwrap();
        assert_eq!(
            peer.receive_current_value(&record).unwrap_err().code,
            "stale-harness-event-session"
        );
        declare(&peer, "foreign");
        assert!(peer.receive_current_value(&record).is_err());
        // Restore the declaration without fabricating runtime.running.
        peer.connection
            .write()
            .execute(
                "UPDATE desired SET member=json_set(member,'$.host','owner')
            WHERE subject='agent/example/cedar'",
                [],
            )
            .unwrap();
        assert!(peer.receive_current_value(&record).unwrap());
        let mut undeclared = record.clone();
        undeclared.subject = "agent/undeclared".into();
        assert!(peer.receive_current_value(&undeclared).is_err());
        peer.connection
            .write()
            .execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000)
            INSERT INTO latest_values SELECT 'host/cap-'||i,'transport.observed','owner','owner',1,
            'cap/'||i,1,0,NULL,'{}' FROM n",
                [],
            )
            .unwrap();
        let mut transport = record.clone();
        transport.subject = "host/new-peer".into();
        transport.kind = "transport.observed".into();
        transport.body = json!({"fields":{"status":"up"}});
        assert_eq!(
            peer.receive_current_value_for_hosts(
                &transport,
                &BTreeSet::from(["owner".into(), "peer".into(), "new-peer".into()])
            )
            .unwrap_err()
            .code,
            "current-value-capacity"
        );
        let next = source.append_claim(&state("working", "one", 20)).unwrap();
        assert!(
            peer.receive_current_value(&next).unwrap(),
            "the cap never blocks existing-key replacement"
        );
    }

    #[test]
    #[ignore = "whole-process SQLite counters: run this cost probe alone"]
    fn native_register_writer_cost() {
        use smallclaims::sqlite::work;
        for legacy in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(&root.path().join("cost.sqlite"), "owner").unwrap();
            let mut runtime = state("idle", "one", 1);
            runtime.kind = "runtime.observed".into();
            runtime.fields = serde_json::from_value(
                json!({"status":"running", "host":"owner", "incarnation_id":"one"}),
            )
            .unwrap();
            store.append_claim(&runtime).unwrap();
            let publish = |activity, at| {
                let input = state(activity, "one", at);
                if legacy {
                    append_legacy_graph_observation_fenced(
                        &store.graph,
                        &input,
                        u128::from(at),
                        Some("one"),
                    )
                    .unwrap();
                } else {
                    append(&store.graph, &input, u128::from(at), Some("one")).unwrap();
                }
            };
            publish("idle", 1);
            for (name, transition) in [("heartbeat", false), ("transition", true)] {
                for sample in 0..5 {
                    let activity = if transition && sample % 2 == 0 {
                        "working"
                    } else {
                        "idle"
                    };
                    CURRENT_TRANSACTION_ELAPSED.with(|elapsed| {
                        elapsed.set((!legacy).then_some(std::time::Duration::ZERO))
                    });
                    let before = work::total();
                    let started = std::time::Instant::now();
                    publish(activity, 10 + sample + if transition { 100 } else { 0 });
                    let elapsed = started.elapsed();
                    let cost = work::total() - before;
                    let transaction_us = CURRENT_TRANSACTION_ELAPSED
                        .with(|elapsed| elapsed.take().map(|elapsed| elapsed.as_micros()));
                    println!(
                        "writer-cost legacy={legacy} kind={name} sample={sample} transaction_us={transaction_us:?} elapsed_us={} statements={} vm_steps={} fullscan_steps={} sorts={} autoindex_rows={}",
                        elapsed.as_micros(),
                        cost.statements,
                        cost.vm_steps,
                        cost.fullscan_steps,
                        cost.sorts,
                        cost.autoindex_rows
                    );
                    assert_eq!(cost.fullscan_steps, 0);
                    assert!(cost.vm_steps > 0);
                }
            }
        }
    }

    #[test]
    fn a_retained_workspace_cannot_relabel_a_runtime_after_handoff() {
        let store = Store::open_memory("amber").unwrap();
        let subject = "agent/example/worker";
        let claim = |kind: &str, fields: Value| ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        };
        store
            .append_claim(&claim(
                "runtime.observed",
                json!({"status":"running", "host":"amber", "incarnation_id":"one"}),
            ))
            .unwrap();
        store
            .append_claim(&claim(
                "workspace.observed",
                json!({"host":"amber", "workspace":"/tmp/amber"}),
            ))
            .unwrap();
        assert_eq!(
            store.latest_actual_value(subject).unwrap().unwrap()["workspace"],
            "/tmp/amber"
        );
        store
            .append_claim(&claim(
                "runtime.observed",
                json!({"status":"running", "host":"cobalt", "incarnation_id":"two"}),
            ))
            .unwrap();
        let actual = store.latest_actual_value(subject).unwrap().unwrap();
        assert_eq!(actual["host"], "cobalt");
        assert_eq!(actual["incarnation_id"], "two");
        assert!(
            actual.get("workspace").is_none(),
            "a predecessor's workspace is not the destination's: {actual}"
        );
        // A fresh value from the destination remains visible.
        store
            .append_claim(&claim(
                "workspace.observed",
                json!({"host":"cobalt", "workspace":"/tmp/cobalt"}),
            ))
            .unwrap();
        assert_eq!(
            store.latest_actual_value(subject).unwrap().unwrap()["workspace"],
            "/tmp/cobalt"
        );
    }

    #[test]
    fn permission_blocking_and_explicit_clears_start_distinct_status_episodes() {
        let store = Store::open_memory("owner").unwrap();
        let publish = |at, blocked_on: Value, ask: Value| {
            let mut input = state("working", "one", at);
            input.fields.insert("observed_since_ms".into(), json!(10));
            input.fields.insert("blocked_on".into(), blocked_on);
            input.fields.insert("ask".into(), ask);
            append(&store.graph, &input, u128::from(at), None)
                .unwrap()
                .0
                .body
        };
        let running = publish(10, Value::Null, Value::Null);
        assert_eq!(running["fields"]["observed_since_ms"], 10);
        let permission = publish(20, json!("human"), json!("permission"));
        assert_eq!(permission["fields"]["observed_since_ms"], 20);
        assert_eq!(permission["fields"]["status_transition"], true);
        let heartbeat = publish(30, json!("human"), json!("permission"));
        assert_eq!(heartbeat["fields"]["observed_since_ms"], 20);
        assert_eq!(heartbeat["fields"]["status_transition"], false);
        let resumed = publish(40, Value::Null, Value::Null);
        assert_eq!(resumed["fields"]["observed_since_ms"], 40);
        assert_eq!(resumed["fields"]["status_transition"], true);
        let question = publish(50, json!("human"), json!("question"));
        assert_eq!(question["fields"]["observed_since_ms"], 40);
        assert_eq!(question["fields"]["status_transition"], false);
    }

    #[test]
    fn sparse_permission_updates_carry_axes_only_within_the_same_incarnation() {
        let store = Store::open_memory("owner").unwrap();
        let mut blocked = state("working", "one", 10);
        blocked.fields.insert("blocked_on".into(), json!("human"));
        blocked.fields.insert("ask".into(), json!("permission"));
        append(&store.graph, &blocked, 10, None).unwrap();
        let heartbeat = append(&store.graph, &state("working", "one", 20), 20, None)
            .unwrap()
            .0;
        assert_eq!(heartbeat.body["fields"]["ask"], "permission");
        assert_eq!(heartbeat.body["fields"]["observed_since_ms"], 10);
        assert_eq!(heartbeat.body["fields"]["status_transition"], false);
        let relaunched = append(&store.graph, &state("working", "two", 30), 30, None)
            .unwrap()
            .0;
        assert!(relaunched.body["fields"].get("ask").is_none());
        assert_eq!(relaunched.body["fields"]["observed_since_ms"], 30);
        assert_eq!(relaunched.body["fields"]["status_transition"], true);
    }

    #[test]
    fn terminal_snapshots_do_not_revive_permission_on_a_sparse_successor() {
        for terminal in ["ended", "indeterminate"] {
            let store = Store::open_memory("owner").unwrap();
            let mut blocked = state("working", "one", 10);
            blocked.fields.insert("blocked_on".into(), json!("human"));
            blocked.fields.insert("ask".into(), json!("permission"));
            append(&store.graph, &blocked, 10, None).unwrap();
            let stopped = append(&store.graph, &state(terminal, "one", 20), 20, None)
                .unwrap()
                .0;
            assert!(!permission_blocked(&stopped.body["fields"]));
            let resumed = append(&store.graph, &state("working", "one", 30), 30, None)
                .unwrap()
                .0;
            assert!(!permission_blocked(&resumed.body["fields"]));
            assert!(resumed.body["fields"]["ask"].is_null());
        }
    }

    #[test]
    fn a_late_reduction_cannot_revalidate_an_evicted_current_status() {
        let store = Store::open_memory("owner").unwrap();
        let mut runtime = state("idle", "one", 1);
        runtime.kind = "runtime.observed".into();
        runtime.fields =
            serde_json::from_value(json!({"status":"running","incarnation_id":"one"})).unwrap();
        store.append_claim(&runtime).unwrap();
        store.append_claim(&state("idle", "one", 10)).unwrap();
        let index = store.index().unwrap();
        let old_revision = subject_current_revision(&store.readers.get(), "agent/example/cedar").unwrap();
        let old = store
            .status(Some("agent/example/cedar"))
            .unwrap()
            .subjects
            .remove(0);
        store.append_claim(&state("working", "one", 20)).unwrap();
        store.refresh_current_caches().unwrap();
        // A reader pinned before the write finishes after the newer reader evicts its entry.
        store.keep_subject_status(
            "agent/example/cedar",
            index,
            SubjectStatusMode::Full,
            old_revision,
            &old,
            &None,
        );
        let revision = subject_current_revision(&store.readers.get(), "agent/example/cedar").unwrap();
        assert!(
            store
                .kept_subject_status("agent/example/cedar", index, SubjectStatusMode::Full, revision)
                .is_none()
        );
        let current = store.status(Some("agent/example/cedar")).unwrap();
        assert_eq!(
            current.subjects[0].harness.as_ref().unwrap().state,
            "working"
        );
    }

    #[test]
    fn current_register_batches_respect_the_roster_fold_bound() {
        let store = Store::open_memory("owner").unwrap();
        for name in ["cedar", "birch", "elm"] {
            let mut input = state("idle", "one", 1);
            input.subject = format!("agent/{name}");
            store.append_legacy_claim(&input).unwrap();
        }
        let index = store.index().unwrap();
        let items = ["cedar", "birch", "elm"]
            .map(|name| json!({"id":format!("agent/{name}"),"name":name,"state":"idle"}));
        store
            .cached_agent_resources(index, true, |_| Ok(items.to_vec()))
            .unwrap();
        for name in ["cedar", "birch", "elm"] {
            let mut input = state("working", "one", 10);
            input.subject = format!("agent/{name}");
            store.append_claim(&input).unwrap();
        }
        assert_eq!(store.index().unwrap(), index);
        assert_eq!(
            store
                .agent_roster_unbounded_because(index, true, 2)
                .unwrap(),
            Some("cards changed".to_owned())
        );
        assert_eq!(
            store
                .agent_roster_unbounded_because(index, true, 3)
                .unwrap(),
            None
        );
        let selected = BTreeSet::from(["agent/example/cedar".to_owned(), "agent/birch".to_owned()]);
        store
            .cached_agent_resources_chunk(index, true, &selected, |changed| {
                let (names, _) = changed.unwrap();
                assert_eq!(names, &selected);
                Ok(items[..2].to_vec())
            })
            .unwrap();
        assert_eq!(
            store
                .agent_roster_unbounded_because(index, true, 1)
                .unwrap(),
            None,
            "only the unrefreshed card remains after the bounded chunk"
        );
    }

    #[test]
    fn a_register_change_refolds_only_its_card_at_the_same_graph_cut() {
        let store = Store::open_memory("owner").unwrap();
        let index = store.index().unwrap();
        let items = vec![
            json!({"id":"agent/example/cedar","name":"cedar","state":"idle"}),
            json!({"id":"agent/birch","name":"birch","state":"idle"}),
        ];
        store
            .cached_agent_resources(index, false, |changed| {
                assert!(changed.is_none());
                Ok(items.clone())
            })
            .unwrap();
        store.append_claim(&state("working", "one", 10)).unwrap();
        assert_eq!(store.index().unwrap(), index);
        let next = store
            .cached_agent_resources(index, false, |changed| {
                let (names, _) = changed.unwrap();
                assert_eq!(names, &BTreeSet::from(["agent/example/cedar".to_owned()]));
                Ok(vec![
                    json!({"id":"agent/example/cedar","name":"cedar","state":"working"}),
                ])
            })
            .unwrap();
        assert_eq!(next.len(), 2);
        assert_eq!(
            next.iter()
                .find(|item| item["id"] == "agent/birch")
                .unwrap(),
            &items[1]
        );
        store
            .cached_agent_resources(index, false, |_| {
                panic!("warm read must reuse the updated roster")
            })
            .unwrap();
    }

    #[test]
    fn unknown_auth_null_is_not_a_transition_or_an_auth_restoration() {
        let store = Store::open_memory("owner").unwrap();
        append(&store.graph, &state("working", "one", 10), 10, None).unwrap();
        let publish = |at, auth| {
            let mut input = state("working", "one", at);
            input.fields.insert("provider_auth".into(), auth);
            append(&store.graph, &input, u128::from(at), None)
                .unwrap()
                .0
                .body
        };
        let unknown = publish(20, Value::Null);
        assert_eq!(unknown["fields"]["status_transition"], false);
        assert_eq!(unknown["fields"]["observed_since_ms"], 10);
        let expired = publish(30, json!(false));
        assert_eq!(expired["fields"]["status_transition"], true);
        let still_expired = publish(40, Value::Null);
        assert_eq!(still_expired["fields"]["provider_auth"], false);
        assert_eq!(still_expired["fields"]["status_transition"], false);
        assert_eq!(still_expired["fields"]["observed_since_ms"], 30);
        let restored = publish(50, json!(true));
        assert_eq!(restored["fields"]["status_transition"], true);
        assert_eq!(restored["fields"]["observed_since_ms"], 50);
    }

    #[test]
    fn retired_current_transitions_require_resync_and_reopen_keeps_the_boundary() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("current.sqlite3");
        let store = Store::open(&path, "node").unwrap();
        let initial = store.current_observation_boundary().unwrap();
        assert!(!initial.requires_resync(&initial.epoch, 0));
        store.append_claim(&state("working", "one", 1)).unwrap();
        let first = store.current_observation_boundary().unwrap();
        store.append_claim(&state("idle", "one", 2)).unwrap();
        let second = store.current_observation_boundary().unwrap();
        assert!(second.requires_resync(&initial.epoch, 0));
        assert!(!second.requires_resync(&first.epoch, first.local_cursor));
        store.append_claim(&state("working", "one", 3)).unwrap();
        let third = store.current_observation_boundary().unwrap();
        assert!(third.requires_resync(&first.epoch, first.local_cursor));
        assert!(!third.requires_resync(&third.epoch, second.local_cursor));
        assert!(third.requires_resync("older-database", third.local_cursor));
        assert!(third.requires_resync(&third.epoch, third.local_cursor + 1));
        assert_eq!(store.local_observations_after(0, 10).unwrap().len(), 1);
        drop(store);
        let reopened = Store::open(&path, "node").unwrap();
        assert_eq!(reopened.current_observation_boundary().unwrap(), third);
    }

    #[test]
    fn a_current_seat_lookup_seeks_its_register_without_scanning_the_fleet() {
        let store = Store::open_memory("node").unwrap();
        store
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        let connection = store.readers.get();
        let query = harness_sql(
            &connection,
            "agent/example/cedar",
            &format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query("claims.body", "harness.observed")
            ),
        )
        .unwrap();
        let plan: Vec<String> = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(params!["agent/example/cedar", i64::MAX], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH latest_values") && step.contains("subject=?")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(
                |step| step.contains("SCAN latest_values") || step.contains("SCAN main.claims")
            ),
            "{plan:?}"
        );
    }

    #[test]
    fn current_values_replace_without_graph_history_and_fold_into_status() {
        let store = Store::open_memory("node").unwrap();
        let mut runtime = state("idle", "one", now_ms() as u64);
        runtime.kind = "runtime.observed".into();
        runtime.fields = serde_json::from_value(
            json!({"status":"running","runtime_id":"native","incarnation_id":"one"}),
        )
        .unwrap();
        store.append_claim(&runtime).unwrap();
        let inventory = store.replication_inventory().unwrap();
        let stamp = now_ms() as u64;
        for i in 0..100 {
            store
                .append_claim(&state(
                    if i % 2 == 0 { "idle" } else { "working" },
                    "one",
                    stamp + i,
                ))
                .unwrap();
        }
        let connection = store.readers.get();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM latest_values", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM local_observations", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert!(
            store
                .claims_for("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.replication_inventory().unwrap().digest,
            inventory.digest
        );
        let current = store.current_harness("agent/example/cedar").unwrap().unwrap();
        assert_eq!(current.state, "working");
        assert_eq!(current.incarnation_id, "one");
        assert!(current.observed_at_unix_ms >= u128::from(stamp));
        assert!(current.observed_at_unix_ms <= now_ms());
    }

    #[test]
    fn context_replaces_across_incarnations_while_numeric_usage_stays_durable() {
        let store = Store::open_memory("owner").unwrap();
        for incarnation in ["one", "two"] {
            store
                .append_claim(&ClaimInput {
                    subject: "agent/example/cedar".into(),
                    kind: "harness.usage".into(),
                    actor: Some("agent/example/cedar".into()),
                    fields: serde_json::from_value(
                        json!({"driver":"codex","incarnation_id":incarnation,
                    "semantics":"context_occupancy","total_tokens":20}),
                    )
                    .unwrap(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let numeric = ClaimInput { subject:"agent/example/cedar".into(),kind:"harness.usage".into(),actor:Some("agent/example/cedar".into()),
            fields:serde_json::from_value(json!({"driver":"codex","incarnation_id":"two","semantics":"session_cumulative","total_tokens":40})).unwrap(),
            evidence:Vec::new(),expected_subject:None,idempotency_key:None };
        store.append_claim(&numeric).unwrap();
        let connection = store.readers.get();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM latest_values WHERE kind='harness.usage'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM claims WHERE kind='harness.usage'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            1
        );
        let current: String = connection
            .query_row(
                "SELECT body FROM latest_values WHERE kind='harness.usage'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&current).unwrap()["fields"]["incarnation_id"],
            "two"
        );
    }

    #[test]
    fn mixed_build_newer_legacy_status_remains_visible() {
        let store = Store::open_memory("owner").unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/example/cedar".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: serde_json::from_value(
                    json!({"status":"running","runtime_id":"cedar","incarnation_id":"one"}),
                )
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let current = append(&store.graph, &state("idle", "one", 1_000), 1_000, None)
            .unwrap()
            .0;
        let legacy = store
            .append_legacy_claim(&state("working", "one", now_ms() as u64))
            .unwrap();
        assert!(legacy.accepted_at_unix_ms > current.accepted_at_unix_ms);
        assert_eq!(
            store
                .latest_claim("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .id,
            legacy.id
        );
        assert_eq!(
            store.current_harness("agent/example/cedar").unwrap().unwrap().state,
            "working"
        );
    }

    #[test]
    fn fleet_current_values_require_the_running_owner_and_incarnation() {
        let owner = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "peer");
        peer.append_claim(&ClaimInput {
            subject: "agent/example/cedar".into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"status":"running","runtime_id":"cedar","incarnation_id":"one"}),
            )
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
        let report = owner
            .append_claim(&state("working", "one", now_ms() as u64))
            .unwrap();
        assert_eq!(
            peer.receive_current_value(&report).unwrap_err().code,
            "stale-harness-event-session"
        );
        let mut report = report;
        report.origin = "peer".into();
        report.body["fields"]["incarnation_id"] = "older".into();
        assert_eq!(
            peer.receive_current_value(&report).unwrap_err().code,
            "stale-harness-event-session"
        );
        report.body["fields"]["incarnation_id"] = "one".into();
        assert!(peer.receive_current_value(&report).unwrap());
    }

    #[test]
    fn database_reset_starts_a_new_generation_without_accepting_old_replays() {
        let old = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "owner");
        let stamp = now_ms() as u64;
        let mut latest = old.append_claim(&state("working", "one", stamp)).unwrap();
        for n in 1..20 {
            latest = old
                .append_claim(&state("working", "one", stamp + n))
                .unwrap();
        }
        assert!(peer.receive_current_value(&latest).unwrap());
        let reset = Store::open_memory("owner").unwrap();
        let fresh = reset
            .append_claim(&state("idle", "one", stamp + 21))
            .unwrap();
        assert!(
            local_observation_position(&fresh).unwrap()
                < local_observation_position(&latest).unwrap()
        );
        assert!(peer.receive_current_value(&fresh).unwrap());
        assert!(!peer.receive_current_value(&latest).unwrap());
        assert_eq!(
            peer.latest_claim("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["state"],
            "idle"
        );
    }

    #[test]
    fn workspace_registers_bind_to_the_running_host_without_a_native_incarnation() {
        let source = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("owner").unwrap();
        declare(&peer, "owner");
        let mut runtime = state("idle", "one", 1);
        runtime.kind = "runtime.observed".into();
        runtime.fields =
            serde_json::from_value(json!({"status":"running","incarnation_id":"one"})).unwrap();
        peer.append_claim(&runtime).unwrap();
        let mut workspace = runtime;
        workspace.kind = "workspace.observed".into();
        workspace.fields =
            serde_json::from_value(json!({"host":"owner","workspace":"/work/cedar"})).unwrap();
        let record = source.append_claim(&workspace).unwrap();
        assert!(peer.receive_current_value(&record).unwrap());
        let foreign = Store::open_memory("foreign")
            .unwrap()
            .append_claim(&workspace)
            .unwrap();
        assert_eq!(
            peer.receive_current_value(&foreign).unwrap_err().code,
            "stale-harness-event-session"
        );
    }

    #[test]
    fn a_future_sender_clock_cannot_pin_a_current_register() {
        let source = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "owner");
        let record = source.append_claim(&state("working", "one", 10)).unwrap();
        let mut future = record.clone();
        future.accepted_at_unix_ms = now_ms() + 3_600_000;
        assert_eq!(
            peer.receive_current_value(&future).unwrap_err().code,
            "invalid-current-value"
        );
        assert!(peer.receive_current_value(&record).unwrap());
    }

    #[test]
    fn restore_recovery_requires_a_new_durably_bound_incarnation() {
        let source = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("owner").unwrap();
        declare(&peer, "owner");
        let runtime = |incarnation| ClaimInput {
            subject: "agent/example/cedar".into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"status":"running","runtime_id":"cedar","incarnation_id":incarnation}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        };
        peer.append_claim(&runtime("one")).unwrap();
        let stamp = now_ms() as u64;
        let mut old = source
            .append_claim(&state("working", "one", stamp))
            .unwrap();
        for n in 1..20 {
            old = source
                .append_claim(&state("working", "one", stamp + n))
                .unwrap();
        }
        assert!(peer.receive_current_value(&old).unwrap());
        let restored = Store::open_memory("owner").unwrap();
        restored
            .connection
            .write()
            .execute(
                "UPDATE meta SET value=?1 WHERE key='current-value-epoch'",
                [old.body["_source_epoch"].as_str().unwrap()],
            )
            .unwrap();
        let fresh = restored
            .append_claim(&state("idle", "two", stamp + 21))
            .unwrap();
        assert!(
            peer.receive_current_value(&fresh).is_err(),
            "a self-declared new incarnation is insufficient"
        );
        peer.append_claim(&runtime("two")).unwrap();
        assert!(peer.receive_current_value(&fresh).unwrap());
        assert!(peer.receive_current_value(&old).is_err());
        assert_eq!(
            peer.latest_claim("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["incarnation_id"],
            "two"
        );
    }

    #[test]
    fn workspace_and_transport_restores_need_a_new_epoch_when_the_counter_rewinds() {
        for kind in ["workspace.observed", "transport.observed"] {
            let source = Store::open_memory("owner").unwrap();
            let peer = Store::open_memory("peer").unwrap();
            declare(&peer, "owner");
            let mut input = state("idle", "one", 1);
            input.kind = kind.into();
            if kind == "workspace.observed" {
                input.fields =
                    serde_json::from_value(json!({"host":"owner","workspace":"/work/cedar"}))
                        .unwrap();
            } else {
                input.subject = "host/peer".into();
                input.actor = None;
                input.fields =
                    serde_json::from_value(json!({"status":"up","last_success_at":1})).unwrap();
            }
            let mut old = source.append_claim(&input).unwrap();
            for _ in 0..20 {
                old = source.append_claim(&input).unwrap();
            }
            assert!(peer.receive_current_value(&old).unwrap());
            let restored = Store::open_memory("owner").unwrap();
            restored
                .connection
                .write()
                .execute(
                    "UPDATE meta SET value=?1 WHERE key='current-value-epoch'",
                    [old.body["_source_epoch"].as_str().unwrap()],
                )
                .unwrap();
            let replay = restored.append_claim(&input).unwrap();
            assert!(
                !peer.receive_current_value(&replay).unwrap(),
                "{kind}: reopening the old epoch cannot erase ordering"
            );
            restored
                .connection
                .write()
                .execute("DELETE FROM meta WHERE key='current-value-epoch'", [])
                .unwrap();
            initialize_epoch(&restored.connection.write()).unwrap();
            let fresh = restored.append_claim(&input).unwrap();
            assert!(
                peer.receive_current_value(&fresh).unwrap(),
                "{kind}: the explicit restore epoch cuts over"
            );
            assert!(!peer.receive_current_value(&old).unwrap());
        }
    }

    #[test]
    fn peer_refusals_remain_durable_when_the_writer_is_contended() {
        let root = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(Store::open(&root.path().join("peer.sqlite"), "owner").unwrap());
        let mut connection = Connection::open(&store.path).unwrap();
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let other = store.clone();
        let (started, waiting) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            other.record_peer_failure("peer", "refused", "fixture refusal")
        });
        waiting.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        tx.rollback().unwrap();
        assert!(worker.join().unwrap().unwrap());
        let reason: String = store
            .readers
            .get()
            .query_row(
                "SELECT reason FROM replication_refusals WHERE peer='peer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, "fixture refusal");
    }

    #[test]
    fn current_write_deadline_rolls_back_register_source_and_readiness() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("deadline.sqlite");
        let store = Store::open(&path, "node").unwrap();
        let original = store
            .append_claim(&state("starting", "one", now_ms() as u64))
            .unwrap();
        let boundary = store.current_observation_boundary().unwrap();
        let revisions = || {
            std::iter::once("")
                .chain(CURRENT_VALUE_KINDS)
                .map(|kind| store.graph.runtime.current_observation_revision(kind))
                .chain(std::iter::once(store.smalltalk.current_harness_freshness.load(std::sync::atomic::Ordering::Acquire)))
                .collect::<Vec<_>>()
        };
        let before = revisions();
        let mut connection = Connection::open(&path).unwrap();
        connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        let error = current_transaction(&mut connection, None, |tx| {
            tx.execute("UPDATE latest_values SET source_id='timed-out-source' WHERE subject='agent/example/cedar'", [])
                .map_err(internal)?;
            tx.execute("INSERT INTO latest_values SELECT 'agent/pine',kind,slot,origin,source_at,
                'timed-out-new-source',local_id,store_index,actor,body FROM latest_values WHERE subject='agent/example/cedar'", [])
                .map_err(internal)?;
            update_readiness(tx, &state("working", "two", now_ms() as u64))?;
            // The real progress handler interrupts this query at the unchanged 100ms budget.
            tx.query_row("WITH RECURSIVE numbers(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM numbers WHERE n<1000000000)
                SELECT sum(n) FROM numbers", [], |row| row.get::<_, i64>(0)).map_err(internal)
        }).unwrap_err();
        assert_eq!(error.code, "current-value-deadline");
        assert_eq!(
            error.details["sqlite_extended_code"],
            rusqlite::ffi::SQLITE_INTERRUPT
        );
        let (source, incarnation, ready): (String, String, bool) = connection.query_row(
            "SELECT v.source_id,r.incarnation,r.ready FROM latest_values v JOIN latest_readiness r USING(subject)
             WHERE v.subject='agent/example/cedar'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(source, original.id);
        assert_eq!((incarnation.as_str(), ready), ("one", false));
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM latest_values WHERE subject='agent/pine'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(store.current_observation_boundary().unwrap(), boundary);
        assert_eq!(revisions(), before);
        // Clearing this connection's callback permits its next transaction; arbitrary faults
        // and ownership refusals never become a deadline merely because they contain that word.
        current_transaction(&mut connection, None, |tx| {
            tx.execute("INSERT INTO meta VALUES('after-deadline','1')", [])
                .map_err(internal)
        })
        .unwrap();
        for code in ["internal", "stale-harness-event-session"] {
            let error = current_transaction::<()>(&mut connection, None, |_| {
                Err(St3Error::new(code, "interrupted"))
            })
            .unwrap_err();
            assert_eq!(error.code, code);
        }
    }

    #[test]
    fn current_publication_uses_the_remaining_attempt_after_a_managed_writer_hold() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.sqlite"), "owner").unwrap());
        declare(&store, "owner");
        let writer_store = store.clone();
        let (locked, acquired) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut connection = writer_store.connection.write();
            let tx = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            tx.execute(
                "INSERT INTO meta VALUES ('short-current-collision','1')",
                [],
            )
            .unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(60));
            tx.commit().unwrap();
        });
        acquired.recv().unwrap();
        let sample = store
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        writer.join().unwrap();
        assert_eq!(
            store
                .latest_claim(&sample.subject, Some("harness.observed"))
                .unwrap()
                .unwrap()
                .id,
            sample.id
        );
        assert_eq!(store.readers.get().query_row("SELECT count(*) FROM local_observations WHERE subject=?1 AND kind='harness.observed'", [&sample.subject], |r| r.get::<_,u64>(0)).unwrap(), 1);
    }

    #[test]
    fn current_write_deadline_also_rejects_short_sql_after_a_late_closure() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("graph.sqlite"), "owner").unwrap();
        let mut connection = Connection::open(&store.graph.path).unwrap();
        let error = current_transaction(&mut connection, None, |tx| {
            tx.execute("INSERT INTO meta VALUES ('late-current-test','1')", [])
                .map_err(internal)?;
            std::thread::sleep(
                crate::client::LATEST_VALUE_TIMEOUT + std::time::Duration::from_millis(10),
            );
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code, "current-value-deadline");
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM meta WHERE key='late-current-test'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn current_publication_drops_when_writer_admission_exceeds_its_bound() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("graph.sqlite"), "node").unwrap();
        let mut writer = store.connection.write();
        let tx = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO meta VALUES ('test-held-writer','1')", [])
            .unwrap();
        let started = std::time::Instant::now();
        assert!(
            store
                .append_claim(&state("working", "one", now_ms() as u64))
                .is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        tx.rollback().unwrap();
        drop(writer);
        assert!(
            store
                .latest_claim("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .is_none()
        );
        store
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        assert_eq!(
            store
                .latest_claim("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["state"],
            "idle"
        );
    }

    #[test]
    fn fleet_delivery_replaces_the_register_and_refuses_delayed_updates() {
        let owner = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        declare(&peer, "owner");
        let inventory=peer.replication_inventory().unwrap().digest;
        let first = owner
            .append_claim(&state("working", "one", now_ms() as u64))
            .unwrap();
        assert!(peer.receive_current_value(&first).unwrap());
        assert!(peer.harness_was_ready("agent/example/cedar", "one").unwrap());
        let mut second = owner
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        // Source revisions still order correctly when the owner's wall clock moves back.
        second.accepted_at_unix_ms = first.accepted_at_unix_ms.saturating_sub(1);
        assert!(peer.receive_current_value(&second).unwrap());
        assert!(!peer.receive_current_value(&first).unwrap());
        assert!(!peer.receive_current_value(&second).unwrap());
        assert_eq!(
            peer.latest_claim("agent/example/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["state"],
            "idle"
        );
        assert_eq!(peer.replication_inventory().unwrap().digest,inventory);
        assert_eq!(peer.local_observations_after(0, 100).unwrap().len(), 1);
    }
}
