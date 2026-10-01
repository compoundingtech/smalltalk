#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::{ClaimRecord, MemberSpec, MissionRunRequest};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use tokio::sync::{Notify, watch};

/// Only this fixture's runtimes exist here; no shared seats or daemons are touched.
#[derive(Default)]
struct Runtime {
    observations: Mutex<HashMap<String, RuntimeObservation>>,
    starts: Mutex<Vec<MemberSpec>>,
    stops: Mutex<Vec<String>>,
    refuse_start: Mutex<bool>,
}
impl RuntimeControl for Runtime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<RuntimeObservation>> {
        Ok(self
            .observations
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect())
    }
    fn observe_exec(&self, id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        Ok(self.observations.lock().unwrap().get(id).cloned())
    }
    fn start(&self, member: &MemberSpec) -> anyhow::Result<()> {
        anyhow::ensure!(
            !*self.refuse_start.lock().unwrap(),
            "fixture rejected launch"
        );
        let mut starts = self.starts.lock().unwrap();
        starts.push(member.clone());
        self.observations.lock().unwrap().insert(
            member.runtime_id.clone(),
            RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: member.terminal,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(format!("fixture:{}", starts.len())),
            },
        );
        Ok(())
    }
    fn stop(&self, id: &str, _: bool, incarnation: Option<&str>) -> anyhow::Result<()> {
        let mut observations = self.observations.lock().unwrap();
        let current = observations.get_mut(id).unwrap();
        assert_eq!(current.incarnation_id.as_deref(), incarnation);
        self.stops.lock().unwrap().push(incarnation.unwrap().into());
        current.status = "exited".into();
        current.exit_code = Some(0);
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        panic!("fixture stops immediately")
    }
    fn remove(&self, id: &str, _: bool) -> anyhow::Result<()> {
        self.observations.lock().unwrap().remove(id);
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

struct Fixture {
    root: tempfile::TempDir,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    reconciler: Arc<Reconciler<Runtime>>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Fixture {
    async fn new(mission: bool) -> (Self, String) {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), "restart-test").unwrap());
        let notify = Arc::new(Notify::new());
        let runtime = Arc::new(Runtime::default());
        let reconciler = Arc::new(Reconciler::new(
            store.clone(),
            runtime.clone(),
            "restart-test".into(),
            notify.clone(),
        ));
        let member = format!(
            r#"agent "example/worker" {{
            workspace {:?}
            name "Fixture worker"
            command "sleep 1000"
            restart never
            shutdown-timeout "1s"
            env {{ ORIGINAL "kept" }}
        }}"#,
            root.path().to_str().unwrap()
        );
        let source = if mission {
            // The same seat shape is materialized by a mission rather than a root declaration.
            format!(
                "version 2\nmission \"example/restart\" state=\"ready\" {{\ngoal \"Exercise a seat restart.\"\n{}\nstep \"work\" {{ assigned-to \"agent/${{ST_MISSION_RUN}}/worker\"; goal \"Keep the run open.\" }}\n}}",
                member.replace("example/worker", "worker")
            )
        } else {
            format!("version 2\n{member}")
        };
        let intent = st3::parse_intent(&source, "restart-test").unwrap();
        let preview = store
            .mission(
                &intent,
                st3::model::IntentInput {
                    kdl: source.clone(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                "declare",
                Some("person/avery"),
            )
            .unwrap();
        let subject = if mission {
            let run = store
                .create_mission_run(&MissionRunRequest {
                    mission: "example/restart".into(),
                    revision: None,
                    workspace: root.path().to_str().unwrap().into(),
                    requester: Some("person/avery".into()),
                    mode: None,
                    inputs: Default::default(),
                    idempotency_key: "run".into(),
                })
                .unwrap();
            reconciler.reconcile_once().unwrap();
            format!("agent/{}/worker", run.id)
        } else {
            "agent/example/worker".into()
        };
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let state = AppState {
            store: store.clone(),
            notify,
            event_notify: watch::channel(0).0,
            node: "restart-test".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        };
        let socket = root.path().join("st3.sock");
        let server =
            tokio::spawn(
                async move { st3::api::serve_unix(&socket, st3::api::router(state)).await },
            );
        for _ in 0..100 {
            if root.path().join("st3.sock").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        (
            Self {
                root,
                store,
                runtime,
                reconciler,
                server,
            },
            subject,
        )
    }
    fn client(&self) -> st3::client::Client {
        st3::client::Client::unix(self.root.path().join("st3.sock"))
    }
    fn drive(&self) -> tokio::task::JoinHandle<()> {
        let reconciler = self.reconciler.clone();
        tokio::spawn(async move {
            loop {
                reconciler.reconcile_once().unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    }
    async fn request(&self, subject: &str, key: &str) -> ClaimRecord {
        self.client()
            .post(
                "/v1/agents/restart",
                &json!({"subject": subject, "actor":"person/avery", "idempotency_key":key}),
            )
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn cli(socket: &Path, subject: &str, actor: &str, timeout: &str) -> std::process::Output {
    let socket = socket.to_owned();
    let subject = subject.to_owned();
    let actor = actor.to_owned();
    let timeout = timeout.to_owned();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"))
            .env_remove("ST_AGENT")
            .env_remove("ST_MISSION_RUN")
            .args([
                "--endpoint",
                socket.to_str().unwrap(),
                "--json",
                "agents",
                "restart",
                &subject,
                "--as",
                &actor,
                "--timeout",
                &timeout,
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}
async fn restarts_preserving_declaration(mission: bool) {
    let (fixture, subject) = Fixture::new(mission).await;
    let before = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap();
    assert_eq!(before.0.owner_run.is_some(), mission);
    let token = fixture.store.selected_desired_token(&subject).unwrap();
    let driver = fixture.drive();
    // Free mode gives agents the same restart authority as people. Test both actors and both
    // accepted subject forms, then let another pass run to detect an accidental repeat.
    for (actor, target, incarnation) in [
        ("person/avery", subject.as_str(), "fixture:2"),
        (
            "agent/example/operator",
            subject.trim_start_matches("agent/"),
            "fixture:3",
        ),
    ] {
        let output = cli(&fixture.root.path().join("st3.sock"), target, actor, "3s").await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let agent: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(agent["incarnation_id"], incarnation);
        assert_eq!(agent["state"], "running");
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    driver.abort();
    let _ = driver.await;
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 3);
    assert_eq!(
        *fixture.runtime.stops.lock().unwrap(),
        ["fixture:1", "fixture:2"]
    );
    let after = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(before.0).unwrap(),
        serde_json::to_value(after.0).unwrap()
    );
    assert_eq!(before.1, after.1);
    assert_eq!(
        token,
        fixture.store.selected_desired_token(&subject).unwrap()
    );
    // Restart never still governs the replacement after the explicit request has launched it.
    fixture
        .runtime
        .observations
        .lock()
        .unwrap()
        .values_mut()
        .for_each(|item| item.status = "exited".into());
    fixture.reconciler.reconcile_once().unwrap();
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 3);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarts_top_level_seat_on_a_new_incarnation() {
    restarts_preserving_declaration(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarts_mission_seat_without_redeclaring_it() {
    restarts_preserving_declaration(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_reports_timeout_and_launch_failure() {
    let (fixture, subject) = Fixture::new(false).await;
    let output = cli(
        &fixture.root.path().join("st3.sock"),
        &subject,
        "person/avery",
        "100ms",
    )
    .await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("did not reach a new running incarnation")
    );
    *fixture.runtime.refuse_start.lock().unwrap() = true;
    let driver = fixture.drive();
    let output = cli(
        &fixture.root.path().join("st3.sock"),
        &subject,
        "person/avery",
        "3s",
    )
    .await;
    driver.abort();
    let _ = driver.await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fixture rejected launch"),
        "{output:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_is_idempotent_and_cannot_revive_a_later_stop() {
    let (fixture, subject) = Fixture::new(false).await;
    let first = fixture.request(&subject, "same").await;
    assert_eq!(first.id, fixture.request(&subject, "same").await.id);
    let export = fixture.store.export_replication(0).unwrap();
    assert!(
        export
            .batches
            .iter()
            .flat_map(|batch| &batch.claims)
            .any(|claim| claim.id == first.id),
        "request must replicate to the owner"
    );
    let intent =
        st3::parse_intent(&format!("version 2\nstop {subject:?}"), "restart-test").unwrap();
    let preview = fixture
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: format!("version 2\nstop {subject:?}"),
                source_name: None,
            },
        )
        .unwrap();
    fixture
        .store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "stop",
            Some("person/avery"),
        )
        .unwrap();
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 1);
    let rejected: Result<ClaimRecord, _> = fixture
        .client()
        .post(
            "/v1/agents/restart",
            &json!({"subject":subject,"actor":"person/avery","idempotency_key":"after-stop"}),
        )
        .await;
    assert!(
        rejected
            .unwrap_err()
            .to_string()
            .contains("active seat declaration")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delayed_restart_does_not_stop_a_newer_incarnation() {
    let (fixture, subject) = Fixture::new(false).await;
    let request = fixture.request(&subject, "delayed").await;
    let member = fixture.runtime.starts.lock().unwrap()[0].clone();
    fixture.runtime.start(&member).unwrap();
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert!(fixture.runtime.stops.lock().unwrap().is_empty());
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 2);
    assert_eq!(request.id, fixture.request(&subject, "delayed").await.id);
}

#[test]
fn restart_help_explains_seats_and_the_new_incarnation() {
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"))
        .env_remove("ST_AGENT")
        .args(["agents", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(
        help.contains("restart") && help.contains("mission seat") && help.contains("incarnation")
    );
}
