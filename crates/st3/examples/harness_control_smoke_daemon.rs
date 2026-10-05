use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use serde_json::json;
use st3::{api::{AppState, router, serve_unix_bound}, model::{ClaimInput, PlannerSpec}, store::Store};
use tokio::sync::{Notify, watch};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("isolated root"));
    std::fs::create_dir_all(&root)?;
    let store = Arc::new(Store::open(&root.join("owner.sqlite"), "queue-smoke")?);
    let intent = st3::graph::parse_intent(&format!("version 2\nagent \"control-smoke\" {{ workspace {:?}; harness \"omp\" {{ model \"control-smoke/native-smoke\"; }} }}", root.display().to_string()), "queue-smoke")?;
    let planned = store.mission(&intent, st3::model::IntentInput { kdl: String::new(), source_name: None })?;
    store.apply_as(&intent, &planned.subject_tokens, "native-queue-smoke-declaration", Some("person/operator"))?;
    let incarnation = if std::env::var_os("SMOKE_REAL_PTY").is_some() {
        let runtime = st_runtime::PtyRuntime::new(root.join("pty"));
        let observed = runtime.snapshot()?.into_iter().find(|row| row.name == "native-smoke").ok_or_else(|| anyhow::anyhow!("isolated native-smoke PTY must start before the daemon"))?;
        format!("{}:{}", observed.pid.ok_or_else(|| anyhow::anyhow!("PTY has no process identity"))?, observed.created_at.ok_or_else(|| anyhow::anyhow!("PTY has no creation identity"))?)
    } else {
        "incarnation-smoke".into()
    };
    store.append_claim(&ClaimInput { subject: "agent/queue-smoke.control-smoke".into(), kind: "runtime.observed".into(), actor: Some("agent/queue-smoke.control-smoke".into()), fields: BTreeMap::from([("status".into(), json!("running")), ("incarnation_id".into(), json!(incarnation)), ("runtime_id".into(), json!("native-smoke")), ("driver".into(), json!("omp"))]), evidence: Vec::new(), expected_subject: None, idempotency_key: None })?;
    let socket = root.join("daemon.sock");
    let state = AppState { store, notify: Arc::new(Notify::new()), event_notify: watch::channel(0).0, node: "queue-smoke".into(), state_dir: root.clone(), pty_root: root.join("pty"), pty_binary: PathBuf::from("pty"), fleet_id: None, configured_peers: Vec::new(), client_relay: None, native_session_home: std::env::var_os("SMOKE_REAL_PTY").map(|_| root.clone()), planner_default: PlannerSpec::default() };
    st3::api::start_native_session_discovery(&state);
    if let Some(client_socket) = std::env::var_os("ST_SMOKE_CLIENT_SOCKET") {
        let client_socket = PathBuf::from(client_socket);
        let client_router = st3::api::fabric_router(state.clone());
        tokio::spawn(async move { st3::api::serve_unix(&client_socket, client_router).await.expect("isolated paired client listener"); });
    }
    println!("SMOKE_DAEMON_READY {}", socket.display());
    serve_unix_bound(&socket, &root.join("state.sock"), router(state)).await
}
