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

/// Discover positive subjects before looking up their declaration. Do not correlate a
/// whole-fleet declaration scan with the history/register UNION. Legacy observations use
/// exactly the current_claims shadow rule; diagnostics remain durable auth evidence and
/// the final harness fold still checks restoration, readiness, incarnation and ownership.
pub(super) const LOGIN_CANDIDATES_SQL: &str = "
WITH candidates AS (
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
    SELECT subject FROM main.claims INDEXED BY claims_harness_login_candidate_index
    WHERE ((kind='harness.observed' AND (
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
                AND v.source_at>=CAST(claims.accepted_at_unix_ms AS INTEGER)))
)
SELECT desired.subject, desired.kind, desired.body, desired.member,
       desired.owner_run, desired.owner_generation, desired.owner_step
FROM candidates CROSS JOIN desired
WHERE desired.subject=candidates.subject AND desired.kind='agent' ORDER BY desired.subject";

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

pub fn is_current_value(kind: &str) -> bool {
    matches!(
        kind,
        "harness.observed"
            | "harness.usage"
            | "harness.todo.observed"
            | "workspace.observed"
            | "transport.observed"
    )
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

pub(super) fn append(
    graph: &GraphStore,
    input: &ClaimInput,
    now: u128,
    event_runtime: Option<&str>,
) -> Result<(ClaimRecord, bool), St3Error> {
    validate_local_observation(input)?;
    // This connection has no queue and never waits for SQLite's writer. Busy means dropped;
    // the producer moves on and only a subsequent observation can replace this value.
    let mut connection = Connection::open_with_flags(
        &graph.path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(internal)?;
    smallclaims::sqlite::observe(&mut connection);
    connection
        .busy_timeout(std::time::Duration::ZERO)
        .map_err(internal)?;
    let deadline = std::time::Instant::now() + crate::client::LATEST_VALUE_TIMEOUT;
    connection.progress_handler(100, Some(move || std::time::Instant::now() >= deadline));
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(internal)?;
    // A bound driver announces starting before reconciliation records runtime.running.
    // This hint only permits mailbox startup to wait; it grants no delivery or ready authority.
    if input.fields.get("state").and_then(Value::as_str) != Some("starting") {
        check_harness_event_runtime(&tx, &input.subject, event_runtime)?;
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
    let previous: Option<(i64, String)> = tx
        .query_row(
            "SELECT local_id,body FROM latest_values WHERE subject=?1 AND kind=?2 AND slot=?3",
            params![input.subject, input.kind, slot],
            |r| Ok((r.get(0)?, r.get(1)?)),
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
        if let Some((_, body)) = &previous {
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
                    return Ok((claim_by_id_tx(&tx, &id).map_err(internal)?.unwrap(), false));
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
    let (mut local, _) = insert_local_observation_tx(&tx, &graph.origin, &input, now)?;
    let epoch: String = tx
        .query_row(
            "SELECT value FROM meta WHERE key='current-value-epoch'",
            [],
            |row| row.get(0),
        )
        .map_err(internal)?;
    local.body["_source_epoch"] = json!(epoch);
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
    if input.kind == "harness.usage" {
        tx.execute(
            "DELETE FROM local_latest_slots WHERE subject=?1 AND kind='harness.usage'
            AND json_extract(published_fields,'$.semantics')='context_occupancy'",
            [&input.subject],
        )
        .map_err(internal)?;
    } else {
        tx.execute(
            "DELETE FROM local_latest_slots WHERE subject=?1 AND kind=?2",
            params![input.subject, input.kind],
        )
        .map_err(internal)?;
    }
    let retired: Option<i64> = tx
        .query_row(
            "SELECT MAX(id) FROM local_observations WHERE subject=?1 AND kind=?2 AND id!=?3
         AND (?2!='harness.usage' OR json_extract(body,'$.fields.semantics')='context_occupancy')",
            params![input.subject, input.kind, sequence],
            |row| row.get(0),
        )
        .map_err(internal)?;
    if let Some(retired) = retired {
        retire_feed_through(&tx, retired)?;
    }
    tx.execute(
        "DELETE FROM local_observations WHERE subject=?1 AND kind=?2 AND id!=?3
        AND (?2!='harness.usage' OR json_extract(body,'$.fields.semantics')='context_occupancy')",
        params![
            input.subject,
            input.kind,
            local_observation_position(&local)
        ],
    )
    .map_err(internal)?;
    if let Some((old_id, _)) = previous {
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
    update_readiness(&tx, &input)?;
    tx.commit().map_err(internal)?;
    Ok((local, true))
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
    /// The explicit reconnect boundary for consumers of current-observation transitions.
    /// Read this alongside the feed/snapshot in a pinned read transaction. A cursor behind
    /// retired evidence must resync from current values rather than promise missing history.
    pub fn current_observation_boundary(&self) -> Result<CurrentObservationBoundary> {
        Ok(self.readers.get().query_row(
            "SELECT (SELECT value FROM meta WHERE key='current-value-epoch'),
                    (SELECT COALESCE(MAX(id),0) FROM local_observations),
                    COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key='current-value-retired-through'),0)",
            [], |row| Ok(CurrentObservationBoundary {
                epoch: row.get(0)?, local_cursor: row.get(1)?,
                retired_through_local_cursor: row.get(2)?,
            }),
        )?)
    }

    /// Local dial failures are replace-in-place hints. They never wait behind graph writes.
    pub fn record_peer_failure(&self, peer: &str, status: &str, error: &str) -> Result<bool> {
        let now = now_ms();
        let last_success = self.replication_peer_last_success(peer)?;
        let observed = self
            .own_transport_value(peer)?
            .filter(|record| record.body["fields"]["status"] == "up")
            .and_then(|record| record.body["fields"]["last_success_at"].as_u64())
            .map(u128::from);
        let recent = last_success
            .or(observed)
            .is_some_and(|at| now.saturating_sub(at) < PEER_UP_MS);
        let status = if status == "down" { "unknown" } else { status };
        let mut connection = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        smallclaims::sqlite::observe(&mut connection);
        connection.busy_timeout(std::time::Duration::ZERO)?;
        let Ok(tx) = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        else {
            return Ok(false);
        };
        if status == "refused" {
            tx.execute("INSERT INTO replication_refusals(peer,reason,updated_at_unix_ms) VALUES(?1,?2,?3)
                ON CONFLICT(peer) DO UPDATE SET reason=excluded.reason,updated_at_unix_ms=excluded.updated_at_unix_ms",
                params![peer,error,now.to_string()])?;
        } else {
            tx.execute("INSERT INTO replication_peers(peer,status,last_error,updated_at_unix_ms) VALUES(?1,?2,?3,?4)
                ON CONFLICT(peer) DO UPDATE SET status=CASE WHEN ?5 THEN replication_peers.status ELSE excluded.status END,
                last_error=excluded.last_error,updated_at_unix_ms=excluded.updated_at_unix_ms",
                params![peer,status,error,now.to_string(),recent])?;
        }
        tx.commit()?;
        Ok(status == "refused" || !recent)
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
        // routes current observations to the same zero-wait register writer, so an
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
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(internal)?;
        let deadline = std::time::Instant::now() + crate::client::LATEST_VALUE_TIMEOUT;
        connection.progress_handler(100, Some(move || std::time::Instant::now() >= deadline));
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(internal)?;
        let mut incarnation_bound = false;
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
        let previous: Option<(u64, String, i64, String, String)> = tx.query_row(
            "SELECT source_at,source_id,local_id,origin,body FROM latest_values WHERE subject=?1 AND kind=?2 AND slot=?3",
            params![record.subject,record.kind,slot], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))
            .optional().map_err(internal)?;
        let source_sequence = |id: &str| {
            id.rsplit('/')
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_default()
        };
        if previous.as_ref().is_some_and(|(at, id, _, origin, body)| {
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
        }) {
            return Ok(false);
        }
        let input = ClaimInput {
            subject: record.subject.clone(),
            kind: record.kind.clone(),
            actor: record.actor.clone(),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        let (local, _) =
            insert_local_observation_tx(&tx, &self.origin, &input, record.accepted_at_unix_ms)?;
        if let Some((_, _, old_id, _, _)) = previous {
            retire_feed_through(&tx, old_id)?;
            tx.execute("DELETE FROM local_observations WHERE id=?1", [old_id])
                .map_err(internal)?;
        }
        tx.execute(
            "INSERT INTO latest_values VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(subject,kind,slot) DO UPDATE SET origin=excluded.origin,source_at=excluded.source_at,
             source_id=excluded.source_id,local_id=excluded.local_id,store_index=excluded.store_index,actor=excluded.actor,body=excluded.body",
            params![record.subject,record.kind,slot,record.origin,record.accepted_at_unix_ms as u64,
                record.id,local_observation_position(&local),local.store_index,record.actor,canonical_json_text(&record.body).map_err(internal)?]
        ).map_err(internal)?;
        update_readiness(&tx, &input)?;
        tx.commit().map_err(internal)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(state: &str, incarnation: &str, at: u64) -> ClaimInput {
        ClaimInput {
            subject: "agent/cedar".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/cedar".into()),
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
        let old_revision = subject_current_revision(&store.readers.get(), "agent/cedar").unwrap();
        let old = store
            .status(Some("agent/cedar"))
            .unwrap()
            .subjects
            .remove(0);
        store.append_claim(&state("working", "one", 20)).unwrap();
        store.refresh_current_caches().unwrap();
        // A reader pinned before the write finishes after the newer reader evicts its entry.
        store.keep_subject_status(
            "agent/cedar",
            index,
            SubjectStatusMode::Full,
            old_revision,
            &old,
            &None,
        );
        let revision = subject_current_revision(&store.readers.get(), "agent/cedar").unwrap();
        assert!(
            store
                .kept_subject_status("agent/cedar", index, SubjectStatusMode::Full, revision)
                .is_none()
        );
        let current = store.status(Some("agent/cedar")).unwrap();
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
        let selected = BTreeSet::from(["agent/cedar".to_owned(), "agent/birch".to_owned()]);
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
            json!({"id":"agent/cedar","name":"cedar","state":"idle"}),
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
                assert_eq!(names, &BTreeSet::from(["agent/cedar".to_owned()]));
                Ok(vec![
                    json!({"id":"agent/cedar","name":"cedar","state":"working"}),
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
            "agent/cedar",
            &format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query("claims.body", "harness.observed")
            ),
        )
        .unwrap();
        let plan: Vec<String> = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(params!["agent/cedar", i64::MAX], |row| row.get(3))
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
                .claims_for("agent/cedar", Some("harness.observed"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.replication_inventory().unwrap().digest,
            inventory.digest
        );
        let current = store.current_harness("agent/cedar").unwrap().unwrap();
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
                    subject: "agent/cedar".into(),
                    kind: "harness.usage".into(),
                    actor: Some("agent/cedar".into()),
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
        let numeric = ClaimInput { subject:"agent/cedar".into(),kind:"harness.usage".into(),actor:Some("agent/cedar".into()),
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
                subject: "agent/cedar".into(),
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
                .latest_claim("agent/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .id,
            legacy.id
        );
        assert_eq!(
            store.current_harness("agent/cedar").unwrap().unwrap().state,
            "working"
        );
    }

    #[test]
    fn fleet_current_values_require_the_running_owner_and_incarnation() {
        let owner = Store::open_memory("owner").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        peer.append_claim(&ClaimInput {
            subject: "agent/cedar".into(),
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
            peer.latest_claim("agent/cedar", Some("harness.observed"))
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
        let runtime = |incarnation| ClaimInput {
            subject: "agent/cedar".into(),
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
            peer.latest_claim("agent/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["incarnation_id"],
            "two"
        );
    }

    #[test]
    fn current_publication_drops_when_sqlite_is_busy_instead_of_queueing() {
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
                .latest_claim("agent/cedar", Some("harness.observed"))
                .unwrap()
                .is_none()
        );
        store
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        assert_eq!(
            store
                .latest_claim("agent/cedar", Some("harness.observed"))
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
        let first = owner
            .append_claim(&state("working", "one", now_ms() as u64))
            .unwrap();
        assert!(peer.receive_current_value(&first).unwrap());
        assert!(peer.harness_was_ready("agent/cedar", "one").unwrap());
        let mut second = owner
            .append_claim(&state("idle", "one", now_ms() as u64))
            .unwrap();
        // Source revisions still order correctly when the owner's wall clock moves back.
        second.accepted_at_unix_ms = first.accepted_at_unix_ms.saturating_sub(1);
        assert!(peer.receive_current_value(&second).unwrap());
        assert!(!peer.receive_current_value(&first).unwrap());
        assert!(!peer.receive_current_value(&second).unwrap());
        assert_eq!(
            peer.latest_claim("agent/cedar", Some("harness.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["state"],
            "idle"
        );
        assert!(peer.replication_inventory().unwrap().envelopes.is_empty());
        assert_eq!(peer.local_observations_after(0, 100).unwrap().len(), 1);
    }
}
