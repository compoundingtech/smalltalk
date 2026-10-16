//! Period reads seek the last snapshot at each reporting boundary, not every historical rollup.
use super::*;

const INDEX: &str = "claims_usage_period_boundary_index";
const PREDICATE: &str = "kind='harness.usage' AND json_extract(body,'$.fields.semantics')='response_rollup'";
const SERIES_FIELDS: [&str; 6] = ["incarnation_id", "model", "account", "owner_run", "owner_step", "host"];

fn series_sql() -> String {
    let fields = SERIES_FIELDS.map(|field| format!(
        "CASE WHEN json_type(body,'$.fields.{field}')='text' THEN json_extract(body,'$.fields.{field}') ELSE '' END"
    ));
    format!("json_array(subject,{})", fields.join(","))
}

fn observed_sql() -> &'static str {
    // Space-padded decimal text preserves the full u64 domain, unlike SQLite INTEGER/REAL.
    // Non-u64 JSON readings have the same zero fallback as Value::as_u64 in the fold.
    "CASE WHEN json_type(body,'$.fields.observed_at_unix_ms')='integer' \
     AND (body -> '$.fields.observed_at_unix_ms') NOT LIKE '-%' \
     AND length(body -> '$.fields.observed_at_unix_ms')<=20 \
     AND printf('%020s',body -> '$.fields.observed_at_unix_ms')<='18446744073709551615' \
     THEN printf('%020s',body -> '$.fields.observed_at_unix_ms') ELSE printf('%020s','0') END"
}

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS {INDEX} ON claims({}, {}) WHERE {PREDICATE}",
        series_sql(), observed_sql()
    ))?;
    connection.execute_batch("DROP INDEX IF EXISTS claims_usage_rollup_index")?;
    Ok(())
}

pub(super) fn boundary_rows(
    connection: &Connection,
    boundaries: &BTreeSet<u64>,
) -> Result<Vec<(String, u64, String)>> {
    let series = series_sql();
    let observed = observed_sql();
    let boundaries = boundaries.iter().map(|boundary| format!("{boundary:>20}")).collect::<Vec<_>>();
    // A loose index scan: one successor seek per series, regardless of its history length.
    let mut next = connection.prepare_cached(&format!(
        "SELECT {series} FROM claims INDEXED BY {INDEX} WHERE {PREDICATE} AND {series}>?1 ORDER BY {series} LIMIT 1"
    ))?;
    let mut timestamp = connection.prepare_cached(&format!(
        "SELECT {observed} FROM claims INDEXED BY {INDEX} WHERE {PREDICATE} AND {series}=?1 AND {observed}<=?2 ORDER BY {observed} DESC LIMIT 1"
    ))?;
    let mut snapshots = connection.prepare_cached(&format!(
        "SELECT id,subject,store_index,body FROM claims INDEXED BY {INDEX} WHERE {PREDICATE} AND {series}=?1 AND {observed}=?2 LIMIT 2"
    ))?;
    // Canonical order matters only when a series has several snapshots at the same observed
    // timestamp. Keep the exact replicated tie-breaker, but never sort its whole history.
    let mut tied = connection.prepare_cached(&canonical_sql(&format!(
        "SELECT id,subject,store_index,body FROM claims INDEXED BY {INDEX} WHERE {PREDICATE} AND {series}=?1 AND {observed}=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1"
    )))?;
    let read_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(String, String, u64, String)> {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    };
    let mut result = BTreeMap::new();
    let mut previous = String::new();
    while let Some(key) = next.query_row([&previous], |row| row.get::<_, String>(0)).optional()? {
        let mut previous_at = None;
        for boundary in &boundaries {
            let Some(at) = timestamp.query_row(params![key, boundary], |row| row.get::<_, String>(0)).optional()? else {
                continue;
            };
            if previous_at.as_ref() == Some(&at) {
                continue;
            }
            let mut candidates = snapshots.query(params![key, at])?;
            let first = read_row(candidates.next()?.context("indexed usage timestamp has a snapshot")?)?;
            let has_ties = candidates.next()?.is_some();
            drop(candidates);
            let winner = if has_ties {
                tied.query_row(params![key, at], read_row)?
            } else {
                first
            };
            let (id, subject, index, body) = winner;
            result.insert(id, (subject, index, body));
            previous_at = Some(at);
        }
        previous = key;
    }
    Ok(result.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const DAY: u64 = 86_400_000;

    fn oracle(store: &Store, since: u64, until: u64) -> (Vec<Value>, Value) {
        let connection = store.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT subject,store_index,body FROM claims INDEXED BY claims_usage_rollup_index
             WHERE kind='harness.usage' AND json_extract(body,'$.fields.semantics')='response_rollup'
             ORDER BY CANONICAL_ASC(claims)",
        )).unwrap();
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        let (rows, days) = Store::usage_period_data_from_rows(rows, since, until).unwrap();
        (rows, store.agent_message_estimate(&days).unwrap())
    }

    fn fixture(count: usize) -> Store {
        let store = Store::open_memory("usage-fixture").unwrap();
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute_batch("CREATE INDEX claims_usage_rollup_index ON claims(store_index)
                WHERE kind='harness.usage' AND json_extract(body,'$.fields.semantics')='response_rollup'").unwrap();
            // Legacy batches exercise the canonical position fallback without a contrived
            // single huge batch. Accepted times and batch sequences oppose arrival order.
            for i in 0..count {
                let batch = format!("fixture-{}", i / 48);
                if i % 48 == 0 {
                    tx.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                                VALUES (?1,'fixture',?2,?1,'100')",
                        params![batch, count / 48 + 1 - i / 48]).unwrap();
                }
                let series = i % 24;
                let sample = i / 24;
                let at = if sample == 0 { 0 } else { DAY + (sample as u64 / 2) * 600_000 };
                let tokens = if sample >= 120 { (sample - 120) * 100 } else { sample * 100 };
                let body = json!({"fields":{
                    "semantics":"response_rollup","incarnation_id":"one",
                    "model":format!("model-{}",series % 2),"account":format!("account-{}",series % 3),
                    "owner_run":format!("run-{}",series % 2),"owner_step":format!("step-{}",series % 3),
                    "host":"fixture","observed_at_unix_ms":at,"total_tokens":tokens,
                    "input_tokens":tokens / 2,"cost_microusd":tokens * 10,"unpriced_tokens":series,
                    "pricing":"fixture","native_session_id":format!("session-{series}"),
                    "pricing_provenance":[{"version":"fixture","total_tokens":tokens,"cost_microusd":tokens * 10}]
                }});
                tx.execute(
                    "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                     VALUES (?1,?2,?3,'harness.usage','fixture',?4,'[]',?5)",
                    params![format!("usage-{i}"), batch, format!("agent/fixture-{series}"), body.to_string(),
                        (1000 + (count - i) / 48).to_string()],
                ).unwrap();
            }
            tx.commit().unwrap();
        }
        store
    }

    #[test]
    fn indexed_boundaries_match_full_fold_with_ties_restarts_and_daily_windows() {
        let store = fixture(12_000);
        for (since, until) in [
            (0, 0), (0, DAY), (DAY, 2 * DAY), (DAY + 1, 3 * DAY - 1),
            (2 * DAY, 3 * DAY), (0, 40 * DAY), (4 * DAY, 5 * DAY),
        ] {
            assert_eq!(store.usage_period_report(since, until).unwrap(), oracle(&store, since, until));
        }
        let boundaries = BTreeSet::from([DAY, 2 * DAY, 3 * DAY]);
        let rows = boundary_rows(&store.readers.get(), &boundaries).unwrap();
        assert!(rows.len() <= 24 * boundaries.len());
        let start = Instant::now();
        let before = oracle(&store, DAY, 2 * DAY);
        let before_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let after = store.usage_period_report(DAY, 2 * DAY).unwrap();
        let after_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(before, after);
        println!("client usage fixture: 12000 rollups / 24 series; full fold {before_ms:.3} ms -> indexed boundaries {after_ms:.3} ms; {} selected snapshots", rows.len());
    }

    #[test]
    fn boundary_seeks_use_index_without_history_sort() {
        let store = fixture(48);
        let connection = store.readers.get();
        let series = series_sql();
        let observed = observed_sql();
        for sql in [
            format!("SELECT {series} FROM claims INDEXED BY {INDEX} WHERE {PREDICATE} AND {series}>?1 ORDER BY {series} LIMIT 1"),
            format!("SELECT {observed} FROM claims INDEXED BY {INDEX} WHERE {PREDICATE} AND {series}=?1 AND {observed}<=?2 ORDER BY {observed} DESC LIMIT 1"),
        ] {
            let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let params = vec![""; statement.parameter_count()];
            let plan = statement.query_map(rusqlite::params_from_iter(params), |row| row.get::<_, String>(3))
                .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap().join("\n");
            assert!(plan.contains(&format!("SEARCH claims USING INDEX {INDEX}")), "{plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        }
    }

    #[test]
    fn observed_timestamp_index_preserves_u64_and_invalid_value_fallbacks() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute("CREATE TABLE readings(body TEXT)", []).unwrap();
        for value in [json!(0), json!(u64::MAX), json!(-1), json!(1.5), json!("123"), Value::Null] {
            connection.execute("INSERT INTO readings VALUES (?1)", [json!({"fields":{"observed_at_unix_ms":value}}).to_string()]).unwrap();
            let at: String = connection.query_row(&format!("SELECT {} FROM readings ORDER BY rowid DESC LIMIT 1", observed_sql()), [], |row| row.get(0)).unwrap();
            assert_eq!(at, format!("{:>20}", value.as_u64().unwrap_or(0)));
        }
    }
}
