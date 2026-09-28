//! A seat's native session identity and observed harness state belong to the seat's own harness
//! process. The saved conversation reference of an imported seat survives failed resumes and
//! killed incarnations, and no process started inside a seat can speak for the seat.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use sha2::{Digest as _, Sha256};
use st2::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer};
use st3::model::{IntentInput, LaunchSpec, MemberSpec};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use tokio::sync::Notify;

const HOST: &str = "node";
const SAVED_SESSION: &str = "5b0f2c7e-8d1a-4c3b-9e6f-0a1b2c3d4e5f";

/// A terminal runtime that records every launch and reports only the sessions a test gives it.
#[derive(Default)]
struct RecordingRuntime {
    ptys: Mutex<Vec<RuntimeObservation>>,
    started: Mutex<Vec<MemberSpec>>,
}

impl RecordingRuntime {
    fn started(&self) -> Vec<MemberSpec> {
        self.started.lock().unwrap().clone()
    }

    fn report(&self, observation: RuntimeObservation) {
        let mut ptys = self.ptys.lock().unwrap();
        ptys.retain(|pty| pty.runtime_id != observation.runtime_id);
        ptys.push(observation);
    }
}

impl RuntimeControl for RecordingRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        Ok(self.ptys.lock().unwrap().clone())
    }
    fn observe_exec(&self, _runtime_id: &str) -> Result<Option<RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, member: &MemberSpec) -> Result<()> {
        self.started.lock().unwrap().push(member.clone());
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    fn remove(&self, runtime_id: &str, _terminal: bool) -> Result<()> {
        self.ptys
            .lock()
            .unwrap()
            .retain(|pty| pty.runtime_id != runtime_id);
        Ok(())
    }
    fn attach(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

fn apply(store: &Store, source: &str, key: &str) {
    let intent = st3::parse_intent(source, HOST).unwrap();
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &mission.subject_tokens, key).unwrap();
}

fn daemon(
    database: &Path,
    runtime: Arc<RecordingRuntime>,
) -> (Arc<Store>, Reconciler<RecordingRuntime>) {
    let store = Arc::new(Store::open(database, HOST).unwrap());
    let reconciler = Reconciler::new(store.clone(), runtime, HOST.into(), Arc::new(Notify::new()));
    (store, reconciler)
}

fn argv(member: &MemberSpec) -> &[String] {
    match &member.launch {
        LaunchSpec::Argv(argv) => argv,
        LaunchSpec::Shell(source) => panic!("a typed harness launches argv, not `{source}`"),
    }
}

fn resumes(member: &MemberSpec, session: &str) -> bool {
    argv(member)
        .windows(2)
        .any(|pair| pair[0] == "--resume" && pair[1] == session)
}

/// The declaration `st3 import run` writes for a saved Claude Code session: the native session
/// is carried as the harness's own resume arguments, and the seat restarts always.
fn imported_seat_source(workspace: &Path) -> String {
    format!(
        r#"
version 2

agent "import/claude/3d1f0a9b8c7e6d5f4a3b2c1d" {{
    workspace "{workspace}"
    harness "claude" {{
        args "--resume" "{SAVED_SESSION}"
    }}
    restart "always"
}}
"#,
        workspace = workspace.display(),
    )
}

const IMPORTED_SEAT: &str = "import/claude/3d1f0a9b8c7e6d5f4a3b2c1d";

fn declared_member(store: &Store) -> MemberSpec {
    store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject.ends_with(IMPORTED_SEAT))
        .expect("the imported seat is still declared")
        .member
        .expect("the seat keeps its member launch")
}

fn exited(member: &MemberSpec, incarnation: &str, exit_code: i64) -> RuntimeObservation {
    RuntimeObservation {
        runtime_id: member.runtime_id.clone(),
        terminal: true,
        status: "exited".into(),
        exit_code: Some(exit_code),
        incarnation_id: Some(incarnation.into()),
    }
}

#[test]
fn an_imported_seat_retries_a_failed_resume_with_the_same_saved_session() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let runtime = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&root.path().join("graph.db"), runtime.clone());
    apply(&store, &imported_seat_source(&workspace), "import-seat");
    reconciler.reconcile_once().unwrap();
    let first = runtime.started();
    assert_eq!(first.len(), 1, "the imported seat launches once");
    let member = first[0].clone();
    assert!(resumes(&member, SAVED_SESSION), "{:?}", argv(&member));

    // The resume fails: the harness exits at once, as it does when it cannot open the
    // conversation (an unreadable workspace, a missing transcript). Every failure is retried
    // within the restart budget, and the budget then holds the seat instead of dropping it.
    runtime.report(exited(&member, "failed-resume", 1));
    for _ in 0..5 {
        reconciler.reconcile_once().unwrap();
    }
    let launches = runtime.started();
    assert!(
        launches.len() >= 2,
        "a failed resume is retried, not abandoned: {} launches",
        launches.len()
    );
    for launch in &launches {
        assert!(
            resumes(launch, SAVED_SESSION),
            "every relaunch resumes the saved conversation: {:?}",
            argv(launch)
        );
    }
    assert!(resumes(&declared_member(&store), SAVED_SESSION));
}

#[test]
fn an_imported_seat_killed_at_shutdown_resumes_its_saved_session_after_the_restart() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let database = root.path().join("graph.db");

    let runtime = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, runtime.clone());
    apply(&store, &imported_seat_source(&workspace), "import-seat");
    reconciler.reconcile_once().unwrap();
    let member = runtime.started()[0].clone();
    // The live harness is terminated by the shutdown before the daemon itself stops.
    runtime.report(exited(&member, "killed-at-shutdown", 143));
    reconciler.reconcile_once().unwrap();
    drop(reconciler);
    drop(store);

    // A machine restart: a fresh daemon over the same durable graph and an empty terminal
    // runtime, with no client attached.
    let rebooted = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, rebooted.clone());
    assert!(resumes(&declared_member(&store), SAVED_SESSION));
    reconciler.reconcile_once().unwrap();
    let after_reboot = rebooted.started();
    assert!(
        !after_reboot.is_empty()
            && after_reboot
                .iter()
                .all(|launch| resumes(launch, SAVED_SESSION)),
        "after the restart the seat resumes its saved conversation: {:?}",
        after_reboot.iter().map(argv).collect::<Vec<_>>()
    );
}

fn st3_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_st3"))
}

fn hex24(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..24].to_string()
}

fn hex16(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..16].to_string()
}

/// Where the seat's native driver keeps its private agent directory under the driver state root.
fn seat_agent_dir(state_root: &Path, subject: &str) -> PathBuf {
    let identity = subject.strip_prefix("agent/").unwrap_or(subject);
    state_root
        .join(hex24(subject))
        .join("catalog")
        .join("agents")
        .join(st2::run::detect_host())
        .join(hex16(identity))
}

fn read_state(agent_dir: &Path) -> Activity {
    st2::harness_state::read(&st2::harness_state::harness_state_path(agent_dir), None)
        .expect("the seat has an observed harness record")
        .state
}

#[test]
#[ignore = "fails on main: a Claude process started inside a seat inherits the seat's channel environment, and its channel server rewrites the seat's working state as ready"]
fn a_claude_process_started_inside_a_seat_cannot_rewrite_the_seats_observed_state() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config = root.path().join("config");
    let state_root = root.path().join("drivers");
    let elsewhere = root.path().join("elsewhere");
    for dir in [&home, &config, &state_root, &elsewhere] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let subject = "agent/fixture/worker";
    let identity = "fixture/worker";
    let runtime_id = identity;
    let wrapper_session = "seat-wrapper-session";
    let agent_dir = seat_agent_dir(&state_root, subject);
    std::fs::create_dir_all(&agent_dir).unwrap();

    // The seat's wrapper claims the record, and the seat's own hooks report a running turn.
    let seq = st2::harness_state::claim(&agent_dir, identity, "claude", wrapper_session).unwrap();
    Writer::new(&agent_dir, identity, "claude", Some(runtime_id.into()))
        .with_ownership(wrapper_session, seq)
        .observe(Observation::new(
            Activity::Active,
            BlockedOn::None,
            InputBuffer::Unknown,
        ))
        .unwrap();
    assert_eq!(read_state(&agent_dir), Activity::Active);

    // During that turn the agent runs another Claude from its shell tool, in another directory.
    // The inner Claude starts its user-scoped channel server with the environment every tool
    // child of the seat inherits.
    let mut child = Command::new(st3_binary())
        .args(["driver", "claude-mcp"])
        .current_dir(&elsewhere)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("ST_AGENT", subject)
        .env("ST3_DRIVER_STATE_DIR", &state_root)
        .env("ST2_CLAUDE_IDENTITY", identity)
        .env("ST2_CLAUDE_RUNTIME_ID", runtime_id)
        .env("ST2_CLAUDE_SESSION", wrapper_session)
        .env("ST2_CLAUDE_SESSION_SEQ", seq.to_string())
        .env("CATALOG", state_root.join(hex24(subject)).join("catalog"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#
    )
    .unwrap();
    let mut response = String::new();
    stdout.read_line(&mut response).unwrap();
    assert!(response.contains("\"serverInfo\""), "{response}");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && read_state(&agent_dir) == Activity::Active {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdin);
    let _ = child.wait();

    assert_eq!(
        read_state(&agent_dir),
        Activity::Active,
        "the seat is still in its turn; a nested Claude must not report it ready"
    );
}
