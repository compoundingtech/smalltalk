//! Single-use, scratch-only applicability check through the actual production source.
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use st3::store::{Store, collection_ivm::agent_source::TABLES};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const TOTAL: u64 = 1_000_000;
const ROW_BYTES: u64 = 64 * 1024;

fn gauges(store: &Store) -> Result<()> {
    // TEMP schema is deliberately excluded from the source's main-schema fingerprint.
    // Observe already-required shadow writes; do not scan native tables a second time.
    let c = store.connection.write();
    c.execute_batch("CREATE TEMP TABLE qualification_counts(namespace TEXT,source_table TEXT,rows INTEGER NOT NULL,max_json_bytes INTEGER NOT NULL,PRIMARY KEY(namespace,source_table));
        CREATE TEMP TRIGGER qualification_insert AFTER INSERT ON main.local_agent_source_atoms BEGIN
          INSERT INTO qualification_counts VALUES(NEW.namespace,NEW.source_table,1,length(CAST(NEW.body AS BLOB)))
          ON CONFLICT(namespace,source_table) DO UPDATE SET rows=rows+1,max_json_bytes=MAX(max_json_bytes,excluded.max_json_bytes);
        END;
        CREATE TEMP TRIGGER qualification_update AFTER UPDATE OF body ON main.local_agent_source_atoms BEGIN
          UPDATE qualification_counts SET max_json_bytes=MAX(max_json_bytes,length(CAST(NEW.body AS BLOB))) WHERE namespace=NEW.namespace AND source_table=NEW.source_table;
        END;
        CREATE TEMP TRIGGER qualification_delete AFTER DELETE ON main.local_agent_source_atoms BEGIN
          UPDATE qualification_counts SET rows=rows-1 WHERE namespace=OLD.namespace AND source_table=OLD.source_table;
        END;")?;
    Ok(())
}

fn counts(store: &Store, namespace: &str) -> Result<(Value, u64, u64)> {
    let c = store.connection.write();
    let mut result = serde_json::Map::new();
    let mut total = 0_u64;
    let mut max = 0_u64;
    for table in TABLES {
        let (rows, bytes): (u64, u64) = c.query_row(
            "SELECT COALESCE((SELECT rows FROM qualification_counts WHERE namespace=?1 AND source_table=?2),0),COALESCE((SELECT max_json_bytes FROM qualification_counts WHERE namespace=?1 AND source_table=?2),0)",
            params![namespace, table.name], |r| Ok((r.get(0)?, r.get(1)?)))?;
        total = total.checked_add(rows).context("cardinality overflow")?;
        max = max.max(bytes);
        result.insert(table.name.into(), json!(rows));
    }
    Ok((Value::Object(result), total, max))
}

fn qualify(path: &Path, receiver: &str, deadline: Duration) -> Result<Value> {
    ensure!(path.is_file(), "existing scratch backup required");
    ensure!(
        path.parent()
            .context("scratch parent missing")?
            .join(".ivm-qualification-scratch")
            .is_file(),
        "scratch sentinel required"
    );
    let started = Instant::now();
    let store = Store::open_with_agent_collections(path, receiver)?;
    ensure!(
        !store.agent_collection_qualification_status()?["stopped"]
            .as_bool()
            .unwrap_or(true),
        "source setup refused"
    );
    gauges(&store)?;
    let mut pumps = 0_u64;
    let mut max_pump_us = 0_u128;
    loop {
        ensure!(
            started.elapsed() < deadline,
            "qualification deadline reached"
        );
        let before = Instant::now();
        let more = store.maintain_agent_collections()?;
        max_pump_us = max_pump_us.max(before.elapsed().as_micros());
        pumps += 1;
        let status = store.agent_collection_qualification_status()?;
        ensure!(
            !status["stopped"].as_bool().unwrap_or(true)
                && !status["job"]["refused"].as_bool().unwrap_or(false),
            "source installation refused"
        );
        if status["ready"] == true {
            let namespace = status["namespace"]
                .as_str()
                .context("ready source without genuine namespace")?;
            let (tables, total, max_json) = counts(&store, namespace)?;
            // Source is single-threaded here. Recheck the whole live boundary and exact cut
            // after sampling the gauges; filesystem/producer evidence can still revoke it.
            let after = store.agent_collection_qualification_status()?;
            if after["ready"] != true
                || after["source"] != status["source"]
                || after["cut"] != status["cut"]
                || after["namespace"] != status["namespace"]
                || after["database_id"] != status["database_id"]
            {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            ensure!(
                total <= TOTAL && max_json <= ROW_BYTES,
                "same-cut applicability limit exceeded"
            );
            return Ok(
                json!({"result":"QUALIFIED_SCRATCH_ONLY","source":status,"same_cut_table_rows":tables,"same_cut_total_rows":total,"observed_max_shadow_json_bytes":max_json,"enforced_raw_cell_bytes":ROW_BYTES,"page_rows":128,"page_bytes":1048576,"callback_limit_ms":1000,"pumps":pumps,"max_pump_us":max_pump_us,"elapsed_ms":started.elapsed().as_millis(),"instrumentation":"TEMP shadow-write gauges; callback measurements include this overhead","live_host_producer_and_profiles":"UNQUALIFIED"}),
            );
        }
        if !more {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 3 {
        eprintln!("usage: ivm_agent_qualify SCRATCH_DATABASE ORIGINAL_RECEIVER");
        std::process::exit(2);
    }
    match qualify(Path::new(&args[1]), &args[2], Duration::from_secs(900)) {
        Ok(report) => println!("{report}"),
        Err(error) => {
            // Generic public output; detailed cause stays in the private scratch stderr.
            eprintln!("{error:#}");
            println!("{{\"result\":\"UNAVAILABLE\"}}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn scratch() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ivm-qualification-scratch"), b"fixture").unwrap();
        let path = dir.path().join("backup.sqlite");
        drop(Store::open(&path, "node").unwrap());
        (dir, path)
    }
    #[test]
    fn ordinary_populated_source_uses_genuine_installation_and_all_table_gauges() {
        let _lock = LOCK.lock().unwrap();
        let (_dir, path) = scratch();
        let store = Store::open(&path, "node").unwrap();
        let intent = st3::graph::parse_intent(
            "version 2\nagent \"amber\" { name \"Amber\"; harness \"claude\" {}; }\n",
            "node",
        )
        .unwrap();
        store
            .apply_internal(&intent, "qualification-fixture")
            .unwrap();
        drop(store);
        if let Ok(destination) = std::env::var("ST_IVM_QUALIFIER_TEST_BACKUP") {
            let destination = std::path::PathBuf::from(destination);
            assert!(destination.is_dir());
            std::fs::copy(&path, destination.join("backup.sqlite")).unwrap();
        }
        let report = qualify(&path, "node", Duration::from_secs(30)).unwrap();
        assert_eq!(report["result"], "QUALIFIED_SCRATCH_ONLY");
        assert_eq!(report["same_cut_table_rows"].as_object().unwrap().len(), 16);
        assert!(report["same_cut_table_rows"]["claims"].as_u64().unwrap() > 0);
        assert!(!report["source"]["namespace"].as_str().unwrap().is_empty());
        let c = rusqlite::Connection::open(&path).unwrap();
        let actual: u64 = c
            .query_row(
                "SELECT count(*) FROM local_agent_source_atoms WHERE namespace=?1",
                [report["source"]["namespace"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(report["same_cut_total_rows"].as_u64().unwrap(), actual);
    }
    #[test]
    fn refuses_a_database_without_scratch_marker() {
        let _lock = LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.sqlite");
        drop(Store::open(&path, "node").unwrap());
        assert!(qualify(&path, "node", Duration::from_secs(1)).is_err());
    }
    #[test]
    fn expired_budget_never_reports_ready() {
        let _lock = LOCK.lock().unwrap();
        let (_dir, path) = scratch();
        assert!(qualify(&path, "node", Duration::ZERO).is_err());
    }
    #[test]
    fn actual_extractor_refuses_oversized_raw_input() {
        let _lock = LOCK.lock().unwrap();
        let (_dir, path) = scratch();
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute("INSERT INTO local_observations(subject,kind,actor,body,observed_at_unix_ms,after_store_index) VALUES('agent/node.amber','runtime.observed',NULL,?1,0,0)", [format!("{{\"status\":\"running\",\"extra\":\"{}\"}}", "x".repeat(70_000))]).unwrap();
        drop(c);
        assert!(qualify(&path, "node", Duration::from_secs(30)).is_err());
    }
}
