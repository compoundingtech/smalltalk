//! Time quiet reconcile passes against a copy of a member's store.
//!
//! The reconciler runs a pass whenever the graph changes, on every member, so a pass's cost is
//! paid for every write in the fleet. This runs passes until they stop writing, then times quiet
//! ones and lists the statements one pass runs, most expensive first.
//!
//! The passes write to the copy and render into the workspaces it names, so run it on a copy
//! made for it, in a sandbox whose only writable directories are the copy's and a private `/tmp`:
//!
//! `ST3_PERF_NODE` is the member's node name. `ST3_PERF_RUNTIMES` names a JSON list of the member's
//! running runtimes (`runtime_id`, `incarnation_id`, `terminal`), taken from its
//! `/v1/client/runtimes`.
//!
//! ```sh
//! bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --bind COPY_DIR COPY_DIR \
//!   --unshare-pid --unshare-net --die-with-parent env ST3_PERF_STORE=COPY_DIR/claims.sqlite3 ST3_PERF_NODE=NODE \
//!   INTEGRATION_TEST_BINARY reconcile_pass_perf:: --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use tokio::sync::Notify;

/// A runtime in which every member the reconciler starts keeps running.
#[derive(Default)]
struct Started {
    running: Mutex<BTreeMap<String, RuntimeObservation>>,
}

impl RuntimeControl for Started {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        Ok(self
            .running
            .lock()
            .unwrap()
            .values()
            .map(|observation| RuntimeObservation {
                runtime_id: observation.runtime_id.clone(),
                terminal: observation.terminal,
                status: observation.status.clone(),
                exit_code: None,
                incarnation_id: observation.incarnation_id.clone(),
            })
            .collect())
    }
    fn observe_exec(&self, _: &str) -> Result<Option<RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, member: &st3::model::MemberSpec) -> Result<()> {
        let mut running = self.running.lock().unwrap();
        let incarnation = format!("{}:2026-10-01T00:00:00.000Z", 1000 + running.len());
        running.insert(
            member.runtime_id.clone(),
            RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(incarnation),
            },
        );
        Ok(())
    }
    fn stop(&self, runtime_id: &str, _: bool, _: Option<&str>) -> Result<()> {
        self.running.lock().unwrap().remove(runtime_id);
        Ok(())
    }
    fn kill(&self, runtime_id: &str, _: bool, _: Option<&str>) -> Result<()> {
        self.running.lock().unwrap().remove(runtime_id);
        Ok(())
    }
    fn remove(&self, runtime_id: &str, _: bool) -> Result<()> {
        self.running.lock().unwrap().remove(runtime_id);
        Ok(())
    }
    fn screen(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

fn thread_cpu_ms() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to a valid timespec; the clock reads this thread only.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0
}

#[test]
#[ignore = "needs ST3_PERF_STORE pointing at a copy of a member's store, inside a sandbox"]
fn quiet_reconcile_passes_on_a_store_copy() {
    // With ST3_PROFILE_DIR set, the file profiler writes each reconcile section's time to
    // totals.json there.
    st3::profile::init_from_env();
    let path = PathBuf::from(std::env::var("ST3_PERF_STORE").expect("set ST3_PERF_STORE"));
    let node = std::env::var("ST3_PERF_NODE").expect("set ST3_PERF_NODE to the member's node name");
    let store = Arc::new(st3::store::Store::open(&path, node.clone()).unwrap());
    // The member's running runtimes, as `/v1/client/runtimes` lists them, so passes see the same
    // incarnations its seats report harness state for.
    let runtime = Started::default();
    if let Ok(path) = std::env::var("ST3_PERF_RUNTIMES") {
        let listed: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut running = runtime.running.lock().unwrap();
        for item in listed {
            let runtime_id = item["runtime_id"].as_str().unwrap().to_owned();
            running.insert(
                runtime_id.clone(),
                RuntimeObservation {
                    runtime_id,
                    terminal: item["terminal"].as_bool().unwrap_or(true),
                    status: "running".into(),
                    exit_code: None,
                    incarnation_id: item["incarnation_id"].as_str().map(str::to_owned),
                },
            );
        }
    }
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(runtime),
        node,
        Arc::new(Notify::new()),
    );
    // Settle: start what is declared and record what changed, until a pass writes nothing.
    for settle in 0..30 {
        let before = store.index().unwrap();
        let started = Instant::now();
        let _ = reconciler.reconcile_once();
        let wrote = store.index().unwrap() - before;
        println!(
            "settling pass {settle}: {:.0} ms, {wrote} claims",
            started.elapsed().as_secs_f64() * 1000.0
        );
        if wrote == 0 {
            break;
        }
    }
    let queries = |report: &serde_json::Value| {
        report["queries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["kind"].as_str().unwrap().to_owned(),
                    (
                        row["count"].as_u64().unwrap(),
                        row["total_ms"].as_f64().unwrap(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = queries(&st3::performance::snapshot());
    let passes = 10;
    let mut wall = Vec::new();
    let mut cpu = Vec::new();
    for _ in 0..passes {
        let started = Instant::now();
        let cpu_started = thread_cpu_ms();
        st3::profile::task("task reconcile-pass", || reconciler.reconcile_once()).unwrap();
        cpu.push(thread_cpu_ms() - cpu_started);
        wall.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let after = queries(&st3::performance::snapshot());
    println!(
        "quiet pass: wall {:.0} ms, CPU on the pass thread {:.0} ms (mean of {passes})",
        wall.iter().sum::<f64>() / passes as f64,
        cpu.iter().sum::<f64>() / passes as f64
    );
    let mut rows = after
        .iter()
        .map(|(kind, (count, total))| {
            let (count_before, total_before) = before.get(kind).copied().unwrap_or_default();
            (
                (total - total_before) / passes as f64,
                (count - count_before) as f64 / passes as f64,
                kind,
            )
        })
        .filter(|(total, _, _)| *total > 0.0)
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("statements in one pass, by total time (top 20 of the report):");
    for (total, count, kind) in rows {
        println!(
            "  {total:8.1} ms  {count:7.1}x  {}",
            kind.chars().take(160).collect::<String>()
        );
    }
}
