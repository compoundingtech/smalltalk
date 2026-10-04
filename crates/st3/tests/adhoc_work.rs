//! Start, checkpoint, transfer, acknowledge and close through an isolated durable daemon.
#![cfg(unix)]
use serde_json::{Value, json};
use st3::{api::AppState, model::ClaimInput, store::Store};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Notify, watch};

struct NoRuntime;

impl st3::reconcile::RuntimeControl for NoRuntime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<st3::reconcile::RuntimeObservation>> {
        Ok(Vec::new())
    }
    fn observe_exec(&self, _: &str) -> anyhow::Result<Option<st3::reconcile::RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &st3::model::MemberSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove(&self, _: &str, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

fn finish_run(store: &Arc<Store>, run: &str) {
    let reconciler = st3::reconcile::Reconciler::new(
        store.clone(),
        Arc::new(NoRuntime),
        "example".into(),
        Arc::new(Notify::new()),
    );
    for _ in 0..20 {
        reconciler.reconcile_once().unwrap();
        if store.mission_run(run).unwrap().unwrap().status == "completed" {
            return;
        }
    }
    panic!(
        "minimal work run did not complete: {:?}",
        store.mission_run(run).unwrap()
    );
}

const ALDER: &str = "agent/example/alder";
const BIRCH: &str = "agent/example/birch";

struct Daemon {
    root: PathBuf,
    store: Arc<Store>,
    server: Option<tokio::task::JoinHandle<()>>,
}
impl Daemon {
    fn new(root: &Path) -> Self {
        Self {
            root: root.into(),
            store: Arc::new(Store::open(&root.join("graph.sqlite3"), "example").unwrap()),
            server: None,
        }
    }
    fn socket(&self) -> PathBuf {
        self.root.join("api.sock")
    }
    async fn start(&mut self) {
        let state = AppState {
            store: self.store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "example".into(),
            state_dir: self.root.clone(),
            pty_root: self.root.join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let socket = self.socket();
        self.server = Some(tokio::spawn(async move {
            st3::api::serve_unix(&socket, st3::api::router(state))
                .await
                .unwrap();
        }));
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("isolated daemon did not start");
    }
    async fn restart(&mut self) {
        self.stop().await;
        self.store = Arc::new(Store::open(&self.root.join("graph.sqlite3"), "example").unwrap());
        self.store.replay_replication_graph().unwrap();
        self.start().await;
    }
    async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
    }
    async fn cli(&self, args: &[&str]) -> std::process::Output {
        let mut command =
            st3::test_support::async_command(assert_cmd::cargo::cargo_bin!("st3-fixture"));
        command
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .arg("--endpoint")
            .arg(self.socket())
            .args(["--daemon-wait", "0", "--json"])
            .args(args);
        command.output().await.unwrap()
    }
    async fn ok(&self, args: &[&str]) -> Value {
        let output = self.cli(args).await;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    async fn fails(&self, args: &[&str], message: &str) {
        let output = self.cli(args).await;
        assert!(!output.status.success(), "unexpected success for {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "expected {message}: {stderr}");
    }
    fn declare(&self) {
        let intent = st3::parse_intent("version 2\nagent \"example/alder\" { workspace \"/tmp\"; command \"true\"; }\nagent \"example/birch\" { workspace \"/tmp\"; command \"true\"; }", "example").unwrap();
        self.store.apply_internal(&intent, "seats").unwrap();
        for (actor, incarnation) in [(ALDER, "alder-one"), (BIRCH, "birch-one")] {
            for (kind, fields) in [
                (
                    "runtime.observed",
                    json!({"runtime_id": actor.trim_start_matches("agent/"), "incarnation_id": incarnation, "status": "running", "reachability": "local"}),
                ),
                (
                    "harness.observed",
                    json!({"incarnation_id": incarnation, "state": "ready"}),
                ),
            ] {
                self.store
                    .append_claim(&ClaimInput {
                        subject: actor.into(),
                        kind: kind.into(),
                        actor: None,
                        fields: serde_json::from_value(fields).unwrap(),
                        evidence: vec![],
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
            }
        }
    }
    fn note(&self, recipient: &str) -> String {
        let notes = self.store.messages(Some(recipient), true).unwrap();
        assert_eq!(notes.len(), 1, "one durable note per transfer");
        assert!(notes[0].content.contains("Next: check the fixture"));
        notes[0].subject.clone()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

#[tokio::test]
async fn spontaneous_work_survives_restart_handoff_acknowledgment_and_close() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(root.path());
    daemon.declare();
    daemon.start().await;
    let title = "Inspect ${literal} fixture";
    let start = [
        "work",
        "start",
        title,
        "--as",
        ALDER,
        "--idempotency-key",
        "fixture-start",
    ];
    let opened = daemon.ok(&start).await;
    let step = opened["subject"].as_str().unwrap();
    assert_eq!(opened["title"], title);
    assert_eq!(opened["assigned_to"], ALDER);
    assert_eq!(daemon.ok(&start).await["subject"], step);
    daemon
        .fails(
            &[
                "work",
                "start",
                "Another title",
                "--as",
                ALDER,
                "--idempotency-key",
                "fixture-start",
            ],
            "another task",
        )
        .await;
    daemon
        .ok(&[
            "work",
            "claim",
            step,
            "--as",
            ALDER,
            "--incarnation",
            "alder-one",
        ])
        .await;
    daemon
        .fails(
            &["work", "start", "Independent work", "--as", ALDER],
            "finish or release",
        )
        .await;
    daemon
        .ok(&[
            "work",
            "progress",
            step,
            "--as",
            ALDER,
            "--incarnation",
            "alder-one",
            "--summary",
            "Current: fixture found",
            "--evidence",
            "file:fixture",
        ])
        .await;
    daemon.restart().await;
    assert_eq!(daemon.ok(&start).await["subject"], step);
    let handoff = [
        "work",
        "handoff",
        step,
        "--as",
        ALDER,
        "--incarnation",
        "alder-one",
        "--to",
        BIRCH,
        "--note",
        "Next: check the fixture",
        "--idempotency-key",
        "fixture-handoff",
    ];
    daemon
        .fails(
            &[
                "work",
                "handoff",
                step,
                "--as",
                ALDER,
                "--incarnation",
                "stale",
                "--to",
                BIRCH,
                "--note",
                "Next: check the fixture",
            ],
            "not the current live incarnation",
        )
        .await;
    let transferred = daemon.ok(&handoff).await;
    assert_eq!(transferred["assigned_to"], BIRCH);
    assert!(transferred["claimant"].is_null());
    assert_eq!(daemon.ok(&handoff).await["subject"], step);
    let note = daemon.note(BIRCH);
    daemon
        .ok(&["conversations", "read", &note, "--as", BIRCH])
        .await;
    assert_eq!(daemon.store.message(&note).unwrap().unwrap().status, "read");
    daemon
        .fails(
            &[
                "work",
                "claim",
                step,
                "--as",
                BIRCH,
                "--incarnation",
                "birch-one",
            ],
            "acknowledge",
        )
        .await;
    daemon
        .fails(
            &[
                "work",
                "progress",
                step,
                "--as",
                ALDER,
                "--incarnation",
                "alder-one",
            ],
            "not available",
        )
        .await;
    daemon.restart().await;
    assert_eq!(daemon.note(BIRCH), note);
    let shown = daemon.ok(&["work", "show", step]).await;
    assert!(shown.to_string().contains("Next: check the fixture"));
    daemon
        .fails(
            &[
                "work",
                "acknowledge",
                step,
                "--message",
                &note,
                "--as",
                ALDER,
            ],
            "current recipient",
        )
        .await;
    daemon
        .fails(
            &[
                "work",
                "acknowledge",
                step,
                "--message",
                "message/old",
                "--as",
                BIRCH,
            ],
            "current recipient",
        )
        .await;
    daemon
        .ok(&[
            "work",
            "acknowledge",
            step,
            "--message",
            &note,
            "--as",
            BIRCH,
        ])
        .await;
    daemon
        .ok(&[
            "work",
            "acknowledge",
            step,
            "--message",
            &note,
            "--as",
            BIRCH,
        ])
        .await;
    assert_eq!(
        daemon
            .store
            .claims_for(&note, Some("message.read"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(daemon.store.message(&note).unwrap().unwrap().status, "read");
    daemon.restart().await;
    assert!(
        daemon
            .ok(&["work", "show", step])
            .await
            .to_string()
            .contains("(acknowledged)")
    );
    daemon
        .ok(&[
            "work",
            "claim",
            step,
            "--as",
            BIRCH,
            "--incarnation",
            "birch-one",
        ])
        .await;
    daemon
        .fails(
            &[
                "work",
                "complete",
                step,
                "--as",
                BIRCH,
                "--incarnation",
                "birch-one",
            ],
            "evidence",
        )
        .await;
    let submitted = daemon
        .ok(&[
            "work",
            "complete",
            step,
            "--as",
            BIRCH,
            "--incarnation",
            "birch-one",
            "--summary",
            "Fixture checked",
            "--evidence",
            "file:verified-fixture",
        ])
        .await;
    assert_eq!(submitted["status"], "verifying");
    finish_run(&daemon.store, opened["run"].as_str().unwrap());
    daemon.restart().await;
    let run = daemon
        .store
        .mission_run(opened["run"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "completed");
    assert!(
        run.steps
            .iter()
            .all(|s| s.status == "completed" && s.claimant.is_none())
    );
    let evidence = daemon
        .store
        .claims_for(step, Some("work.submitted"))
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].body["evidence"],
        json!(["file:verified-fixture"])
    );
    daemon.stop().await;
}

#[tokio::test]
async fn person_handoff_is_visible_acknowledged_and_closed_with_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(root.path());
    daemon.declare();
    daemon.start().await;
    let opened = daemon
        .ok(&["work", "start", "Review fixture", "--as", ALDER])
        .await;
    let step = opened["subject"].as_str().unwrap();
    daemon
        .ok(&[
            "work",
            "claim",
            step,
            "--as",
            ALDER,
            "--incarnation",
            "alder-one",
        ])
        .await;
    daemon
        .ok(&[
            "work",
            "handoff",
            step,
            "--as",
            ALDER,
            "--incarnation",
            "alder-one",
            "--to",
            "person/avery",
            "--note",
            "Next: check the fixture",
        ])
        .await;
    let note = daemon.note("person/avery");
    let attention = daemon.store.attention_items(Some("person/avery")).unwrap();
    assert!(attention.iter().any(|item| item.subject == step));
    daemon
        .fails(
            &[
                "work",
                "done",
                step,
                "--as",
                "person/avery",
                "--summary",
                "Reviewed",
                "--evidence",
                "file:review",
            ],
            "acknowledge",
        )
        .await;
    daemon.restart().await;
    daemon
        .ok(&[
            "work",
            "acknowledge",
            step,
            "--message",
            &note,
            "--as",
            "person/avery",
        ])
        .await;
    daemon
        .fails(
            &[
                "work",
                "done",
                step,
                "--as",
                "person/avery",
                "--summary",
                "Reviewed",
            ],
            "evidence",
        )
        .await;
    daemon
        .ok(&[
            "work",
            "done",
            step,
            "--as",
            "person/avery",
            "--summary",
            "Reviewed",
            "--evidence",
            "file:review",
        ])
        .await;
    finish_run(&daemon.store, opened["run"].as_str().unwrap());
    daemon.restart().await;
    assert_eq!(
        daemon
            .store
            .mission_run(opened["run"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    assert!(
        daemon
            .store
            .attention_items(Some("person/avery"))
            .unwrap()
            .is_empty()
    );
    daemon.stop().await;
}
