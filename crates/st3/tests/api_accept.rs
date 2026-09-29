#![cfg(target_os = "linux")]
//! The local API keeps serving after an accept fails.
//!
//! Many agents, PTYs, and websockets can exhaust the daemon's file descriptors. An accept that
//! fails then must not end the daemon. This test lowers its own descriptor limit, so it lives in
//! its own test binary and process.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use st3::api::AppState;
use st3::store::Store;
use tokio::sync::{Notify, watch};

fn open_descriptors() -> u64 {
    std::fs::read_dir("/proc/self/fd").unwrap().count() as u64
}

fn set_descriptor_limit(soft: u64) -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    let previous = limit.rlim_cur;
    limit.rlim_cur = soft;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    previous
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_keeps_serving_after_an_accept_runs_out_of_descriptors() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = AppState {
        store: Arc::new(Store::open_memory("accept-node").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "accept-node".into(),
        state_dir: root.path().join("daemon"),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    while UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Fill every descriptor the process may open. The server then fails to accept the
    // connections still waiting in its backlog.
    let previous = set_descriptor_limit(open_descriptors() + 16);
    let mut held = Vec::new();
    while let Ok(stream) = UnixStream::connect(&socket) {
        held.push(stream);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !server.is_finished(),
        "the API ended after an accept failed: {:?}",
        server.await
    );
    drop(held);
    set_descriptor_limit(previous);

    let health = st3::client::Client::new(st3::client::Endpoint::Unix(socket.clone()))
        .get::<serde_json::Value>("/v1/health")
        .await;
    assert!(health.is_ok(), "the API no longer answers: {health:?}");
    server.abort();
}
