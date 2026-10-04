use serde_json::json;
use st3::api::{AppState, router, serve_unix};
use st3::backup;
use st3::client::{Client, Endpoint};
use st3::model::ClaimInput;
use st3::store::Store;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, watch};

fn backup_cli(root: &Path, socket: &Path) -> tokio::process::Command {
    let mut command = st3::test_support::async_command(env!("CARGO_BIN_EXE_st3-fixture"));
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_DATA_HOME", root.join("data"));
    command.args(["--json", "--endpoint"]);
    command.arg(format!("unix://{}", socket.display()));
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_backup_download_restores_and_leaves_the_daemon_serving_reads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("config/st3")).unwrap();
    std::fs::write(
        root.path().join("config/st3/config.toml"),
        r#"node = "alder"
"#,
    )
    .unwrap();
    let store = Arc::new(Store::open(&root.path().join("source.db"), "alder").unwrap());
    for index in 0..1100 {
        store
            .append_claim(&ClaimInput {
                subject: "daemon/alder".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("code".into(), json!("backup-test")),
                    ("reason".into(), json!(format!("note-{index}"))),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let socket = root.path().join("api.sock");
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "alder".into(),
        state_dir: root.path().into(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        serve_unix(&server_socket, router(state)).await.unwrap();
    });
    let endpoint = Endpoint::Unix(socket.clone());
    let client = Client::new(endpoint.clone());
    for _ in 0..100 {
        if client.get::<serde_json::Value>("/v1/health").await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let file = root.path().join("backup.jsonl");
    let mut create = backup_cli(root.path(), &socket);
    create.args(["backup", "create"]).arg(&file);
    let download = tokio::spawn(async move { create.output().await.unwrap() });
    for _ in 0..10 {
        tokio::time::timeout(
            Duration::from_secs(2),
            client.get::<serde_json::Value>("/v1/claims?subject=daemon%2Falder"),
        )
        .await
        .unwrap()
        .unwrap();
    }
    let output = download.await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(backup::create(&endpoint, &file).await.is_err());
    let target = root.path().join("restored.db");
    let output = backup_cli(root.path(), &socket)
        .args(["backup", "restore"])
        .arg(&file)
        .arg("--database")
        .arg(&target)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: backup::RestoreReport = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.projections_match);
    assert_eq!(report.tables["claim_sources"].count, 1100);
    let remote = Store::open_memory("birch").unwrap();
    remote
        .put_document(
            "doc/remote",
            b"A replicated document.\n",
            &None,
            "remote-document",
        )
        .unwrap();
    let exchange = remote
        .export_replication_exchange("test-fleet", &Default::default())
        .unwrap();
    store
        .receive_replication_exchange("birch", "test-fleet", &exchange)
        .unwrap();
    store.validate_replication_backlog().unwrap();
    let pending = root.path().join("pending.jsonl");
    let error = backup::create(&endpoint, &pending).await.unwrap_err();
    assert!(error.to_string().contains("fully projected"), "{error:#}");
    assert!(!pending.exists());
    store.project_replication_backlog().unwrap();
    server.abort();
}
