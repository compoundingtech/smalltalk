//! Serve an offline store copy for read surveys, without drivers, reconciliation or peers.
//! Copy the source with SQLite's backup API before invoking this program. Never pass a live
//! store: Store::open may migrate it. Responses can contain private data; keep probe logs private.
use st3::{api::AppState, store::Store};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{Notify, watch};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("offline scratch directory"));
    let database = root.join("claims.sqlite3");
    anyhow::ensure!(database.is_file(), "create an offline store copy first");
    st3::profile::init_from_env();
    let store = Arc::new(Store::open(&database, "survey-host")?);
    println!("store_index={}", store.index()?);
    let state = AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "survey-host".into(),
        state_dir: root.clone(),
        pty_root: root.join("pty"),
        pty_binary: "/bin/false".into(),
        fleet_id: Some("7c1e2d3f-4a5b-4c6d-8e9f-0a1b2c3d4e5f".into()),
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: Some(root.join("home")),
        planner_default: Default::default(),
    };
    st3::api::start_operation_report(&state);
    st3::api::serve_unix(&root.join("st3.sock"), st3::api::router(state)).await?;
    Ok(())
}
