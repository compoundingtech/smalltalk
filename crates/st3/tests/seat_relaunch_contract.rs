//! A seat's launch is durable graph state. A daemon restart adopts the live seat; when the whole
//! terminal runtime is gone (a machine restart), a fresh daemon relaunches every seat by itself,
//! with no client attached, from the declaration it recorded: the same harness flags, model,
//! effort, environment, profile, working directory, and name.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;
use st3::model::{ClaimInput, IntentInput, LaunchSpec, MemberSpec};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use tokio::sync::Notify;

const HOST: &str = "node";

/// A terminal runtime that records every launch and reports only the sessions a test gives it.
#[derive(Default)]
struct RecordingRuntime {
    ptys: Mutex<Vec<RuntimeObservation>>,
    started: Mutex<Vec<MemberSpec>>,
    keys: Mutex<Vec<String>>,
}

impl RecordingRuntime {
    fn with_ptys(ptys: Vec<RuntimeObservation>) -> Self {
        Self {
            ptys: Mutex::new(ptys),
            ..Self::default()
        }
    }

    fn started(&self) -> Vec<MemberSpec> {
        self.started.lock().unwrap().clone()
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
    fn send_key(&self, _: &str, key: &str) -> Result<()> {
        self.keys.lock().unwrap().push(key.into());
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

/// One daemon lifetime: a fresh store handle over the durable database and a fresh reconciler,
/// with no client connected to either.
fn daemon(database: &Path, runtime: Arc<RecordingRuntime>) -> (Arc<Store>, Reconciler<RecordingRuntime>) {
    let store = Arc::new(Store::open(database, HOST).unwrap());
    let reconciler = Reconciler::new(store.clone(), runtime, HOST.into(), Arc::new(Notify::new()));
    (store, reconciler)
}

fn running(member: &MemberSpec, incarnation: &str) -> RuntimeObservation {
    RuntimeObservation {
        runtime_id: member.runtime_id.clone(),
        terminal: true,
        status: "running".into(),
        exit_code: None,
        incarnation_id: Some(incarnation.into()),
    }
}

fn argv(member: &MemberSpec) -> &[String] {
    match &member.launch {
        LaunchSpec::Argv(argv) => argv,
        LaunchSpec::Shell(source) => panic!("a typed harness launches argv, not `{source}`"),
    }
}

fn contains_in_order(haystack: &[String], needle: &[&str]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.iter().map(String::as_str).eq(needle.iter().copied()))
}

fn seat_source(workspace: &Path, profile: &Path) -> String {
    format!(
        r#"
version 2

agent "garden/worker" {{
    name "Garden worker"
    workspace "{workspace}"
    env {{
        CLAUDE_CONFIG_DIR "{profile}"
        OMP_PROFILE "restricted"
    }}
    harness "claude" {{
        model "claude-sonnet-5"
        effort "high"
        args "--permission-mode" "auto" "--dangerously-skip-permissions"
    }}
}}

agent "import/claude/saved" {{
    workspace "{workspace}"
    restart "always"
    harness "claude" {{
        args "--resume" "0f8a5a3e-5d0c-4f7a-9c61-2b7c1f0e9d42"
    }}
}}
"#,
        workspace = workspace.display(),
        profile = profile.display(),
    )
}

#[test]
fn a_machine_restart_relaunches_each_seat_headless_with_its_exact_declared_launch() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("garden");
    std::fs::create_dir_all(&workspace).unwrap();
    let profile = root.path().join("claude-profile");
    let database = root.path().join("state/st3.sqlite");

    // First boot: the declaration is applied and both seats launch.
    let first_runtime = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, first_runtime.clone());
    apply(&store, &seat_source(&workspace, &profile), "seats");
    reconciler.reconcile_once().unwrap();
    let first = first_runtime.started();
    assert_eq!(first.len(), 2, "both declared seats launch on first boot");
    *first_runtime.ptys.lock().unwrap() = first
        .iter()
        .map(|member| running(member, &format!("{}-boot-one", member.runtime_id)))
        .collect();
    reconciler.reconcile_once().unwrap();
    assert_eq!(first_runtime.started().len(), 2, "a live seat is not relaunched");
    drop(reconciler);
    drop(store);

    // A daemon restart while the terminals survive adopts both seats and launches nothing.
    let survivors = first_runtime.ptys.lock().unwrap().clone();
    let restart_runtime = Arc::new(RecordingRuntime::with_ptys(survivors));
    let (store, reconciler) = daemon(&database, restart_runtime.clone());
    reconciler.reconcile_once().unwrap();
    assert!(
        restart_runtime.started().is_empty(),
        "a daemon restart adopts live seats instead of relaunching them"
    );
    for member in &first {
        let actual = store
            .latest_actual_value(&format!("agent/{}", member_identity(member)))
            .unwrap()
            .expect("an adopted seat has an actual observation");
        let fields = actual.get("fields").unwrap_or(&actual);
        assert_eq!(fields["status"], "running", "{actual}");
        assert_eq!(fields["adopted"], true, "{actual}");
    }
    drop(reconciler);
    drop(store);

    // A machine restart: every terminal is gone and no client ever connects. The daemon's own
    // reconcile pass relaunches each seat once, from the durable declaration alone.
    let reboot_runtime = Arc::new(RecordingRuntime::default());
    let (_store, reconciler) = daemon(&database, reboot_runtime.clone());
    reconciler.reconcile_once().unwrap();
    let relaunched = reboot_runtime.started();
    assert_eq!(relaunched.len(), 2, "each seat relaunches exactly once");
    assert!(
        reboot_runtime.keys.lock().unwrap().is_empty(),
        "nothing is typed into a relaunched seat's terminal"
    );

    for before in &first {
        let after = relaunched
            .iter()
            .find(|member| member.runtime_id == before.runtime_id)
            .expect("the same runtime identity relaunches");
        assert_eq!(after.launch, before.launch, "the launch argv is replayed");
        assert_eq!(after.environment, before.environment, "the environment is replayed");
        assert_eq!(after.cwd, before.cwd, "the working directory is replayed");
        assert_eq!(after.workspace, before.workspace);
        assert_eq!(after.display_name, before.display_name, "the seat name is replayed");
        assert_eq!(after.tags, before.tags);
        assert_eq!(after.cwd, workspace.to_string_lossy(), "the declared workspace, never $HOME");
    }

    let worker = relaunched
        .iter()
        .find(|member| member.runtime_id.ends_with("garden.worker"))
        .unwrap();
    let worker_argv = argv(worker);
    for flags in [
        &["--model", "claude-sonnet-5"][..],
        &["--effort", "high"][..],
        &["--permission-mode", "auto", "--dangerously-skip-permissions"][..],
    ] {
        assert!(
            contains_in_order(worker_argv, flags),
            "relaunch keeps {flags:?}: {worker_argv:?}"
        );
    }
    assert_eq!(
        worker.environment.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some(profile.to_string_lossy().as_ref()),
        "a declared config directory survives the restart"
    );
    assert_eq!(
        worker.environment.get("OMP_PROFILE").map(String::as_str),
        Some("restricted"),
        "a declared profile survives the restart"
    );
    assert_eq!(worker.display_name.as_deref(), Some("Garden worker"));

    let imported = relaunched
        .iter()
        .find(|member| member.runtime_id.contains("import"))
        .unwrap();
    assert!(
        contains_in_order(
            argv(imported),
            &["--resume", "0f8a5a3e-5d0c-4f7a-9c61-2b7c1f0e9d42"]
        ),
        "a seat that declares a native session resumes it after the restart: {:?}",
        argv(imported)
    );
}

#[test]
fn a_seat_whose_terminal_exited_during_shutdown_is_relaunched_not_forgotten() {
    for (label, exit_code) in [("hangup", 129_i64), ("error", 1), ("clean", 0)] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("garden");
        std::fs::create_dir_all(&workspace).unwrap();
        let database = root.path().join("state/st3.sqlite");
        let runtime = Arc::new(RecordingRuntime::default());
        let (store, reconciler) = daemon(&database, runtime.clone());
        apply(
            &store,
            &format!(
                r#"
version 2
agent "garden/worker" {{
    workspace "{}"
    harness "claude" {{
        model "claude-sonnet-5"
    }}
}}
"#,
                workspace.display()
            ),
            label,
        );
        reconciler.reconcile_once().unwrap();
        let first = runtime.started();
        assert_eq!(first.len(), 1);
        drop(reconciler);
        drop(store);

        // The machine is shutting down: the terminal runtime reports the seat exited with the
        // status its harness returned to the hangup, and then a fresh daemon starts.
        let shutdown = Arc::new(RecordingRuntime::with_ptys(vec![RuntimeObservation {
            runtime_id: first[0].runtime_id.clone(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(exit_code),
            incarnation_id: Some(format!("{label}-one")),
        }]));
        let (store, reconciler) = daemon(&database, shutdown.clone());
        reconciler.reconcile_once().unwrap();
        let relaunched = shutdown.started();
        assert_eq!(relaunched.len(), 1, "{label}: the seat relaunches");
        assert_eq!(relaunched[0].launch, first[0].launch, "{label}");
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .any(|subject| subject.subject == "agent/garden/worker"),
            "{label}: an exit never removes the seat's declaration"
        );
    }
}

#[test]
fn an_unavailable_workspace_holds_the_relaunch_and_never_rewrites_the_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("mounted/garden");
    std::fs::create_dir_all(&workspace).unwrap();
    let profile = root.path().join("claude-profile");
    let database = root.path().join("state/st3.sqlite");
    let runtime = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, runtime.clone());
    apply(&store, &seat_source(&workspace, &profile), "seats");
    reconciler.reconcile_once().unwrap();
    let first = runtime.started();
    assert_eq!(first.len(), 2);
    drop(reconciler);
    drop(store);

    // After the machine restart the volume holding the workspace is not mounted yet.
    std::fs::remove_dir_all(root.path().join("mounted")).unwrap();
    let unmounted = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, unmounted.clone());
    reconciler.reconcile_once().unwrap();
    assert!(
        unmounted.started().is_empty(),
        "a seat whose workspace is missing is held, not started somewhere else"
    );
    for subject in store.desired_subjects().unwrap() {
        if let Some(member) = subject.member {
            assert_eq!(member.cwd, workspace.to_string_lossy(), "the declaration keeps its directory");
        }
    }

    // The volume returns: the held seats launch in their own directory with their own launch.
    std::fs::create_dir_all(&workspace).unwrap();
    reconciler.reconcile_once().unwrap();
    let relaunched = unmounted.started();
    assert_eq!(relaunched.len(), 2);
    for before in &first {
        let after = relaunched
            .iter()
            .find(|member| member.runtime_id == before.runtime_id)
            .unwrap();
        assert_eq!(after.cwd, before.cwd);
        assert_eq!(after.launch, before.launch);
        assert_eq!(after.environment, before.environment);
    }
}

#[test]
fn a_seat_name_is_declared_and_survives_new_harness_sessions_and_restarts() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("garden");
    std::fs::create_dir_all(&workspace).unwrap();
    let profile = root.path().join("claude-profile");
    let database = root.path().join("state/st3.sqlite");
    let runtime = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, runtime.clone());
    apply(&store, &seat_source(&workspace, &profile), "seats");
    reconciler.reconcile_once().unwrap();
    let worker = runtime
        .started()
        .into_iter()
        .find(|member| member.runtime_id.ends_with("garden.worker"))
        .unwrap();
    *runtime.ptys.lock().unwrap() = vec![running(&worker, "incarnation-one")];
    reconciler.reconcile_once().unwrap();

    // The harness inside the seat switches to a new provider session several times; the driver
    // reports each new session's state. None of that is seat identity.
    for (index, state) in ["ready", "working", "idle"].into_iter().enumerate() {
        store
            .append_claim(&ClaimInput {
                subject: "agent/garden/worker".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/garden/worker".into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String(state.into())),
                    ("driver".into(), Value::String("claude".into())),
                    ("transport".into(), Value::String("claude-channel".into())),
                    ("incarnation_id".into(), Value::String("incarnation-one".into())),
                    (
                        "evidence_incarnation".into(),
                        Value::String(format!("provider-session-{index}")),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    reconciler.reconcile_once().unwrap();
    let declared_name = |store: &Store| {
        store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/garden/worker")
            .and_then(|subject| subject.member)
            .and_then(|member| member.display_name)
    };
    assert_eq!(declared_name(&store).as_deref(), Some("Garden worker"));
    drop(reconciler);
    drop(store);

    // A machine restart: the seat keeps its address and its name.
    let reboot = Arc::new(RecordingRuntime::default());
    let (store, reconciler) = daemon(&database, reboot.clone());
    reconciler.reconcile_once().unwrap();
    assert_eq!(declared_name(&store).as_deref(), Some("Garden worker"));
    let relaunched = reboot
        .started()
        .into_iter()
        .find(|member| member.runtime_id == worker.runtime_id)
        .expect("the seat relaunches under the same runtime identity");
    assert_eq!(relaunched.display_name.as_deref(), Some("Garden worker"));
    assert_eq!(relaunched.environment.get("ST_AGENT"), worker.environment.get("ST_AGENT"));
    assert_eq!(
        relaunched.environment.get("ST_AGENT").map(String::as_str),
        Some("agent/garden/worker"),
        "the seat's address is its declared subject"
    );
}

fn member_identity(member: &MemberSpec) -> String {
    member
        .tags
        .get("st3.subject")
        .and_then(|subject| subject.strip_prefix("agent/"))
        .expect("a typed seat is tagged with its subject")
        .to_owned()
}
