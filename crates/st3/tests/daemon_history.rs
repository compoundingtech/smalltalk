//! The history check, advisory: does a request cost more when the same current state has a longer
//! past? It measures every route (as `daemon_cost` does) on one generated store twice, once where
//! the first seat has a few past observations before its current one and once with many more,
//! and writes both measurements. It asserts nothing about them: `scripts/sql_advisory.py` reads
//! the report and says which routes cost more with the longer past. A correct read costs the
//! same on both.
//!
//! ```sh
//! ST_HISTORY_REPORT=/tmp/history.json \
//!   cargo test -p st3 --test integration daemon_history:: -- --nocapture --test-threads 1
//! ```
//!
//! - `ST_HISTORY_SCALE` is the generated scale (default `0.01`).
//! - `ST_HISTORY_SHORT` and `ST_HISTORY_LONG` are the past observations of the first seat
//!   (defaults 20 and 200, ten times as many).
//! - `ST_BENCH_DIR` keeps generated stores, as for `daemon_cost`.
//!
//! The counts are the whole process's, so nothing else may run beside it. Past history so far is
//! two kinds: a seat's repeated harness observations, the audit's largest offender, and old usage rollups; finished runs
//! are still to come.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::daemon_bench::generated_stores;
use crate::daemon_cost::measure;

fn number(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_does_not_change_what_requests_cost() {
    let scale = std::env::var("ST_HISTORY_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.01);
    let (short, long) = (number("ST_HISTORY_SHORT", 20), number("ST_HISTORY_LONG", 200));
    assert!(long > short, "the long past must be longer than the short one");
    let keep = std::env::var_os("ST_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/st-bench"));
    std::fs::create_dir_all(&keep).unwrap();
    let (store, peer) = generated_stores(&keep, scale).await;
    // `measure` copies the store for each run, so the two runs start from the same bytes.
    let before = measure(scale, &store, &peer, Some(short)).await;
    let after = measure(scale, &store, &peer, Some(long)).await;
    println!(
        "history {short} -> {long}: {} claims -> {}, {} requests measured",
        before.claims,
        after.claims,
        after.costs.len()
    );
    assert!(!after.costs.is_empty(), "nothing was measured");
    if let Some(path) = std::env::var_os("ST_HISTORY_REPORT") {
        let report = json!({
            "scale": scale,
            "history": [short, long],
            "claims": [before.claims, after.claims],
            "short": before.costs,
            "long": after.costs,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
