//! Isolated daemon API comparison using the existing invented-data benchmark generator.
//! Run each cache setting in a fresh process; all state is under the scratch directory.
#![allow(dead_code, unused_imports)]
#[path = "../tests/daemon_bench.rs"]
mod daemon_bench;

use serde_json::Value;
use st3::{api::AppState, client::Client, store::Store};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::{Notify, watch};

fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

fn rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> anyhow::Result<()> {
    let keep = PathBuf::from(std::env::args().nth(1).expect("scratch fixture directory"));
    std::fs::create_dir_all(&keep)?;
    let source = daemon_bench::generated_store(&keep, 0.1).await;
    if std::env::args().any(|arg| arg == "--generate-only") {
        return Ok(());
    }
    let work = tempfile::tempdir()?;
    let root = work.path();
    let state_dir = root.join("state");
    std::fs::create_dir_all(&state_dir)?;
    for suffix in ["", "-wal"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            std::fs::copy(from, state_dir.join(format!("claims.sqlite3{suffix}")))?;
        }
    }
    let store = Arc::new(Store::open(
        &state_dir.join("claims.sqlite3"),
        daemon_bench::NODE,
    )?);
    let socket = root.join("st3.sock");
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: daemon_bench::NODE.into(),
        state_dir,
        pty_root: root.join("pty"),
        pty_binary: daemon_bench::stub_pty(root),
        fleet_id: Some(daemon_bench::FLEET.into()),
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: Some(root.join("home")),
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    println!(
        "claims={} cache_kib={}",
        store.index()?,
        smallclaims::sqlite::read_cache_kib()
    );
    let routes = [
        (
            "mailbox",
            "/v1/messages/page?include_closed=false&limit=100&to=agent%2Fbench%2Fseat-1",
        ),
        ("work", "/v1/work?actor=agent%2Fbench%2Fseat-1"),
        ("interactive", "/v1/client/missions"),
        ("replication", "/v1/replication/status"),
    ];
    for wave in 0..3 {
        for (label, route) in routes {
            let cpu = cpu_ms();
            let started = Instant::now();
            let output = tokio::process::Command::new("python3")
                .args(["-c", include_str!("reader_cache_probe.py")])
                .arg(&socket)
                .arg(route)
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "client probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let samples: Value = serde_json::from_slice(&output.stdout)?;
            println!(
                "wave={wave} route={label} requests={} cpu_ms={:.3} wall_ms={:.3} p50_ms={:.3} p95_ms={:.3} rss_kib={:?} readers={:?}",
                samples["requests"],
                cpu_ms() - cpu,
                started.elapsed().as_secs_f64() * 1000.0,
                samples["p50_ms"].as_f64().unwrap(),
                samples["p95_ms"].as_f64().unwrap(),
                rss_kib(),
                store.readers.usage()
            );
        }
    }
    let doctor: Value = Client::unix(&socket).get("/v1/doctor").await?;
    println!(
        "reader-doctor={}",
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "reader-memory")
            .unwrap()
    );
    server.abort();
    Ok(())
}
