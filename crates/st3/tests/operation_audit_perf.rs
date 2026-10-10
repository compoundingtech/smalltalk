#![cfg(target_os = "linux")]
//! The hydrated operation audit against the bounded one, on one generated store, each in a
//! process of its own: peak resident memory over the opened store, the longest read
//! transaction, how many transactions, and the whole audit's time.
//!
//! The generated store has many single-claim operations, one operation with a great many
//! claims and tombstones, tombstones beside the single operations, and some repaired claims.
//! Every claim carries a payload, so hydrating bodies costs what it costs on a member.
//!
//! The ordinary test runs a small store. The benchmark is ignored; run it with
//!
//! ```sh
//! INTEGRATION_TEST_BINARY operation_audit_perf::operation_audit_benchmark --exact --ignored --nocapture
//! ```
//!
//! - `ST3_OP_AUDIT_OPERATIONS`: single-claim operations, default 8192.
//! - `ST3_OP_AUDIT_HUGE_CLAIMS`: claims of the one large operation, default 8192.
//! - `ST3_OP_AUDIT_TOMBSTONES`: tombstones, half of them for the large operation, default 4096.
//! - `ST3_OP_AUDIT_BODY_BYTES`: payload bytes per claim, default 16384.
//! - `ST3_OP_AUDIT_DIR`: where the store is generated; the default is a temporary directory.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::store::Store;

use crate::operation_audit::{
    NODE, claim, mark_repaired, old_drift, old_operation_projection_drift, settle_operations,
    tombstone, write_claims, write_tombstones,
};

const CHILD_STORE: &str = "ST3_OP_AUDIT_CHILD_STORE";
const CHILD_AUDIT: &str = "ST3_OP_AUDIT_CHILD_AUDIT";
const RESULT: &str = "ST3_OP_AUDIT_RESULT ";
const CHUNK: usize = 256;

#[derive(Clone, Copy, Debug)]
struct Shape {
    operations: usize,
    huge_claims: usize,
    tombstones: usize,
    body_bytes: usize,
}

fn generate(path: &Path, shape: Shape) {
    let store = Store::open(path, NODE).unwrap();
    let started = Instant::now();
    // Reuse the daemon benchmark's sampled background history, then add the operation
    // groups, tombstones and repaired claims that its public idempotent API cannot create.
    super::daemon_bench::generate(&store, "operation-audit", 0.01);
    let mut ids = Vec::with_capacity(shape.operations + shape.huge_claims);
    for start in (0..shape.operations).step_by(CHUNK) {
        let claims = (start..(start + CHUNK).min(shape.operations))
            .map(|index| claim(format!("op/bench/{index:08}"), "d/1", &format!("bench-{index:08}")))
            .collect();
        ids.extend(write_claims(&store, claims, shape.body_bytes));
    }
    for start in (0..shape.huge_claims).step_by(CHUNK) {
        let claims = (start..(start + CHUNK).min(shape.huge_claims))
            .map(|index| {
                let digest = if index % 11 == 0 { "d/4" } else { "d/5" };
                claim("op/bench-huge", digest, &format!("bench-huge-{index:08}"))
            })
            .collect();
        ids.extend(write_claims(&store, claims, shape.body_bytes));
    }
    for start in (0..shape.tombstones).step_by(CHUNK) {
        let tombstones = (start..(start + CHUNK).min(shape.tombstones))
            .map(|index| {
                let id = format!("claim/bench-dropped/{index:08}");
                if index % 2 == 0 {
                    let digest = if index == 0 { "d/3" } else { "d/6" };
                    tombstone(&id, Some("op/bench-huge"), Some(digest))
                } else {
                    let operation = format!("op/bench/{:08}", index % shape.operations.max(1));
                    tombstone(&id, Some(&operation), Some("d/1"))
                }
            })
            .collect();
        write_tombstones(&store, tombstones);
    }
    mark_repaired(&store, ids.iter().step_by(97).cloned().collect());
    settle_operations(&store);
    assert!(old_drift(&store).is_empty());
    drop(store);
    // A reopen seals this node's batches, as a daemon start does; the measured opens then
    // find nothing left to write.
    drop(Store::open(path, NODE).unwrap());
    println!(
        "generated {shape:?} in {:.1}s",
        started.elapsed().as_secs_f64()
    );
}

fn status_kib(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or_else(|| panic!("{field} in /proc/self/status"))
}

/// Run in a child process: open the store, reset the peak resident set to what is resident
/// now, audit once, and print what it measured.
#[test]
#[ignore = "a child process of the operation audit measurements"]
fn audit_child() {
    let (Ok(path), Ok(audit)) = (std::env::var(CHILD_STORE), std::env::var(CHILD_AUDIT)) else {
        return;
    };
    let store = Store::open(Path::new(&path), NODE).unwrap();
    // Writing 5 resets VmHWM to the current resident set, so opening the store is not counted.
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    let baseline = status_kib("VmRSS:");
    let mut longest = Duration::ZERO;
    let mut transactions = 0_u64;
    let mut observer = |elapsed: Duration| {
        longest = longest.max(elapsed);
        transactions += 1;
    };
    let started = Instant::now();
    let drift = match audit.as_str() {
        "old" => old_operation_projection_drift(&store.readers.get(), &mut observer),
        "new" => store.operation_projection_drift_with_observer(&mut observer),
        other => panic!("unknown audit {other}"),
    }
    .unwrap();
    let total = started.elapsed();
    let peak = status_kib("VmHWM:");
    let counts = |sql: &str| -> i64 {
        store.readers.get().query_row(sql, [], |row| row.get(0)).unwrap()
    };
    println!(
        "{RESULT}{}",
        json!({
            "audit": audit,
            "drift": drift.len(),
            "transactions": transactions,
            "longest_transaction_ms": longest.as_secs_f64() * 1000.0,
            "total_ms": total.as_secs_f64() * 1000.0,
            "baseline_rss_kib": baseline,
            "peak_rss_kib": peak,
            "peak_over_baseline_kib": peak.saturating_sub(baseline),
            "claims": counts("SELECT COUNT(*) FROM claims"),
            "tombstones": counts("SELECT COUNT(*) FROM checkpoint_claims"),
            "operations": counts("SELECT COUNT(*) FROM operations"),
        })
    );
}

fn measure(path: &Path, audit: &str) -> Value {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "operation_audit_perf::audit_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_STORE, path)
        .env(CHILD_AUDIT, audit)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{audit} audit child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // libtest prints the test's name on the line its output starts.
    let line = stdout
        .lines()
        .find_map(|line| line.split_once(RESULT).map(|(_, result)| result))
        .unwrap_or_else(|| panic!("{audit} audit child printed no result:\n{stdout}"));
    serde_json::from_str(line).unwrap()
}

/// Generate one store, audit it with each audit in its own process, and check what both
/// must agree on.
fn compare(directory: &Path, shape: Shape) -> (Value, Value) {
    let path = directory.join("operation-audit.sqlite3");
    if !path.exists() {
        generate(&path, shape);
    }
    let old = measure(&path, "old");
    let new = measure(&path, "new");
    for field in ["claims", "tombstones", "operations"] {
        assert_eq!(old[field], new[field], "{field}: both audits read one store");
    }
    assert_eq!(old["drift"], 0, "{old}");
    assert_eq!(new["drift"], 0, "{new}");
    assert_eq!(old["transactions"], 1, "{old}");
    for measurement in [&old, &new] {
        println!(
            "{:>3}: {:>6} transactions, longest {:>9.1} ms, total {:>9.1} ms, peak RSS +{:>8} KiB (baseline {} KiB)",
            measurement["audit"].as_str().unwrap(),
            measurement["transactions"],
            measurement["longest_transaction_ms"].as_f64().unwrap(),
            measurement["total_ms"].as_f64().unwrap(),
            measurement["peak_over_baseline_kib"],
            measurement["baseline_rss_kib"],
        );
    }
    (old, new)
}

/// On a small store already, the hydrated audit holds one transaction for the whole pass and
/// keeps every body it read; the bounded audit holds many short ones and keeps a fraction.
#[test]
fn the_bounded_audit_holds_shorter_transactions_and_less_memory_than_the_hydrated_one() {
    let directory = tempfile::tempdir().unwrap();
    let shape = Shape {
        operations: 1024,
        huge_claims: 1024,
        tombstones: 512,
        body_bytes: 32 * 1024,
    };
    let (old, new) = compare(directory.path(), shape);
    let number = |value: &Value, field: &str| value[field].as_f64().unwrap();
    let pages = ((shape.operations + shape.huge_claims) / 128) as f64;
    assert!(number(&new, "transactions") >= pages, "{new}");
    assert!(number(&new, "longest_transaction_ms") < 100.0, "{new}");
    assert!(
        number(&new, "longest_transaction_ms") < number(&old, "longest_transaction_ms"),
        "old {old}\nnew {new}"
    );
    // The hydrated audit keeps nearly 64 MiB of parsed payloads at once; one in 97 claims is
    // repaired and skipped.
    let hydrated = ((shape.operations + shape.huge_claims) * shape.body_bytes / 1024) as f64;
    assert!(number(&old, "peak_over_baseline_kib") >= 0.9 * hydrated, "{old}");
    assert!(
        number(&new, "peak_over_baseline_kib") * 4.0 < number(&old, "peak_over_baseline_kib"),
        "old {old}\nnew {new}"
    );
}

fn env_count(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |value| {
        value.parse().unwrap_or_else(|_| panic!("{name} must be a count"))
    })
}

#[test]
#[ignore = "generates a store of hundreds of megabytes; run it explicitly"]
fn operation_audit_benchmark() {
    let shape = Shape {
        operations: env_count("ST3_OP_AUDIT_OPERATIONS", 8192),
        huge_claims: env_count("ST3_OP_AUDIT_HUGE_CLAIMS", 8192),
        tombstones: env_count("ST3_OP_AUDIT_TOMBSTONES", 4096),
        body_bytes: env_count("ST3_OP_AUDIT_BODY_BYTES", 16 * 1024),
    };
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::env::var_os("ST3_OP_AUDIT_DIR")
        .map_or_else(|| temporary.path().to_path_buf(), Into::into);
    std::fs::create_dir_all(&directory).unwrap();
    let (old, new) = compare(&directory, shape);
    assert!(new["longest_transaction_ms"].as_f64().unwrap() < 100.0, "{new}");
    println!("old {old}\nnew {new}");
}
