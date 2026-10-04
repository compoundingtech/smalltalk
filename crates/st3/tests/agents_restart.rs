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
    state: AppState,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    reconciler: Arc<Reconciler<Runtime>>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}
/// Where the fixture's seat is declared.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    TopLevel,
    /// In the mission body, materialized once per generation.
    Mission,
    /// Inside the step it works, materialized on every pass while the step is active.
    Step,
}

impl Fixture {
    async fn new(mission: bool) -> (Self, String) {
        Self::shaped(if mission {
            Shape::Mission
        } else {
            Shape::TopLevel
        })
        .await
    }

    async fn shaped(shape: Shape) -> (Self, String) {
        Self::launching(shape, r#"command "sleep 1000""#, "never").await
    }

    /// A seat whose harness is the typed Claude harness, which continues its native session.
    async fn claude(shape: Shape, restart: &str) -> (Self, String) {
        Self::launching(
            shape,
            r#"harness "claude" { model "example-model"; }"#,
            restart,
        )
        .await
    }

    async fn launching(shape: Shape, launch: &str, restart: &str) -> (Self, String) {
        let mission = shape != Shape::TopLevel;
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
            {launch}
            restart {restart}
            shutdown-timeout "1s"
            env {{ ORIGINAL "kept" }}
        }}"#,
            root.path().to_str().unwrap()
        );
        let source = match shape {
            // The same seat shape is materialized by a mission rather than a root declaration.
            Shape::Mission => format!(
                "version 2\nmission \"example/restart\" state=\"ready\" {{\ngoal \"Exercise a seat restart.\"\n{}\nstep \"work\" {{ assigned-to \"agent/${{ST_MISSION_RUN}}/worker\"; goal \"Keep the run open.\" }}\n}}",
                member.replace("example/worker", "worker")
            ),
            Shape::Step => format!(
                "version 2\nmission \"example/restart\" state=\"ready\" {{\ngoal \"Exercise a seat restart.\"\nstep \"work\" {{\nassigned-to \"agent/${{ST_MISSION_RUN}}/worker\"\ngoal \"Keep the run open.\"\n{}\n}}\n}}",
                member.replace("example/worker", "worker")
            ),
            Shape::TopLevel => format!("version 2\n{member}"),
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
        let router = st3::api::router(state.clone());
        let server = tokio::spawn(async move { st3::api::serve_unix(&socket, router).await });
        for _ in 0..100 {
            if root.path().join("st3.sock").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        (
            Self {
                root,
                state,
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
async fn st(socket: &Path, args: &[&str]) -> std::process::Output {
    let socket = socket.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
            .env_remove("ST_AGENT")
            .env_remove("ST_MISSION_RUN")
            .args(["--endpoint", socket.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}
async fn cli(socket: &Path, subject: &str, actor: &str, timeout: &str) -> std::process::Output {
    let socket = socket.to_owned();
    let subject = subject.to_owned();
    let actor = actor.to_owned();
    let timeout = timeout.to_owned();
    tokio::task::spawn_blocking(move || {
        st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
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
    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
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

fn agent_subjects(store: &Store) -> Vec<String> {
    store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .map(|desired| desired.subject)
        .filter(|subject| subject.starts_with("agent/"))
        .collect()
}

fn succeeded(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A daemon that starts again keeps no memory of what its runs already declared.
fn restarted_daemon(fixture: &Fixture) -> Reconciler<Runtime> {
    Reconciler::new(
        fixture.store.clone(),
        fixture.runtime.clone(),
        "restart-test".into(),
        Arc::new(Notify::new()),
    )
}

async fn stopped_mission_seat_stays_stopped(shape: Shape) {
    let (fixture, subject) = Fixture::shaped(shape).await;
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 1);
    let socket = fixture.root.path().join("st3.sock");
    succeeded(
        &st(
            &socket,
            &["agents", "stop", &subject, "--as", "person/avery"],
        )
        .await,
    );
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(*fixture.runtime.stops.lock().unwrap(), ["fixture:1"]);
    let daemon = restarted_daemon(&fixture);
    for _ in 0..3 {
        daemon.reconcile_once().unwrap();
    }
    assert_eq!(
        fixture.runtime.starts.lock().unwrap().len(),
        1,
        "the seat came back after it was stopped"
    );
    assert_eq!(
        fixture
            .store
            .selected_desired_kind(&subject)
            .unwrap()
            .as_deref(),
        Some("stop")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_mission_seat_stays_stopped_when_the_daemon_restarts() {
    stopped_mission_seat_stays_stopped(Shape::Mission).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_step_seat_stays_stopped() {
    stopped_mission_seat_stays_stopped(Shape::Step).await;
}

async fn starts_a_stopped_mission_seat_on_its_own_declaration(shape: Shape) {
    let (fixture, subject) = Fixture::shaped(shape).await;
    let socket = fixture.root.path().join("st3.sock");
    let before = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap()
        .0;
    let seats = agent_subjects(&fixture.store);
    assert_eq!(seats, [subject.clone()]);
    // Starting a running mission seat changes nothing and says how to relaunch it.
    let running = succeeded(
        &st(
            &socket,
            &["agents", "start", &subject, "--as", "person/avery"],
        )
        .await,
    );
    assert!(running.contains("st agents restart"), "{running}");
    succeeded(
        &st(
            &socket,
            &["agents", "stop", &subject, "--as", "person/avery"],
        )
        .await,
    );
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    // A mission seat's declaration cannot be patched from the command line.
    let patched = st(
        &socket,
        &[
            "agents",
            "start",
            &subject,
            "--workspace",
            "/elsewhere",
            "--as",
            "person/avery",
        ],
    )
    .await;
    assert!(!patched.status.success());
    assert!(String::from_utf8_lossy(&patched.stderr).contains("mission seat"));
    // Both accepted spellings of the identity name the same seat.
    succeeded(
        &st(
            &socket,
            &[
                "agents",
                "start",
                subject.trim_start_matches("agent/"),
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    let after = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    assert_eq!(
        agent_subjects(&fixture.store),
        seats,
        "start created another seat"
    );
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].workspace, starts[1].workspace);
    assert_eq!(starts[0].environment, starts[1].environment);
    // Once started again, the seat is the run's own: a restarted daemon keeps it running.
    let daemon = restarted_daemon(&fixture);
    for _ in 0..3 {
        daemon.reconcile_once().unwrap();
    }
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_a_stopped_mission_seat_restores_its_declaration_and_creates_nothing_else() {
    starts_a_stopped_mission_seat_on_its_own_declaration(Shape::Mission).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_a_stopped_step_seat_restores_its_declaration() {
    starts_a_stopped_mission_seat_on_its_own_declaration(Shape::Step).await;
}

fn github_observer_running(store: &Store, thread: &st3::github_watch::ThreadRef) -> bool {
    store.desired_subjects_named(&[thread.observer()]).unwrap().into_iter().next()
        .and_then(|desired| st3::graph::observer_spec(&desired.desired))
        .is_some_and(|spec| !spec.stopped)
}

/// CLI stop/start and an observation while stopped against this fixture's isolated daemon.
/// The native mailbox stream proof lives beside its authenticated API in api/mailbox.rs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_then_start_keeps_a_github_watch_and_its_queued_wake() {
    // A mission seat also exercises restoring its original owning declaration.
    let (fixture, subject) = Fixture::new(true).await;
    let thread = st3::github_watch::ThreadRef::parse("acme/garden#12").unwrap();
    let watch = thread.watch(&subject);
    fixture
        .store
        .declare_watch(&thread, &subject, None)
        .unwrap();
    let began = fixture.store.watch_view(&watch).unwrap().unwrap()["since"].clone();
    let observe = |comments: Value| {
        let subscriptions = fixture
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .filter_map(|desired| {
                st3::graph::subscription_spec(&desired.desired)
                    .filter(|spec| !spec.stopped)
                    .map(|spec| (desired.subject, spec))
            })
            .collect::<Vec<_>>();
        fixture
            .store
            .record_resource_observation(
                &thread.observer(),
                &fixture
                    .store
                    .selected_desired_revision(&thread.observer())
                    .unwrap()
                    .unwrap(),
                None,
                &thread.resource(),
                None,
                &json!({"repository_id": 7, "issues": [{"number": 12, "new": false,
                "state": "open", "title": "Example thread", "recent_comments": comments}]}),
                0,
                &subscriptions,
            )
            .unwrap();
    };
    observe(json!([]));
    let socket = fixture.root.path().join("st3.sock");
    succeeded(
        &st(
            &socket,
            &["agents", "stop", &subject, "--as", "person/avery"],
        )
        .await,
    );
    fixture.reconciler.reconcile_once().unwrap();
    assert_eq!(
        fixture.store.watch_view(&watch).unwrap().unwrap()["state"],
        "active"
    );
    assert!(github_observer_running(&fixture.store, &thread));
    observe(
        json!([{"kind": "comment", "id": 91, "author": "fern-example",
        "at": chrono::Utc::now().to_rfc3339()}]),
    );
    let queued = fixture
        .store
        .messages(Some(&subject), false)
        .unwrap()
        .into_iter()
        .filter(|message| message.tags.iter().any(|tag| tag == "github-watch"))
        .collect::<Vec<_>>();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].status, "sent");

    // A restarted daemon still sees a stopped declaration owning a live watch.
    let daemon = restarted_daemon(&fixture);
    daemon.reconcile_once().unwrap();
    assert_eq!(
        fixture.store.watch_view(&watch).unwrap().unwrap()["state"],
        "active"
    );
    succeeded(
        &st(
            &socket,
            &["agents", "start", &subject, "--as", "person/avery"],
        )
        .await,
    );
    daemon.reconcile_once().unwrap();
    assert_eq!(
        fixture.store.watch_view(&watch).unwrap().unwrap()["since"],
        began
    );
    assert_eq!(
        fixture.store.watch_view(&watch).unwrap().unwrap()["state"],
        "active"
    );
    let after = fixture.store.messages(Some(&subject), false).unwrap();
    assert!(
        after
            .iter()
            .any(|message| message.subject == queued[0].subject && message.status == "sent")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_retired_seats_declaration_ends_its_watch_and_stops_the_last_observer() {
    let (fixture, subject) = Fixture::new(true).await;
    let thread = st3::github_watch::ThreadRef::parse("acme/garden#12").unwrap();
    fixture
        .store
        .declare_watch(&thread, &subject, None)
        .unwrap();
    let run = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap()
        .0
        .owner_run
        .unwrap();
    fixture
        .store
        .set_mission_run_state(
            run.trim_start_matches("mission-run/"),
            "completed",
            "terminal",
            None,
        )
        .unwrap();
    fixture.reconciler.reconcile_once().unwrap();
    assert_eq!(
        fixture
            .store
            .watch_view(&thread.watch(&subject))
            .unwrap()
            .unwrap()["state"],
        "active",
        "ending the run and stopping the seat keeps its declared conversation"
    );
    fixture.store.discard_desired_owned_by(&run).unwrap();
    fixture.reconciler.reconcile_once().unwrap();
    let view = fixture
        .store
        .watch_view(&thread.watch(&subject))
        .unwrap()
        .unwrap();
    assert_eq!(view["state"], "ended");
    assert_eq!(view["ended"], "seat-ended");
    assert!(!github_observer_running(&fixture.store, &thread));
    assert!(
        fixture
            .store
            .messages(Some(&subject), false)
            .unwrap()
            .into_iter()
            .all(|message| !message.tags.iter().any(|tag| tag == "github-watch"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_run_cannot_start_its_seat_again() {
    let (fixture, subject) = Fixture::new(true).await;
    let socket = fixture.root.path().join("st3.sock");
    succeeded(
        &st(
            &socket,
            &["agents", "stop", &subject, "--as", "person/avery"],
        )
        .await,
    );
    let run = fixture.store.mission_runs().unwrap()[0].subject.clone();
    fixture
        .store
        .set_mission_run_state(
            run.trim_start_matches("mission-run/"),
            "cancelled",
            "terminal",
            None,
        )
        .unwrap();
    let output = st(
        &socket,
        &["agents", "start", &subject, "--as", "person/avery"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot start again"),
        "{output:?}"
    );
    assert_eq!(agent_subjects(&fixture.store), [subject]);
}

/// Send a typed client action as a person, the way stui and the phone do, fenced on the seat's
/// selected declaration.
async fn client_action(fixture: &Fixture, kind: &str, agent: &str, key: &str) -> Value {
    use tower::ServiceExt as _;
    let app = st3::api::router(fixture.state.clone());
    let read = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/client/machines")
                .header("x-st3-person", "person/avery")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let read: Value = serde_json::from_slice(
        &axum::body::to_bytes(read.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let token = fixture
        .store
        .selected_desired_token(agent)
        .unwrap()
        .unwrap();
    let action = json!({
        "api_version": "st3.client.v0",
        "id": format!("action/{key}"),
        "type": kind,
        "idempotency_key": key,
        "fence": {"snapshot_id": read["snapshot"]["id"], "runtime_desired_revision": token},
        "parameters": {"agent": agent},
    });
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/client/actions")
                .header("x-st3-person", "person/avery")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&action).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(status.is_success(), "{kind}: {status} {value}");
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_stops_and_starts_a_mission_seat_on_its_own_declaration() {
    let (fixture, subject) = Fixture::new(true).await;
    let before = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap()
        .0;
    client_action(
        &fixture,
        "agent.stop",
        &subject,
        "client-stop-mission-seat-1",
    )
    .await;
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(*fixture.runtime.stops.lock().unwrap(), ["fixture:1"]);
    let daemon = restarted_daemon(&fixture);
    daemon.reconcile_once().unwrap();
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 1);
    client_action(
        &fixture,
        "agent.start",
        &subject,
        "client-start-mission-seat-1",
    )
    .await;
    for _ in 0..3 {
        daemon.reconcile_once().unwrap();
    }
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 2);
    let after = fixture
        .store
        .desired_subject_with_writer(&subject)
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    assert_eq!(agent_subjects(&fixture.store), [subject]);
}

/// Report the native session the fixture's harness bound for `incarnation`, as its driver does.
async fn bind_session(fixture: &Fixture, subject: &str, incarnation: &str, session: &str) {
    let _: Value = fixture
        .client()
        .post(
            "/v1/agents/native-session",
            &json!({
                "subject": subject,
                "actor": subject,
                "incarnation_id": incarnation,
                "harness": "claude",
                "session_id": session,
                "path": format!("/claude/projects/-example/{session}.jsonl"),
            }),
        )
        .await
        .unwrap();
}

fn continued(member: &MemberSpec) -> Option<&str> {
    member
        .environment
        .get("ST3_NATIVE_CONTINUE_SESSION")
        .map(String::as_str)
}

fn launch(member: &MemberSpec) -> Vec<String> {
    match &member.launch {
        st3::model::LaunchSpec::Argv(argv) => argv.clone(),
        st3::model::LaunchSpec::Shell(source) => vec![source.clone()],
    }
}

/// The harness ends on its own, as when its terminal hangs up.
fn hang_up(fixture: &Fixture) {
    fixture
        .runtime
        .observations
        .lock()
        .unwrap()
        .values_mut()
        .for_each(|item| {
            item.status = "exited".into();
            item.exit_code = Some(1);
        });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_workspace_change_restarts_a_running_seat() {
    let (fixture, subject) = Fixture::new(false).await;
    let socket = fixture.root.path().join("st3.sock");
    let elsewhere = fixture.root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let workspace = elsewhere.to_str().unwrap();
    // The preview says what applying the change does to the running seat.
    let kdl = succeeded(
        &st(
            &socket,
            &[
                "agents",
                "start",
                &subject,
                "--workspace",
                workspace,
                "--print-kdl",
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    let preview: st3::model::MissionResponse = fixture
        .client()
        .post("/v1/intent/mission", &json!({"intent": {"kdl": kdl}}))
        .await
        .unwrap();
    assert!(
        preview
            .predicted_actions
            .iter()
            .any(|action| action.subject == subject
                && action.action == "restart"
                && action.reason.contains("workspace")),
        "{:?}",
        preview.predicted_actions
    );
    succeeded(
        &st(
            &socket,
            &[
                "agents",
                "start",
                &subject,
                "--workspace",
                workspace,
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    for _ in 0..4 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(
        *fixture.runtime.stops.lock().unwrap(),
        ["fixture:1"],
        "the running seat kept its first workspace"
    );
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].workspace, workspace);
    // The replacement runs the new declaration; further passes leave it running.
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 2);
    // A label is not a launch: renaming the seat leaves it running.
    succeeded(
        &st(
            &socket,
            &[
                "agents",
                "rename",
                &subject,
                "Renamed",
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_restart_continues_the_seats_last_native_session() {
    let (fixture, subject) = Fixture::claude(Shape::TopLevel, "always").await;
    let socket = fixture.root.path().join("st3.sock");
    // A first launch has no session to continue.
    assert_eq!(continued(&fixture.runtime.starts.lock().unwrap()[0]), None);
    bind_session(&fixture, &subject, "fixture:1", "session-one").await;

    // A hangup: the harness ends and its restart policy relaunches it on its session.
    hang_up(&fixture);
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 2);
    assert_eq!(continued(&starts[1]), Some("session-one"));
    assert_eq!(
        starts[1]
            .environment
            .get("ST3_NATIVE_CONTINUE_PATH")
            .map(String::as_str),
        Some("/claude/projects/-example/session-one.jsonl")
    );

    // `st agents restart` continues it. The fixture's harness never reports itself ready, so
    // this asks the daemon directly rather than waiting for a ready harness as the CLI does.
    fixture.request(&subject, "explicit").await;
    for _ in 0..4 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 3);
    assert_eq!(continued(&starts[2]), Some("session-one"));

    // A declared model change restarts the harness, on its latest session.
    bind_session(&fixture, &subject, "fixture:3", "session-two").await;
    succeeded(
        &st(
            &socket,
            &[
                "agents",
                "start",
                &subject,
                "--model",
                "another-model",
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    for _ in 0..4 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(
        starts.len(),
        4,
        "the model change did not restart the harness"
    );
    assert!(launch(&starts[3]).contains(&"another-model".to_owned()));
    assert_eq!(continued(&starts[3]), Some("session-two"));

    // A stopped seat started again continues too, and a daemon restart keeps it running.
    succeeded(
        &st(
            &socket,
            &["agents", "stop", &subject, "--as", "person/avery"],
        )
        .await,
    );
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    succeeded(
        &st(
            &socket,
            &["agents", "start", &subject, "--as", "person/avery"],
        )
        .await,
    );
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 5);
    assert_eq!(continued(&starts[4]), Some("session-two"));
    let daemon = restarted_daemon(&fixture);
    for _ in 0..3 {
        daemon.reconcile_once().unwrap();
    }
    assert_eq!(fixture.runtime.starts.lock().unwrap().len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_the_driver_could_not_continue_is_not_tried_again() {
    let (fixture, subject) = Fixture::claude(Shape::TopLevel, "always").await;
    bind_session(&fixture, &subject, "fixture:1", "session-one").await;
    // The driver started a new session instead, and said so once for that session.
    let _: Value = fixture
        .client()
        .post(
            "/v1/claims",
            &json!({
                "subject": subject,
                "kind": "harness.diagnostic",
                "actor": subject,
                "fields": {
                    "severity": "warning",
                    "status": "transcript-missing",
                    "code": "native-continue-unavailable",
                    "reason": "claude started a new session instead of continuing session-one",
                    "incarnation_id": "fixture:1",
                },
                "evidence": [],
                "idempotency_key": format!("native-continue-unavailable:{subject}:session-one"),
            }),
        )
        .await
        .unwrap();
    hang_up(&fixture);
    for _ in 0..3 {
        fixture.reconciler.reconcile_once().unwrap();
    }
    let starts = fixture.runtime.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 2);
    assert_eq!(continued(&starts[1]), None);
}
