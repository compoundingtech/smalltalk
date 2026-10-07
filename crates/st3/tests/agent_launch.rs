#![cfg(unix)]
//! Real finite fake harnesses behind the runtime, API and CLI. No fleet seats are touched.
use serde_json::{Value, json};
use st3::{
    api::AppState,
    model::{ClaimInput, MemberSpec},
    reconcile::{Reconciler, RuntimeControl, RuntimeObservation},
    store::Store,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

struct Processes {
    exec: st_runtime::ExecRuntime,
    ids: Mutex<BTreeSet<String>>,
    starts: Mutex<usize>,
    code: i32,
    root: PathBuf,
}
impl Processes {
    fn new(root: &Path, code: i32) -> Self {
        st_runtime::initialize_isolation(&BTreeMap::new());
        Self {
            exec: st_runtime::ExecRuntime::new(root.join("processes"), root.join("logs")),
            ids: Mutex::new(BTreeSet::new()),
            starts: Mutex::new(0),
            code,
            root: root.into(),
        }
    }
    fn observation(&self, id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        Ok(self.exec.observe(id)?.map(|observed| match observed {
            st_runtime::ExecObservation::Running(g) => RuntimeObservation {
                runtime_id: id.into(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(g.generation_id),
            },
            st_runtime::ExecObservation::Exited(g) => RuntimeObservation {
                runtime_id: id.into(),
                terminal: true,
                status: "exited".into(),
                exit_code: g.exit_code.map(i64::from),
                incarnation_id: Some(g.generation_id),
            },
            st_runtime::ExecObservation::Indeterminate(reason) => {
                panic!("fixture process indeterminate: {reason}")
            }
        }))
    }
}
impl RuntimeControl for Processes {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<RuntimeObservation>> {
        self.ids
            .lock()
            .unwrap()
            .iter()
            .map(|id| self.observation(id))
            .collect::<anyhow::Result<Vec<_>>>()
            .map(|items| items.into_iter().flatten().collect())
    }
    fn observe_exec(&self, id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        self.observation(id)
    }
    fn start(&self, member: &MemberSpec) -> anyhow::Result<()> {
        *self.starts.lock().unwrap() += 1;
        self.ids.lock().unwrap().insert(member.runtime_id.clone());
        let ending = if self.code == 127 {
            format!(
                "exec {}",
                self.root.join("missing-fixture-executable").display()
            )
        } else {
            format!("exit {}", self.code)
        };
        let script = format!(
            "printf 'fixture-stdout-marker\\n'; printf 'fixture-stderr-marker\\n' >&2; {ending}"
        );
        self.exec.spawn(
            &member.runtime_id,
            &st_runtime::Launch::Argv(vec![
                env!("ST3_FIXTURE_BASH").into(),
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                script,
            ]),
            &self.root,
            &BTreeMap::new(),
        )?;
        // Deliberately reap before launch acceptance, exercising exit-before-observation.
        for _ in 0..100 {
            if self
                .observation(&member.runtime_id)?
                .is_some_and(|o| o.status == "exited")
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        anyhow::bail!("finite fixture harness failed to exit")
    }
    fn stop(&self, id: &str, _: bool, expected: Option<&str>) -> anyhow::Result<()> {
        self.exec.stop_if(id, expected)
    }
    fn kill(&self, id: &str, _: bool, expected: Option<&str>) -> anyhow::Result<()> {
        self.exec.kill_if(id, expected)
    }
    fn remove(&self, id: &str, _: bool) -> anyhow::Result<()> {
        self.exec.remove(id)
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.exec.read_log_tail(id)
    }
    fn exit_tail(&self, observation: &RuntimeObservation) -> anyhow::Result<Option<String>> {
        self.exec.read_log_tail(&observation.runtime_id)
    }
}
fn state(root: &Path, store: Arc<Store>) -> AppState {
    AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "studio".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    }
}
fn declare(store: &Store, source: &str, key: &str) {
    let intent = st3::parse_intent(source, "studio").unwrap();
    let preview = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, key, Some("person/avery"))
        .unwrap();
}
fn claim(store: &Store, subject: &str, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}
async fn early_exit_cli(code: i32, structured: bool, attach: bool) {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "studio").unwrap());
    let runtime = Arc::new(Processes::new(root.path(), code));
    let state = state(root.path(), store.clone());
    let reconciler = Arc::new(Reconciler::new(
        store.clone(),
        runtime.clone(),
        "studio".into(),
        state.notify.clone(),
    ));
    let socket = root.path().join("api.sock");
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    // serve_unix owns a borrowed path only until this enclosing task ends.
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let drive = tokio::spawn(async move {
        loop {
            reconciler.reconcile_once().unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let mut command = st3::test_support::async_command(env!("CARGO_BIN_EXE_st3-fixture"));
    command
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null());
    command.args(["--endpoint", socket.to_str().unwrap(), "--daemon-wait", "0"]);
    if structured {
        command.arg("--json");
    }
    command.args([
        "agents",
        "new",
        "example/early-exit",
        "--host",
        "studio",
        "--workspace",
        root.path().to_str().unwrap(),
        "--harness",
        "pi",
        "--model",
        "invented-model",
        "--timeout",
        "5s",
        "--as",
        "person/avery",
    ]);
    if attach {
        command.arg("--attach");
    }
    let started = Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap();
    drive.abort();
    server.abort();
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "exit was not prompt: {:?}",
        started.elapsed()
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("exit code"), "{stderr}");
    assert!(
        stderr.contains("fixture-stdout-marker") && stderr.contains("fixture-stderr-marker"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("404") && !stderr.contains("not ready after"),
        "{stderr}"
    );
    assert_eq!(*runtime.starts.lock().unwrap(), 1);
    if structured {
        let result: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(result["error"]["exit_code"], code);
        assert!(
            result["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["stage"] == "replicated" && stage["state"] == "skipped")
        );
        assert!(!stderr.contains("declared (completed)"));
    } else {
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.contains("local launch; replication skipped"));
        assert!(stderr.contains("declared") && stderr.contains("exited"));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_new_returns_nonzero_exit_and_both_streams_before_readiness() {
    early_exit_cli(23, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_new_preserves_zero_exit_and_machine_readable_failure() {
    early_exit_cli(0, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_new_reports_the_missing_harness_executable_exit() {
    early_exit_cli(127, true, true).await;
}

#[test]
fn requested_failure_survives_another_incarnation_and_source_replacement() {
    let store = Store::open_memory("studio").unwrap();
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; restart always; }",
        "declare",
    );
    let subject = "agent/example/worker";
    let token = store.selected_desired_token(subject).unwrap().unwrap();
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({"action":"start","desired_token":token,"incarnation_id":"first"}),
    );
    claim(
        &store,
        subject,
        "runtime.observed",
        json!({"status":"exited","incarnation_id":"first","exit_code":17}),
    );
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({"action":"start","desired_token":token,"incarnation_id":"second"}),
    );
    claim(
        &store,
        subject,
        "runtime.observed",
        json!({"status":"running","incarnation_id":"second"}),
    );
    claim(
        &store,
        subject,
        "harness.observed",
        json!({"state":"ready","incarnation_id":"second"}),
    );
    let status = st3::agent_launch::read(&store, subject, &token).unwrap();
    assert_eq!(status.stage, "exited");
    assert_eq!(status.exit_code, Some(17));
    assert_eq!(status.incarnation_id.as_deref(), Some("first"));
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"false\"; restart always; }",
        "replace",
    );
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .exit_code,
        Some(17)
    );
}

#[test]
fn local_declaration_does_not_prove_destination_replication_or_readiness() {
    let store = Store::open_memory("studio").unwrap();
    declare(
        &store,
        "version 2\nagent \"example/remote\" { host \"workshop\"; workspace \"/tmp\"; command \"true\"; }",
        "declare",
    );
    let subject = "agent/example/remote";
    let token = store.selected_desired_token(subject).unwrap().unwrap();
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .stage,
        "replicating"
    );
    assert_eq!(
        st3::agent_launch::read(&Store::open_memory("workshop").unwrap(), subject, &token)
            .unwrap()
            .stage,
        "replicating"
    );
}

#[test]
fn real_crash_does_not_restart_on_repeated_reconcile_passes_and_new_revision_bypasses_delay() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("studio").unwrap());
    let runtime = Arc::new(Processes::new(root.path(), 9));
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "studio".into(),
        Arc::new(Notify::new()),
    );
    let source = format!(
        "version 2\nagent \"example/crash\" {{ workspace {:?}; command \"true\"; restart always; }}",
        root.path()
    );
    declare(&store, &source, "initial");
    for _ in 0..12 {
        reconciler.reconcile_once().unwrap();
    }
    assert_eq!(*runtime.starts.lock().unwrap(), 1);
    let decision = store
        .latest_observation("agent/example/crash", "runtime.reconcile-decision")
        .unwrap()
        .unwrap();
    assert!(
        decision.body["fields"]["key"]
            .as_str()
            .unwrap()
            .starts_with("crash-backoff:")
    );
    let due = decision.body["fields"]["restart_at_unix_ms"]
        .as_str()
        .unwrap()
        .parse::<u128>()
        .unwrap();
    assert!(due >= decision.accepted_at_unix_ms + 4_000);
    declare(
        &store,
        &source.replace("command \"true\"", "command \"false\""),
        "replacement",
    );
    reconciler.reconcile_once().unwrap();
    assert_eq!(*runtime.starts.lock().unwrap(), 2);
}
