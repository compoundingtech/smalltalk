#![cfg(unix)]

//! A seat's harness view as clients read it: the daemon's API over a temporary store, driven by
//! the claims a seat's driver posts, and read back through the store and the `st3` CLI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::{ClaimInput, ClaimRecord};
use st3::store::Store;
use tokio::sync::{Notify, watch};

const SUBJECT: &str = "agent/view-seat";

struct Daemon {
    _root: tempfile::TempDir,
    socket: PathBuf,
    store: Arc<Store>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Daemon {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let store = Arc::new(Store::open_memory("seat-harness-view").unwrap());
        let state = AppState {
            store: store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "seat-harness-view".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&server_socket, st3::api::router(state)).await
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(socket.exists());
        Self {
            _root: root,
            socket,
            store,
            server,
        }
    }

    fn runtime(&self, status: &str, incarnation: &str) {
        self.store
            .append_claim(&ClaimInput {
                subject: SUBJECT.into(),
                kind: "runtime.observed".into(),
                actor: Some(SUBJECT.into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String("view-seat".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("status".into(), Value::String(status.into())),
                    ("reachability".into(), Value::String("reachable".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    /// The claim a native driver posts for one harness observation, through the API.
    async fn post_harness(&self, incarnation: &str, fields: Value) -> anyhow::Result<ClaimRecord> {
        let mut claim = BTreeMap::from([
            ("driver".into(), json!("claude")),
            ("transport".into(), json!("claude-channel")),
            ("incarnation_id".into(), json!(incarnation)),
        ]);
        for (key, value) in fields.as_object().unwrap() {
            claim.insert(key.clone(), value.clone());
        }
        st3::client::Client::unix(&self.socket)
            .post(
                "/v1/claims",
                &ClaimInput {
                    subject: SUBJECT.into(),
                    kind: "harness.observed".into(),
                    actor: Some(SUBJECT.into()),
                    fields: claim,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                },
            )
            .await
    }

    async fn cli(&self, args: &[&str]) -> Output {
        let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
        let socket = self.socket.clone();
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(binary)
                .env_remove("ST_AGENT")
                .env_remove("ST_MISSION_RUN")
                .env_remove("ST3_ENDPOINT")
                .arg("--endpoint")
                .arg(&socket)
                .arg("--json")
                .args(args)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }

    async fn agent(&self) -> Value {
        let output = self.cli(&["agents", "show", SUBJECT]).await;
        assert!(output.status.success(), "{}", stderr(&output));
        let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
        shown.get("value").cloned().unwrap_or(shown)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn socket_exists(path: &Path) -> bool {
    path.exists()
}

/// A seat's harness process exits. Its last observed state must not outlive it: the harness view
/// is cleared and the seat is no longer listed as an operational agent, idle or working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exited_runtime_clears_the_seats_harness_state() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    daemon
        .post_harness("view-seat:i1", json!({"state": "working"}))
        .await
        .unwrap();
    assert_eq!(
        daemon
            .store
            .current_harness(SUBJECT)
            .unwrap()
            .unwrap()
            .state,
        "working"
    );
    let running = daemon.agent().await;
    assert_eq!(running["state"], "running", "{running}");
    assert_eq!(running["harness_state"], "working", "{running}");

    daemon.runtime("exited", "view-seat:i1");
    assert!(daemon.store.current_harness(SUBJECT).unwrap().is_none());
    let listed = daemon.cli(&["agents", "ls"]).await;
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains(SUBJECT),
        "an exited seat is still listed as operational: {}",
        String::from_utf8_lossy(&listed.stdout)
    );
    let shown = daemon.cli(&["agents", "show", SUBJECT]).await;
    assert!(!shown.status.success());
    assert!(
        stderr(&shown).contains("not operational"),
        "{}",
        stderr(&shown)
    );
}

/// A harness observation from a superseded incarnation can never become the seat's state. The
/// daemon must refuse it rather than acknowledge a report it will not apply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: a harness report for a superseded incarnation is acknowledged and silently ignored"]
async fn a_harness_report_for_a_superseded_incarnation_is_refused() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    daemon.runtime("running", "view-seat:i2");
    let posted = daemon
        .post_harness("view-seat:i1", json!({"state": "working"}))
        .await;
    assert!(daemon.store.current_harness(SUBJECT).unwrap().is_none());
    assert!(
        posted.is_err(),
        "the report was acknowledged as {:?} but is not the seat's state",
        posted.map(|claim| claim.id)
    );
}

/// A seat whose harness is stopped on a permission prompt is waiting on a person. The agent a
/// client lists must say so, not show an ordinary running seat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: the agent resource shows a seat at a permission prompt as running/working"]
async fn a_seat_at_a_permission_prompt_is_listed_as_waiting_on_a_person() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    daemon
        .post_harness("view-seat:i1", json!({"state": "ready"}))
        .await
        .unwrap();
    daemon
        .post_harness(
            "view-seat:i1",
            json!({
                "state": "working",
                "blocked_on": "human",
                "ask": "permission",
                "reason": "permissionRequest",
                "input_buffer": "unknown",
                "exit": null,
            }),
        )
        .await
        .unwrap();
    let view = daemon.store.current_harness(SUBJECT).unwrap().unwrap();
    assert_eq!(view.blocked_on.as_deref(), Some("human"));

    let agent = daemon.agent().await;
    assert!(
        agent["state"] == "waiting" || agent.to_string().contains("\"human\""),
        "the seat waits on a person, but its agent resource reads {agent}"
    );
}

/// `st3 trace wait` on a condition that already holds returns at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_whose_condition_already_holds_returns_at_once() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    let started = Instant::now();
    let output = daemon
        .cli(&[
            "trace",
            "wait",
            SUBJECT,
            "--for",
            "running",
            "--timeout",
            "30s",
        ])
        .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// A wait whose condition never holds ends at its timeout and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_whose_condition_never_holds_ends_at_its_timeout() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    let started = Instant::now();
    let output = daemon
        .cli(&[
            "trace",
            "wait",
            SUBJECT,
            "--for",
            "exited",
            "--timeout",
            "2s",
        ])
        .await;
    let elapsed = started.elapsed();
    assert!(!output.status.success());
    assert!(stderr(&output).contains("timed out"), "{}", stderr(&output));
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(10),
        "{elapsed:?}"
    );
}

/// The daemon goes away while a wait is in flight. The wait must end with an error rather than
/// block until its timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: after the daemon goes away the wait runs to its timeout instead of reporting connection loss"]
async fn a_wait_ends_with_an_error_when_the_daemon_goes_away() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    let started = Instant::now();
    let wait = daemon.cli(&[
        "trace",
        "wait",
        SUBJECT,
        "--for",
        "exited",
        "--timeout",
        "60s",
    ]);
    let abort = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        daemon.server.abort();
        let _ = std::fs::remove_file(&daemon.socket);
        assert!(!socket_exists(&daemon.socket));
    };
    let (output, ()) = tokio::join!(wait, abort);
    assert!(!output.status.success());
    assert!(
        !stderr(&output).contains("timed out"),
        "{}",
        stderr(&output)
    );
    // The in-process server's open long-poll may finish its bounded 30 s window after the
    // listener is gone; the next request then fails.
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "the wait outlived its daemon for {:?}",
        started.elapsed()
    );
    eprintln!(
        "wait ended after {:?}: {}",
        started.elapsed(),
        stderr(&output)
    );
}

/// A wait on a seat the graph has never known cannot be satisfied by waiting. It must say the
/// target is unknown instead of running out its whole timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: a wait on an unknown agent runs to its timeout without reporting the missing target"]
async fn a_wait_on_an_unknown_seat_reports_the_missing_target() {
    let daemon = Daemon::start().await;
    daemon.runtime("running", "view-seat:i1");
    let started = Instant::now();
    let output = daemon
        .cli(&[
            "trace",
            "wait",
            "agent/no-such-seat",
            "--for",
            "running",
            "--timeout",
            "5s",
        ])
        .await;
    let elapsed = started.elapsed();
    let message = stderr(&output);
    assert!(!output.status.success());
    assert!(
        elapsed < Duration::from_secs(5) && !message.contains("timed out"),
        "after {elapsed:?} the wait ended with: {message}"
    );
}
