//! Bounded dependency hints, not an ordered mutation or authority certificate.
use super::*;

pub(crate) const CHANGE_ROWS: usize = 256;
pub(crate) const CHANGE_BYTES: u64 = 1024 * 1024;

// Direct column arguments preserve SQLite's byte-length/type metadata opcodes. In particular,
// do not introduce CAST or JSON extraction before this admission check.
const CLAIM_METADATA: &str = "SELECT COUNT(*),COALESCE(SUM(bytes),0),COALESCE(SUM(invalid),0) FROM (
    SELECT octet_length(subject)+octet_length(kind)+COALESCE(octet_length(actor),0)+octet_length(body) AS bytes,
      CASE WHEN typeof(subject)='text' AND typeof(kind)='text' AND typeof(body)='text'
        AND typeof(actor) IN ('text','null') THEN 0 ELSE 1 END AS invalid
    FROM claims WHERE store_index>?1 AND store_index<=?2 ORDER BY store_index LIMIT ?3)";
const LOCAL_METADATA: &str = "SELECT COUNT(*),COALESCE(SUM(bytes),0),COALESCE(SUM(invalid),0) FROM (
    SELECT octet_length(subject)+octet_length(kind)+COALESCE(octet_length(actor),0)+octet_length(body) AS bytes,
      CASE WHEN typeof(subject)='text' AND typeof(kind)='text' AND typeof(body)='text'
        AND typeof(actor) IN ('text','null') THEN 0 ELSE 1 END AS invalid
    FROM local_observations WHERE id>?1 AND id<=?2 ORDER BY id LIMIT ?3)";

pub(crate) struct ReconcileChanges {
    pub feed: ChangeFeed,
    /// A reset or a range beyond either limit requires all consumers to read again.
    pub invalidate_all: bool,
}

impl Store {
    /// Read both append ranges and their frontiers at one cut, releasing it before effects.
    /// Direct-column metadata probes admit text identities/body and text-or-null actors,
    /// counting at most limit+1 rows without loading overflow payloads. Overflow/type refusal
    /// returns no partial hints: consumers must invalidate every retained evaluation.
    /// Physical edits/removals at unchanged frontiers are outside this append-only seam.
    pub(crate) fn reconcile_changes_since(
        &self,
        index: u64,
        local: i64,
    ) -> Result<ReconcileChanges> {
        // One statement is already a coherent empty cut. Do not open a read transaction or
        // run payload probes for unrelated passes when neither append frontier moved.
        let (at_index, at_local): (u64, i64) = {
            let connection = self.readers.get();
            connection.query_row(
                "SELECT (SELECT COALESCE(MAX(store_index),0) FROM claims),
                        (SELECT COALESCE(MAX(id),0) FROM local_observations)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
        };
        if index > at_index || local > at_local || (index == at_index && local == at_local) {
            return Ok(ReconcileChanges {
                feed: ChangeFeed {
                    index: at_index,
                    local: at_local,
                    changes: Vec::new(),
                },
                invalidate_all: index > at_index || local > at_local,
            });
        }
        self.read_snapshot(|_| {
            let connection = self.readers.get();
            let (to_index, to_local): (u64, i64) = connection.query_row(
                "SELECT (SELECT COALESCE(MAX(store_index),0) FROM claims),
                        (SELECT COALESCE(MAX(id),0) FROM local_observations)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut result = ReconcileChanges {
                feed: ChangeFeed {
                    index: to_index,
                    local: to_local,
                    changes: Vec::new(),
                },
                invalidate_all: index > to_index || local > to_local,
            };
            if result.invalidate_all || (index == to_index && local == to_local) {
                return Ok(result);
            }
            let limit = (CHANGE_ROWS + 1) as u64;
            let measure = |sql: &str, from: i64, to: i64| -> Result<(u64, u64, u64)> {
                Ok(connection.query_row(sql, params![from, to, limit], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?)
            };
            let claims = measure(
                CLAIM_METADATA,
                index.min(i64::MAX as u64) as i64,
                to_index.min(i64::MAX as u64) as i64,
            )?;
            let observations = measure(LOCAL_METADATA, local, to_local)?;
            if claims.0.saturating_add(observations.0) > CHANGE_ROWS as u64
                || claims.1.saturating_add(observations.1) > CHANGE_BYTES
                || claims.2 != 0
                || observations.2 != 0
            {
                result.invalidate_all = true;
                return Ok(result);
            }
            result.feed.changes = connection
                .prepare_cached(
                    "SELECT subject,kind,actor,body FROM claims
                 WHERE store_index>?1 AND store_index<=?2 ORDER BY store_index",
                )?
                .query_map(params![index, to_index], change_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            result.feed.changes.extend(
                connection
                    .prepare_cached(
                        "SELECT subject,kind,actor,body FROM local_observations
                 WHERE id>?1 AND id<=?2 ORDER BY id",
                    )?
                    .query_map(params![local, to_local], change_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallclaims::sqlite::work::SqliteWorkScope;

    // Reader fixtures only: these raw rows do not model admission, authority or replication.
    fn claim(connection: &Connection, number: usize, body: &str) {
        let id = format!("fixture-{number}");
        connection
            .execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
            VALUES(?1,'foreign',?2,?1,'1')",
                params![id, number],
            )
            .unwrap();
        connection.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
            VALUES(?1,?1,?2,'resource.observed','foreign','agent/fixture',?3,'[]','1')",
            params![id, format!("resource/{number}"), body]).unwrap();
    }

    fn local(connection: &Connection, number: usize) {
        connection.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
            VALUES(0,?1,'harness.observed','{}',1)", [format!("agent/{number}")]).unwrap();
    }

    fn json_body_with_bytes(bytes: usize) -> String {
        // Preserve the exact payload size while satisfying the production JSON indexes.
        assert!(bytes >= 2);
        let body = serde_json::to_string(&"x".repeat(bytes - 2)).unwrap();
        assert_eq!(body.len(), bytes);
        body
    }

    #[test]
    fn empty_discovery_is_one_statement_and_does_not_hold_a_snapshot() {
        let store = Store::open_memory("node").unwrap();
        let costs = SqliteWorkScope::start();
        let observed = store.reconcile_changes_since(0, 0).unwrap();
        let cost = costs.finish();
        assert!(!observed.invalidate_all && observed.feed.changes.is_empty());
        assert_eq!((observed.feed.index, observed.feed.local), (0, 0));
        assert_eq!(cost.statements, 1, "{cost:?}");
        assert_eq!(cost.fullscan_steps, 0);
        smallclaims::sqlite::debug_assert_no_pinned_read();
    }

    #[test]
    fn combined_stream_row_limit_is_inclusive_and_never_returns_partial_hints() {
        for (claims, locals, overflow) in [
            (256, 0, false),
            (0, 256, false),
            (128, 128, false),
            (129, 128, true),
        ] {
            let store = Store::open_memory("node").unwrap();
            {
                let writer = store.connection.write();
                for n in 1..=claims {
                    claim(&writer, n, "{}");
                }
                for n in 1..=locals {
                    local(&writer, n);
                }
            }
            let observed = store.reconcile_changes_since(0, 0).unwrap();
            assert_eq!(observed.invalidate_all, overflow);
            assert_eq!(
                (observed.feed.index, observed.feed.local),
                (claims as u64, locals as i64)
            );
            assert_eq!(
                observed.feed.changes.len(),
                if overflow { 0 } else { claims + locals }
            );
            if claims > 0 && !overflow {
                assert_eq!(
                    observed.feed.changes[0].actor.as_deref(),
                    Some("agent/fixture")
                );
            }
        }
    }

    #[test]
    fn payload_limit_counts_identity_and_body_and_rejects_an_oversized_single_row() {
        for (bytes, overflow) in [
            (CHANGE_BYTES as usize - 40, false),
            (CHANGE_BYTES as usize, true),
        ] {
            let store = Store::open_memory("node").unwrap();
            claim(&store.connection.write(), 1, &json_body_with_bytes(bytes));
            // 10-byte subject + 17-byte kind + 13-byte actor = 40 bytes beyond the body.
            let observed = store.reconcile_changes_since(0, 0).unwrap();
            assert_eq!(observed.invalidate_all, overflow);
            assert_eq!(observed.feed.changes.len(), usize::from(!overflow));
            assert_eq!(observed.feed.index, 1);
        }
    }

    #[test]
    fn actual_metadata_queries_use_locked_sqlite_column_byte_and_type_opcodes() {
        assert_eq!(rusqlite::version_number(), 3_046_000);
        let store = Store::open_memory("node").unwrap();
        claim(&store.connection.write(), 1, "{}");
        local(&store.connection.write(), 1);
        let connection = store.readers.get();
        for (table, sql) in [
            ("claims", CLAIM_METADATA),
            ("local_observations", LOCAL_METADATA),
        ] {
            let root: i32 = connection
                .query_row(
                    "SELECT rootpage FROM sqlite_schema WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            let columns = connection
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, i32>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let ops = connection
                .prepare(&format!("EXPLAIN {sql}"))
                .unwrap()
                .query_map(params![0, 1, CHANGE_ROWS + 1], |row| {
                    Ok((
                        row.get::<_, String>(1)?,
                        row.get::<_, i32>(2)?,
                        row.get::<_, i32>(3)?,
                        row.get::<_, i32>(6)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let cursor = ops
                .iter()
                .find(|(op, _, p2, _)| op == "OpenRead" && *p2 == root)
                .unwrap()
                .1;
            assert!(
                !ops.iter().any(|(op, _, _, _)| op == "Cast"),
                "{table}: {ops:?}"
            );
            for column in ["subject", "kind", "actor", "body"] {
                let index = columns.iter().find(|(_, name)| name == column).unwrap().0;
                let reads = ops
                    .iter()
                    .filter(|(op, p1, p2, _)| op == "Column" && *p1 == cursor && *p2 == index)
                    .collect::<Vec<_>>();
                assert!(!reads.is_empty(), "{table}.{column}: {ops:?}");
                assert!(
                    reads.iter().all(|(_, _, _, p5)| p5 & 0xc0 != 0),
                    "payload column read before admission: {table}.{column}: {ops:?}"
                );
                assert!(
                    reads.iter().any(|(_, _, _, p5)| p5 & 0xc0 == 0xc0),
                    "direct byte-length opcode missing: {table}.{column}: {ops:?}"
                );
            }
        }
    }

    #[test]
    fn oversized_overflow_cells_are_refused_without_fetching_their_contents() {
        assert_eq!(rusqlite::version_number(), 3_046_000);
        for table in ["claims", "local_observations"] {
            for column in ["subject", "kind", "actor", "body"] {
                let store = Store::open_memory("node").unwrap();
                if table == "claims" {
                    claim(&store.connection.write(), 1, "{}");
                } else {
                    local(&store.connection.write(), 1);
                }
                let wide_cell = if column == "body" {
                    json_body_with_bytes(2 * CHANGE_BYTES as usize)
                } else {
                    "x".repeat(2 * CHANGE_BYTES as usize)
                };
                store
                    .connection
                    .write()
                    .execute(&format!("UPDATE {table} SET {column}=?1"), [&wide_cell])
                    .unwrap();
                store
                    .read_snapshot(|_| {
                        let connection = store.readers.get();
                        // A private reader handle: no global limit, writer/admission or production
                        // setting is changed. A real wide-cell fetch is the negative control.
                        let prior = unsafe {
                            rusqlite::ffi::sqlite3_limit(
                                connection.handle(),
                                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                                128 * 1024,
                            )
                        };
                        assert!(
                            connection
                                .query_row(&format!("SELECT {column} FROM {table}"), [], |row| {
                                    row.get::<_, String>(0)
                                })
                                .is_err(),
                            "wide-cell negative: {table}.{column}"
                        );
                        let observed = store.reconcile_changes_since(0, 0);
                        unsafe {
                            rusqlite::ffi::sqlite3_limit(
                                connection.handle(),
                                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                                prior,
                            );
                        }
                        let observed = observed?;
                        assert!(
                            observed.invalidate_all && observed.feed.changes.is_empty(),
                            "{table}.{column}"
                        );
                        assert_eq!(observed.feed.index + observed.feed.local as u64, 1);
                        Ok(())
                    })
                    .unwrap();
            }
        }
    }

    #[test]
    fn unsupported_physical_storage_types_invalidate_without_partial_hints() {
        let store = Store::open_memory("node").unwrap();
        // Reader corruption model only. Untyped private columns preserve physical numeric/null
        // cases that production TEXT affinity/NOT NULL would normally prevent at insertion.
        // This models neither a schema migration nor Store/replication admission eligibility.
        store
            .connection
            .write()
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
            DROP TABLE claims; DROP TABLE local_observations;
            CREATE TABLE claims(store_index INTEGER PRIMARY KEY,subject,kind,actor,body);
            CREATE TABLE local_observations(id INTEGER PRIMARY KEY,subject,kind,actor,body);",
            )
            .unwrap();
        for table in ["claims", "local_observations"] {
            for column in ["subject", "kind", "actor", "body"] {
                for value in [
                    rusqlite::types::Value::Blob(b"{}".to_vec()),
                    rusqlite::types::Value::Integer(17),
                    rusqlite::types::Value::Real(1.5),
                    rusqlite::types::Value::Null,
                    rusqlite::types::Value::Text("{}".into()),
                ] {
                    let accepted = matches!(value, rusqlite::types::Value::Text(_))
                        || (column == "actor" && matches!(value, rusqlite::types::Value::Null));
                    {
                        let writer = store.connection.write();
                        writer
                            .execute_batch("DELETE FROM claims; DELETE FROM local_observations;")
                            .unwrap();
                        writer.execute(&format!("INSERT INTO {table} VALUES(1,'resource/a','resource.observed',NULL,'{{}}')"), []).unwrap();
                        writer
                            .execute(&format!("UPDATE {table} SET {column}=?1"), [&value])
                            .unwrap();
                    }
                    let observed = store.reconcile_changes_since(0, 0).unwrap();
                    assert_eq!(
                        observed.invalidate_all, !accepted,
                        "{table}.{column}: {value:?}"
                    );
                    assert_eq!(observed.feed.changes.len(), usize::from(accepted));
                }
            }
        }
    }

    #[test]
    fn range_work_is_bounded_when_unrelated_retained_history_grows() {
        let mut costs = Vec::new();
        for history in [100, 10_000] {
            let store = Store::open_memory("node").unwrap();
            {
                let mut writer = store.connection.write();
                let transaction = writer.transaction().unwrap();
                for n in 1..=history + 1 {
                    claim(&transaction, n, "{}");
                }
                transaction.commit().unwrap();
            }
            let scope = SqliteWorkScope::start();
            let observed = store.reconcile_changes_since(history as u64, 0).unwrap();
            let cost = scope.finish();
            assert!(!observed.invalidate_all);
            assert_eq!(observed.feed.changes.len(), 1);
            assert_eq!(cost.fullscan_steps, 0, "{cost:?}");
            assert!(cost.statements <= 12 && cost.vm_steps <= 1000, "{cost:?}");
            costs.push(cost);
        }
        assert!(costs[1].vm_steps <= costs[0].vm_steps + 100, "{costs:?}");
    }

    #[test]
    fn pinned_frontiers_and_both_ranges_share_the_same_cut() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&directory.path().join("cut.db"), "node").unwrap());
        claim(&store.connection.write(), 1, "{}");
        store
            .read_snapshot(|_| {
                let before = store.reconcile_changes_since(0, 0)?;
                let other = store.clone();
                std::thread::spawn(move || {
                    let mut writer = other.connection.write();
                    let transaction = writer.transaction().unwrap();
                    claim(&transaction, 2, "{}");
                    local(&transaction, 1);
                    transaction.commit().unwrap();
                })
                .join()
                .unwrap();
                let retained = store.reconcile_changes_since(0, 0)?;
                assert_eq!((before.feed.index, before.feed.local), (1, 0));
                assert_eq!((retained.feed.index, retained.feed.local), (1, 0));
                assert_eq!(retained.feed.changes.len(), 1);
                Ok(())
            })
            .unwrap();
        let after = store.reconcile_changes_since(1, 0).unwrap();
        assert_eq!((after.feed.index, after.feed.local), (2, 1));
        assert_eq!(after.feed.changes.len(), 2);
        smallclaims::sqlite::debug_assert_no_pinned_read();
    }

    #[test]
    fn decreased_frontiers_require_invalidation_and_sql_errors_are_not_empty_success() {
        let store = Store::open_memory("node").unwrap();
        claim(&store.connection.write(), 1, "{}");
        local(&store.connection.write(), 1);
        let current = store.reconcile_changes_since(0, 0).unwrap().feed;
        store
            .connection
            .write()
            .execute("DELETE FROM local_observations", [])
            .unwrap();
        let reset = store
            .reconcile_changes_since(current.index, current.local)
            .unwrap();
        assert!(reset.invalidate_all && reset.feed.changes.is_empty());
        assert_eq!((reset.feed.index, reset.feed.local), (1, 0));
        store
            .connection
            .write()
            .execute(
                "ALTER TABLE local_observations RENAME TO unavailable_local",
                [],
            )
            .unwrap();
        assert!(store.reconcile_changes_since(1, 0).is_err());
    }
}
