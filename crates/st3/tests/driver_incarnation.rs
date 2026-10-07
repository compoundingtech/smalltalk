#![cfg(unix)]
//! The driver exports its runtime fence before spawning a provider.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use st3::api::AppState;
use st3::model::{ClaimInput, PlannerSpec};
use st3::store::Store;
use tokio::sync::{Notify, watch};

async fn provider_incarnation(
    inherited: Option<&str>,
    resume: bool,
    subject: &'static str,
    expected: &str,
) {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("api.sock");
    let store = Arc::new(Store::open_memory("orchid").unwrap());
    store
        .append_claim(&ClaimInput {
            subject: "agent/garden/worker".into(),
            kind: "runtime.observed".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("status".into(), json!("running")),
                ("incarnation_id".into(), json!("worker:current")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let state = AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "orchid".into(),
        state_dir: root.path().into(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let listener_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&listener_socket, st3::api::router(state)).await
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let resume_path =
        resume.then(|| {
            st_drivers::reexec::write_state(root.path(), "driver", &json!({
        "driver":"exec", "subject":"agent/garden/worker", "incarnation":"worker:current",
        "session":{"kind":"provider", "pid":0, "session":"provider-session", "seq":0},
        "loop_state":{"ready":false, "harness_record_started":false,
            "published_timeline":[], "delivery_episode":0}
    })).unwrap()
        });
    let startup_resume_path = resume_path.clone();
    let inherited = inherited.map(str::to_owned);
    let socket_arg = socket.clone();
    let home = root.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        let mut command = st3::test_support::command(test_env!("CARGO_BIN_EXE_st3-fixture"));
        command
            .env_clear()
            .env("PTY_ROOT", home.join("pty"))
            .env("HOME", home);
        if let Some(incarnation) = inherited {
            command.env("ST3_INCARNATION", incarnation);
        }
        if let Some(path) = startup_resume_path {
            command.env(st_drivers::reexec::DRIVER_RESUME_ENV, path);
        }
        command
            .arg("--endpoint")
            .arg(socket_arg)
            .args([
                "driver",
                "exec",
                "--subject",
                subject,
                "--",
                env!("ST3_FIXTURE_BASH"),
                "--noprofile",
                "--norc",
                "-c",
                "printf '%s' \"$ST3_INCARNATION\"",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    server.abort();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, expected.as_bytes());
    if let Some(path) = resume_path {
        assert!(
            path.exists(),
            "startup must leave the adoption record for the driver"
        );
    }
}

#[tokio::test]
async fn fresh_driver_exports_its_incarnation_in_the_provider_environment() {
    if st3::test_support::supervise_test() {
        return;
    }
    provider_incarnation(None, false, "agent/garden/worker", "worker:current").await;
}

#[tokio::test]
async fn fresh_driver_replaces_an_inherited_incarnation() {
    if st3::test_support::supervise_test() {
        return;
    }
    provider_incarnation(
        Some("worker:predecessor"),
        false,
        "agent/garden/worker",
        "worker:current",
    )
    .await;
}

#[tokio::test]
async fn reexecuted_driver_refreshes_its_environment_from_the_saved_runtime_fence() {
    if st3::test_support::supervise_test() {
        return;
    }
    provider_incarnation(
        Some("worker:predecessor"),
        true,
        "agent/garden/worker",
        "worker:current",
    )
    .await;
}

#[tokio::test]
async fn exec_gates_start_without_an_agent_runtime_fence() {
    if st3::test_support::supervise_test() {
        return;
    }
    provider_incarnation(None, false, "gate-operation/catalog/check", "").await;
}
