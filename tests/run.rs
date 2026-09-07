//! M1 correctness net: plan execution against a fake Runner (no real processes spawned).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use st2::message;
use st2::reconcile::{
    Launch, PtyPresentation, ReconcilePlan, Session, TaskCompileContext, TaskLaunch, TaskTarget,
    Teardown, compile_generated_tasks,
};
use st2::run::Runner;
use st2::run::{CrashLoop, surface_crash_loop, up_once_selected, up_once_selected_specs};
use st2::spec::{AgentDesiredState, AgentSpec, JobType, Task, TaskKind, TaskLifecycle};

fn selected_catalog_agent(identity: &str, workspace: &Path, render: &str) -> String {
    format!(
        r#"agent "{identity}" {{
  host "host"
  type "service"
  workspace "{}"
  pty "work" {{
    id "host.{identity}.work"
    command "true"
  }}
  render {{
    {render}
  }}
}}
"#,
        workspace.display()
    )
}

fn write_selected_catalog(
    catalog: &Path,
    owner_workspace: &Path,
    sibling_workspace: &Path,
    owner_render: &str,
) {
    fs::create_dir_all(owner_workspace).unwrap();
    fs::create_dir_all(sibling_workspace).unwrap();
    write(
        catalog,
        "agents/host/owner/agent.kdl",
        &selected_catalog_agent("owner", owner_workspace, owner_render),
    );
    write(
        catalog,
        "agents/host/sibling/agent.kdl",
        &selected_catalog_agent(
            "sibling",
            sibling_workspace,
            r#"file "SIBLING.txt" "sibling""#,
        ),
    );
}

#[test]
fn selected_catalog_two_agent_kdl_recording_runner_matrix() {
    enum Actual {
        Missing,
        Live,
        Dead,
    }

    for actual in [Actual::Missing, Actual::Live, Actual::Dead] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let owner_workspace = tmp.path().join("owner-workspace");
        let sibling_workspace = tmp.path().join("sibling-workspace");
        write_selected_catalog(
            &catalog,
            &owner_workspace,
            &sibling_workspace,
            r#"file "OWNER.txt" "owner""#,
        );

        let mut sessions = vec![live("host.sibling.work")];
        match actual {
            Actual::Missing => {}
            Actual::Live => sessions.push(live("host.owner.work")),
            Actual::Dead => sessions.push(dead("host.owner.work")),
        }
        let runner = FakeRunner {
            sessions,
            ..Default::default()
        };

        let report = up_once_selected(&catalog, "host.owner.work", "host", &runner).unwrap();

        assert_eq!(runner.list_calls.get(), 1);
        assert_eq!(
            fs::read_to_string(owner_workspace.join("OWNER.txt")).unwrap(),
            "owner"
        );
        assert!(
            !sibling_workspace.join("SIBLING.txt").exists(),
            "the unrelated owner must not be materialized"
        );
        assert!(runner.killed.borrow().is_empty());
        assert!(runner.removed.borrow().is_empty());
        match actual {
            Actual::Missing => {
                assert_eq!(runner.spawned.borrow().as_slice(), ["host.owner.work"]);
                assert!(runner.reaped.borrow().is_empty());
                assert_eq!(report.launched, ["host.owner.work"]);
                assert!(report.restarted.is_empty());
                assert!(report.gc.is_empty());
            }
            Actual::Live => {
                assert!(runner.spawned.borrow().is_empty());
                assert!(runner.reaped.borrow().is_empty());
                assert_eq!(report.adopted, ["owner"]);
                assert!(report.launched.is_empty());
                assert!(report.restarted.is_empty());
                assert!(report.gc.is_empty());
            }
            Actual::Dead => {
                assert_eq!(runner.reaped.borrow().as_slice(), ["host.owner.work"]);
                assert_eq!(runner.spawned.borrow().as_slice(), ["host.owner.work"]);
                assert!(report.launched.is_empty());
                assert_eq!(report.restarted, ["host.owner.work"]);
                assert!(report.gc.is_empty());
            }
        }
        assert!(
            runner
                .spawned
                .borrow()
                .iter()
                .chain(runner.reaped.borrow().iter())
                .all(|id| id == "host.owner.work")
        );
    }
}

#[test]
fn selected_catalog_surfaces_unrelated_malformed_diagnostics_without_blocking_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let owner_workspace = tmp.path().join("owner-workspace");
    let sibling_workspace = tmp.path().join("sibling-workspace");
    write_selected_catalog(
        &catalog,
        &owner_workspace,
        &sibling_workspace,
        r#"file "OWNER.txt" "owner""#,
    );
    write(
        &catalog,
        "agents/host/sibling/broken.kdl",
        r#"agent "broken" {"#,
    );
    let runner = FakeRunner {
        sessions: vec![live("host.sibling.work")],
        ..Default::default()
    };

    let report = up_once_selected(&catalog, "host.owner.work", "host", &runner).unwrap();

    assert_eq!(runner.spawned.borrow().as_slice(), ["host.owner.work"]);
    assert_eq!(
        fs::read_to_string(owner_workspace.join("OWNER.txt")).unwrap(),
        "owner"
    );
    assert!(!sibling_workspace.join("SIBLING.txt").exists());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("broken.kdl") && error.contains("KDL")),
        "{:?}",
        report.errors
    );
}

#[test]
fn selected_catalog_owner_render_failure_refuses_runner_actions() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let owner_workspace = tmp.path().join("owner-workspace");
    let sibling_workspace = tmp.path().join("sibling-workspace");
    write_selected_catalog(
        &catalog,
        &owner_workspace,
        &sibling_workspace,
        r#"copy "_templates/missing" "OWNER.txt""#,
    );
    let runner = FakeRunner {
        sessions: vec![live("host.sibling.work")],
        ..Default::default()
    };

    let report = up_once_selected(&catalog, "host.owner.work", "host", &runner).unwrap();

    assert_eq!(runner.list_calls.get(), 0);
    assert_refusal(&runner);
    assert!(!owner_workspace.join("OWNER.txt").exists());
    assert!(!sibling_workspace.join("SIBLING.txt").exists());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("_templates/missing")),
        "{:?}",
        report.errors
    );
}

#[test]
fn selected_one_shot_unknown_refuses_before_runner_list() {
    let runner = FakeRunner::default();
    let error =
        up_once_selected_specs(Path::new("/tmp"), &[], "host.missing.task", "host", &runner)
            .unwrap_err();
    assert!(error.to_string().contains("did not resolve"));
    assert_eq!(runner.list_calls.get(), 0);
    assert!(runner.spawned.borrow().is_empty());
    assert!(runner.killed.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
}

fn task_spec(identity: &str, host: Option<&str>, id: &str) -> AgentSpec {
    AgentSpec {
        id: None,
        address: None,
        identity: identity.into(),
        name: None,
        description: None,
        host: host.map(str::to_owned),
        role: None,
        job_type: JobType::Service,
        workspace: None,
        supervisor: None,
        desired_state: AgentDesiredState::Running,
        keep: false,
        restart: None,
        delivery: None,
        session_driver: None,
        driver: None,
        delivery_readiness: None,
        resources: vec![],
        streams: Vec::new(),
        tasks: vec![Task {
            kind: TaskKind::Exec,
            derived: false,
            name: "work".into(),
            id: Some(id.into()),
            command: Some("true".into()),
            argv: None,
            cwd: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
            lifecycle: TaskLifecycle::Service,
        }],
        path: "/tmp/spec.kdl".into(),
    }
}

fn assert_refusal(runner: &FakeRunner) {
    assert_eq!(runner.list_calls.get(), 0);
    assert!(runner.spawned.borrow().is_empty());
    assert!(runner.killed.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
}

fn two_task_spec(identity: &str, first: &str, second: &str) -> AgentSpec {
    let mut spec = task_spec(identity, None, first);
    spec.tasks.push(Task {
        kind: TaskKind::Exec,
        derived: false,
        name: "side".into(),
        id: Some(second.into()),
        command: Some("true".into()),
        argv: None,
        cwd: None,
        tags: BTreeMap::new(),
        env: BTreeMap::new(),
        keep: false,
        lifecycle: TaskLifecycle::Service,
    });
    spec
}

#[test]
fn selected_one_shot_missing_spawns_only_selected_task() {
    let runner = FakeRunner {
        sessions: vec![live("host.agent.side"), live("host.sibling.work")],
        ..Default::default()
    };
    let specs = vec![
        two_task_spec("agent", "host.agent.work", "host.agent.side"),
        task_spec("sibling", None, "host.sibling.work"),
    ];
    let report = up_once_selected_specs(
        Path::new("/tmp"),
        &specs,
        "host.agent.work",
        "host",
        &runner,
    )
    .unwrap();
    assert_eq!(runner.list_calls.get(), 1);
    assert_eq!(runner.spawned.borrow().as_slice(), ["host.agent.work"]);
    assert!(runner.killed.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
    assert_eq!(report.launched, ["host.agent.work"]);
}

#[test]
fn selected_adopt_only_absent_task_is_reported_held_without_runner_mutation() {
    let mut spec = task_spec("agent", None, "host.agent.work");
    spec.tasks[0].lifecycle = TaskLifecycle::AdoptOnly;
    let runner = FakeRunner::default();

    let report = up_once_selected_specs(
        Path::new("/tmp"),
        &[spec],
        "host.agent.work",
        "host",
        &runner,
    )
    .unwrap();

    assert_eq!(report.held, ["host.agent.work"]);
    assert!(report.launched.is_empty());
    assert!(report.gc.is_empty());
    assert!(runner.spawned.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
}

#[test]
fn selected_missing_derived_ding_is_held_without_broadening_to_its_agent() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let runner = FakeRunner::default();

    let report = up_once_selected(tmp.path(), "hetz.demo.ding", "hetz", &runner).unwrap();

    assert_eq!(runner.list_calls.get(), 1);
    assert_eq!(report.held, ["hetz.demo.ding"]);
    assert!(report.launched.is_empty());
    assert!(runner.spawned.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
}

#[test]
fn selected_one_shot_live_adopts_without_actions() {
    let runner = FakeRunner {
        sessions: vec![
            live("host.agent.work"),
            live("host.agent.side"),
            live("host.sibling.work"),
        ],
        ..Default::default()
    };
    let specs = vec![
        two_task_spec("agent", "host.agent.work", "host.agent.side"),
        task_spec("sibling", None, "host.sibling.work"),
    ];
    let report = up_once_selected_specs(
        Path::new("/tmp"),
        &specs,
        "host.agent.work",
        "host",
        &runner,
    )
    .unwrap();
    assert_eq!(runner.list_calls.get(), 1);
    assert!(runner.spawned.borrow().is_empty());
    assert!(runner.killed.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
    assert_eq!(report.adopted, ["agent"]);
}

#[test]
fn selected_one_shot_reports_a_dead_task_only_as_restarted() {
    let runner = FakeRunner {
        sessions: vec![
            dead("host.agent.work"),
            live("host.agent.side"),
            live("host.sibling.work"),
        ],
        ..Default::default()
    };
    let specs = vec![
        two_task_spec("agent", "host.agent.work", "host.agent.side"),
        task_spec("sibling", None, "host.sibling.work"),
    ];
    let report = up_once_selected_specs(
        Path::new("/tmp"),
        &specs,
        "host.agent.work",
        "host",
        &runner,
    )
    .unwrap();
    assert_eq!(runner.list_calls.get(), 1);
    assert_eq!(runner.reaped.borrow().as_slice(), ["host.agent.work"]);
    assert_eq!(runner.spawned.borrow().as_slice(), ["host.agent.work"]);
    assert!(runner.killed.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
    assert!(report.launched.is_empty());
    assert_eq!(report.restarted, ["host.agent.work"]);
    assert!(report.gc.is_empty());
}

#[test]
fn selected_one_shot_second_live_pass_is_a_noop() {
    let runner = FakeRunner {
        sessions: vec![live("host.agent.work"), live("host.sibling.work")],
        ..Default::default()
    };
    let specs = vec![
        task_spec("agent", None, "host.agent.work"),
        task_spec("sibling", None, "host.sibling.work"),
    ];
    let report = up_once_selected_specs(
        Path::new("/tmp"),
        &specs,
        "host.agent.work",
        "host",
        &runner,
    )
    .unwrap();
    assert_eq!(runner.list_calls.get(), 1);
    assert!(runner.spawned.borrow().is_empty());
    assert!(runner.killed.borrow().is_empty());
    assert!(runner.reaped.borrow().is_empty());
    assert!(runner.removed.borrow().is_empty());
    assert_eq!(report.adopted, ["agent"]);
}

#[test]
fn selected_one_shot_ambiguous_refuses_before_runner_list() {
    let runner = FakeRunner::default();
    let specs = vec![task_spec("one", None, "dup"), task_spec("two", None, "dup")];
    let error =
        up_once_selected_specs(Path::new("/tmp"), &specs, "dup", "host", &runner).unwrap_err();
    assert!(error.to_string().contains("ambiguous"), "{error}");
    assert_refusal(&runner);
}

#[test]
fn selected_one_shot_wrong_host_refuses_before_runner_list() {
    let runner = FakeRunner::default();
    let specs = vec![task_spec("remote", Some("other"), "other.remote.work")];
    let error = up_once_selected_specs(
        Path::new("/tmp"),
        &specs,
        "other.remote.work",
        "host",
        &runner,
    )
    .unwrap_err();
    assert!(error.to_string().contains("did not resolve"));
    assert_refusal(&runner);
}
use st2::park::{DirParkObserver, ParkObserver, ParkProjection, ParkState};
use st2::run::{grant_unpark_requests, publish_parks};
use st2::{FlappingCap, UpReport, discover, down, execute, reconcile as reconcile_result, up_once};

fn reconcile<'a>(specs: &'a [AgentSpec], sessions: &[Session], host: &str) -> ReconcilePlan<'a> {
    reconcile_result(specs, sessions, host).unwrap()
}

#[derive(Default)]
struct FakeRunner {
    list_calls: Cell<usize>,
    sessions: Vec<Session>,
    fail_list: bool,
    fail_spawn: Option<String>,
    fail_reap: Option<String>,
    fail_kill: Option<String>,
    spawned: RefCell<Vec<String>>,
    spawned_targets: RefCell<Vec<TaskTarget>>,
    spawn_dirs: RefCell<Vec<(String, String)>>,
    killed: RefCell<Vec<String>>,
    reaped: RefCell<Vec<String>>,
    removed: RefCell<Vec<String>>,
    patched: RefCell<Vec<String>>,
    ops: RefCell<Vec<String>>,
    inbox_expected_during_kill: Option<std::path::PathBuf>,
}

impl Runner for FakeRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.list_calls.set(self.list_calls.get() + 1);
        if self.fail_list {
            anyhow::bail!("simulated list failure");
        }
        Ok(self.sessions.clone())
    }
    fn spawn(&self, target: &TaskTarget, spec_dir: &Path) -> anyhow::Result<()> {
        if self.fail_spawn.as_deref() == Some(target.pty_id.as_str()) {
            anyhow::bail!("simulated spawn failure");
        }
        self.ops
            .borrow_mut()
            .push(format!("spawn:{}", target.pty_id));
        self.spawned.borrow_mut().push(target.pty_id.clone());
        self.spawned_targets.borrow_mut().push(target.clone());
        self.spawn_dirs
            .borrow_mut()
            .push((target.pty_id.clone(), spec_dir.display().to_string()));
        Ok(())
    }
    fn kill(&self, pty_id: &str) -> anyhow::Result<()> {
        self.ops.borrow_mut().push(format!("kill:{pty_id}"));
        if let Some(inbox) = &self.inbox_expected_during_kill {
            assert!(
                inbox.exists(),
                "retirement must stop live work before archiving the inbox"
            );
        }
        if self.fail_kill.as_deref() == Some(pty_id) {
            anyhow::bail!("simulated kill failure");
        }
        self.killed.borrow_mut().push(pty_id.to_string());
        Ok(())
    }
    fn patch_presentation(&self, presentation: &PtyPresentation) -> anyhow::Result<()> {
        self.ops
            .borrow_mut()
            .push(format!("patch:{}", presentation.pty_id));
        self.patched.borrow_mut().push(presentation.pty_id.clone());
        Ok(())
    }
    fn reap_for_restart(&self, pty_id: &str) -> anyhow::Result<()> {
        self.ops.borrow_mut().push(format!("reap:{pty_id}"));
        self.reaped.borrow_mut().push(pty_id.to_string());
        if self.fail_reap.as_deref() == Some(pty_id) {
            anyhow::bail!("reap broke");
        }
        Ok(())
    }
    fn remove(&self, pty_id: &str) -> anyhow::Result<()> {
        self.removed.borrow_mut().push(pty_id.to_string());
        Ok(())
    }
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn live(id: &str) -> Session {
    Session {
        pty_id: id.to_string(),
        alive: true,
        exit_code: None,
        presentation: None,
    }
}
fn dead(id: &str) -> Session {
    Session {
        pty_id: id.to_string(),
        alive: false,
        exit_code: None,
        presentation: None,
    }
}

/// The restart cap forgives a `mode = fail` budget on observed uptime, so the plan has to carry
/// positive evidence of liveness instead of leaving `execute` to infer it from an empty launch set.
/// The two cases below are indistinguishable by that inference — one task is alive, the other was
/// simply never part of the pass — which is exactly why the plan states it.
#[test]
fn the_plan_reports_only_the_tasks_it_proved_alive() {
    let up = task_spec("up", None, "host.up.work");
    let gone = task_spec("gone", None, "host.gone.work");

    let sessions = [live("host.up.work"), dead("host.gone.work")];

    let both = [up, gone.clone()];
    let plan = reconcile(&both, &sessions, "host");
    assert_eq!(plan.live, ["host.up.work"]);

    // The pass never reached `up` (its owner failed to materialize, so it was filtered out before
    // reconcile). Its session is still alive in the snapshot, and it is still not this plan's
    // business to say so.
    let reduced = [gone];
    let plan = reconcile(&reduced, &sessions, "host");
    assert!(
        plan.live.is_empty(),
        "a task the pass did not consider is unobserved, not alive: {:?}",
        plan.live
    );
}

#[test]
fn lifecycle_work_precedes_a_bounded_presentation_batch() {
    let spec = task_spec("owner", None, "host.owner.work");
    let target = TaskTarget {
        kind: TaskKind::Exec,
        pty_id: "host.owner.work".to_owned(),
        bus_id: "host.owner".to_owned(),
        name: "work".to_owned(),
        derived: false,
        launch: TaskLaunch::Shell("true".to_owned()),
        cwd: None,
        workspace: None,
        tags: BTreeMap::new(),
        env: BTreeMap::new(),
        keep: false,
        presentation: None,
    };
    let presentation = (0..10)
        .rev()
        .map(|index| PtyPresentation {
            pty_id: format!("host.presented.{index}"),
            display_name: Some(Some(format!("Presented {index}"))),
            tags: BTreeMap::new(),
        })
        .collect();
    let plan = ReconcilePlan {
        launch: vec![Launch {
            spec: &spec,
            tasks: vec![target],
            live_derived: Vec::new(),
        }],
        teardown: vec![Teardown {
            spec: &spec,
            pty_ids: vec!["host.retired.work".to_owned()],
        }],
        presentation,
        ..ReconcilePlan::default()
    };
    let runner = FakeRunner::default();
    let mut report = UpReport::default();
    execute(&plan, &runner, &mut FlappingCap::default(), &mut report);

    assert_eq!(
        &runner.ops.borrow()[..2],
        ["spawn:host.owner.work", "kill:host.retired.work"]
    );
    assert_eq!(
        *runner.patched.borrow(),
        (0..8)
            .map(|index| format!("host.presented.{index}"))
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("deferred 2 presentation patches"))
    );
    assert!(report.is_noteworthy());
}

/// A v2 service job: a pty agent + an exec ding.
const AGENT: &str = r#"
identity = "demo"
type = "service"
[pty.agent]
id = "hetz.demo-claude"
command = "exec claude 'boot'"
[exec.ding]
id = "hetz.demo.ding"
command = "st2 ding hetz.demo"
"#;

const COMPACT_AGENT_WITH_DING: &str = r#"
agent "demo" {
  host "hetz"
  command "exit 24"
  ding
  restart { attempts 1; interval "60s"; delay "0s"; mode "fail" }
}
"#;

const COMPACT_ADOPT_ONLY_AGENT_WITH_DING: &str = r#"
agent "demo" {
  host "hetz"
  command "true"
  lifecycle "adopt-only"
  ding
}
"#;

const EXPLICIT_RUNNER_IDENTITY_AGENT: &str = r#"
agent "demo" {
  host "hetz"
  pty "agent" { id "hetz.demo"; command "true" }
  pty "shell" { id "hetz.demo.shell"; command "true" }
  exec "sidecar" { id "hetz.demo.sidecar"; command "true" }
  exec "matched" {
    id "hetz.demo.matched"
    command "true"
    env { ST_AGENT "hetz.demo" }
  }
}
"#;

#[test]
fn runner_owned_identity_injects_compact_and_explicit_task_omissions_and_accepts_a_match() {
    for (source, expected_ids) in [
        (COMPACT_AGENT_WITH_DING, vec!["hetz.demo", "hetz.demo.ding"]),
        (
            EXPLICIT_RUNNER_IDENTITY_AGENT,
            vec![
                "hetz.demo",
                "hetz.demo.matched",
                "hetz.demo.shell",
                "hetz.demo.sidecar",
            ],
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "agents/hetz/demo/agent.kdl", source);
        let runner = FakeRunner::default();

        let report = up_once(tmp.path(), "hetz", &runner).unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(runner.spawned.borrow().as_slice(), expected_ids);
        for target in runner.spawned_targets.borrow().iter() {
            assert_eq!(
                target.env.get("ST_AGENT").map(String::as_str),
                Some("hetz.demo"),
                "task {}",
                target.pty_id
            );
        }
    }
}

#[test]
fn runner_owned_identity_metadata_is_form_equivalent_and_role_scoped() {
    let compact = tempfile::tempdir().unwrap();
    write(
        compact.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let compact_runner = FakeRunner::default();
    up_once(compact.path(), "hetz", &compact_runner).unwrap();

    let explicit = tempfile::tempdir().unwrap();
    write(
        explicit.path(),
        "agents/hetz/demo/agent.kdl",
        EXPLICIT_RUNNER_IDENTITY_AGENT,
    );
    let explicit_runner = FakeRunner::default();
    up_once(explicit.path(), "hetz", &explicit_runner).unwrap();

    let compact_targets = compact_runner.spawned_targets.borrow();
    let explicit_targets = explicit_runner.spawned_targets.borrow();
    let compact_agent = compact_targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo")
        .unwrap();
    let explicit_agent = explicit_targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo")
        .unwrap();
    assert_eq!(compact_agent.presentation, explicit_agent.presentation);
    assert_eq!(compact_agent.tags, explicit_agent.tags);

    let primary_tags = &compact_agent.presentation.as_ref().unwrap().tags;
    assert_eq!(
        primary_tags.get("agent.actor.path"),
        Some(&Some("hetz.demo".to_owned()))
    );
    assert_eq!(primary_tags.get("role"), Some(&Some("agent".to_owned())));
    assert!(!primary_tags.contains_key("run.role"));

    let secondary = explicit_targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo.shell")
        .unwrap();
    let secondary_tags = &secondary.presentation.as_ref().unwrap().tags;
    assert_eq!(
        secondary_tags.get("agent.actor.path"),
        Some(&Some("hetz.demo".to_owned()))
    );
    assert_eq!(secondary_tags.get("role"), Some(&None));
    assert!(!secondary_tags.contains_key("run.role"));

    let sidecar = explicit_targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo.sidecar")
        .unwrap();
    assert_eq!(sidecar.kind, TaskKind::Exec);
    assert!(sidecar.presentation.is_none());
    assert!(sidecar.tags.is_empty());
}

#[test]
fn runner_owned_identity_conflict_refuses_before_materialization_or_runner_access() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        &format!(
            r#"agent "demo" {{
  host "hetz"
  workspace "{}"
  command "true"
  env {{ ST_AGENT "wrong.actor" }}
  render {{ file "IDENTITY" "$ST_AGENT" }}
}}
"#,
            workspace.display()
        ),
    );
    let runner = FakeRunner::default();

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(runner.list_calls.get(), 0);
    assert!(runner.spawned.borrow().is_empty());
    assert!(!workspace.join("IDENTITY").exists());
    assert_eq!(
        report.errors,
        [
            "agent 'hetz.demo' task 'agent' declares conflicting ST_AGENT 'wrong.actor'; expected runner-owned value 'hetz.demo'"
        ]
    );
}

#[test]
fn selected_identity_validation_includes_conflicting_active_siblings() {
    let selected = task_spec("selected", Some("host"), "host.selected.work");
    let mut sibling = task_spec("sibling", Some("host"), "host.sibling.work");
    sibling.tasks[0]
        .env
        .insert("ST_AGENT".into(), "wrong.actor".into());
    let runner = FakeRunner::default();

    let error = up_once_selected_specs(
        Path::new("/tmp"),
        &[selected, sibling],
        "host.selected.work",
        "host",
        &runner,
    )
    .unwrap_err();

    assert!(error.to_string().contains("conflicting ST_AGENT"));
    assert_eq!(runner.list_calls.get(), 0);
    assert!(runner.spawned.borrow().is_empty());
}

#[test]
fn retired_identity_conflict_does_not_block_stale_task_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/retired/agent.kdl",
        r#"agent "retired" {
  host "hetz"
  retired #true
  command "true"
  env { ST_AGENT "stale.wrong" }
}"#,
    );
    let runner = FakeRunner {
        sessions: vec![live("hetz.retired")],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(runner.list_calls.get(), 1);
    assert_eq!(runner.killed.borrow().as_slice(), ["hetz.retired"]);
    assert!(runner.spawned.borrow().is_empty());
}

#[test]
fn runner_owned_identity_is_rederived_for_dead_task_replay() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let first = FakeRunner::default();
    up_once(tmp.path(), "hetz", &first).unwrap();
    let replay = FakeRunner {
        sessions: vec![dead("hetz.demo"), dead("hetz.demo.ding")],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &replay).unwrap();

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        replay.reaped.borrow().as_slice(),
        ["hetz.demo", "hetz.demo.ding"]
    );
    for target in replay.spawned_targets.borrow().iter() {
        assert_eq!(
            target.env.get("ST_AGENT").map(String::as_str),
            Some("hetz.demo"),
            "task {}",
            target.pty_id
        );
    }
    assert_eq!(
        first
            .spawned_targets
            .borrow()
            .iter()
            .map(|target| (&target.pty_id, target.env.get("ST_AGENT")))
            .collect::<Vec<_>>(),
        replay
            .spawned_targets
            .borrow()
            .iter()
            .map(|target| (&target.pty_id, target.env.get("ST_AGENT")))
            .collect::<Vec<_>>()
    );
}

#[test]
fn up_once_launches_all_tasks_of_a_fresh_agent() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);

    let runner = FakeRunner::default();
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    let mut launched = report.launched.clone();
    launched.sort();
    assert_eq!(launched, vec!["hetz.demo-claude", "hetz.demo.ding"]);
    assert!(report.restarted.is_empty());
    assert!(report.gc.is_empty());
    assert!(report.errors.is_empty());
    let dirs = runner.spawn_dirs.borrow();
    assert!(dirs.iter().all(|(_, d)| d.ends_with("agents/hetz/demo")));
    let targets = runner.spawned_targets.borrow();
    let authored_ding = targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo.ding")
        .unwrap();
    assert_eq!(
        &authored_ding.launch,
        &TaskLaunch::Shell("st2 ding hetz.demo".into())
    );
}

#[test]
fn fresh_compact_agent_launches_with_its_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );

    let runner = FakeRunner::default();
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(report.launched, ["hetz.demo", "hetz.demo.ding"]);
    let targets = runner.spawned_targets.borrow();
    let ding = targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo.ding")
        .unwrap();
    assert_eq!(
        &ding.launch,
        &TaskLaunch::Argv(vec![
            std::env::current_exe().unwrap().display().to_string(),
            "ding".into(),
            "--identity".into(),
            "hetz.demo".into(),
            "--root".into(),
            tmp.path().display().to_string(),
        ])
    );
}

#[test]
fn absent_adopt_only_compact_agent_holds_its_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_ADOPT_ONLY_AGENT_WITH_DING,
    );

    let report = up_once(tmp.path(), "hetz", &FakeRunner::default()).unwrap();

    assert_eq!(report.held, ["hetz.demo"]);
    assert!(report.launched.is_empty());
}

#[test]
fn held_adopt_only_compact_agent_stops_its_live_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_ADOPT_ONLY_AGENT_WITH_DING,
    );
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.ding")],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(report.held, ["hetz.demo"]);
    assert_eq!(report.torn_down, ["hetz.demo.ding"]);
    assert!(report.launched.is_empty());
}

#[test]
fn up_once_adopts_when_all_tasks_already_live() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        sessions: vec![live("hetz.demo-claude"), live("hetz.demo.ding")],
        ..Default::default()
    };
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert!(report.launched.is_empty());
    assert_eq!(report.adopted, vec!["demo"]);
}

#[test]
fn up_once_launches_only_the_missing_task() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        sessions: vec![live("hetz.demo-claude")],
        ..Default::default()
    };
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert_eq!(report.launched, vec!["hetz.demo.ding"]);
}

#[test]
fn up_once_tears_down_a_retired_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let retired = r#"
identity = "demo"
retired = true
[pty.agent]
id = "hetz.demo-claude"
command = "exec claude 'boot'"
[exec.ding]
id = "hetz.demo.ding"
command = "st2 ding hetz.demo"
"#;
    write(tmp.path(), "agents/hetz/demo/agent.toml", retired);
    let runner = FakeRunner {
        sessions: vec![live("hetz.demo-claude"), live("hetz.demo.ding")],
        ..Default::default()
    };
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    let mut torn = report.torn_down.clone();
    torn.sort();
    assert_eq!(torn, vec!["hetz.demo-claude", "hetz.demo.ding"]);
    assert!(report.launched.is_empty());
}

#[test]
fn retired_agent_idempotently_archives_every_inbox_message() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agents/hetz/demo");
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        r#"agent "demo" {
  host "hetz"
  desired-state "retired" reason="Work complete"
  command "true"
}"#,
    );
    let filename = "1784649988123-abc23z.md";
    let inbox = message::inbox_dir(&agent_dir);
    let archive = message::archive_dir(&agent_dir);
    fs::create_dir_all(&inbox).unwrap();
    fs::write(inbox.join(filename), "first receipt").unwrap();
    let runner = FakeRunner {
        sessions: vec![live("hetz.demo")],
        inbox_expected_during_kill: Some(inbox.join(filename)),
        ..Default::default()
    };

    let first = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(
        fs::read_to_string(archive.join(filename)).unwrap(),
        "first receipt"
    );
    assert!(!inbox.join(filename).exists());

    fs::write(inbox.join(filename), "restored duplicate").unwrap();
    let failing_runner = FakeRunner {
        sessions: vec![live("hetz.demo")],
        fail_kill: Some("hetz.demo".into()),
        inbox_expected_during_kill: Some(inbox.join(filename)),
        ..Default::default()
    };
    let second = up_once(tmp.path(), "hetz", &failing_runner).unwrap();
    assert!(
        second
            .errors
            .iter()
            .any(|error| error.contains("kill hetz.demo")),
        "{:?}",
        second.errors
    );
    assert_eq!(
        fs::read_to_string(archive.join(filename)).unwrap(),
        "first receipt",
        "the durable archive receipt must never be overwritten"
    );
    assert_eq!(
        fs::read_to_string(inbox.join(filename)).unwrap(),
        "restored duplicate",
        "failed teardown must leave the inbox available to the still-live agent"
    );

    let retry_runner = FakeRunner {
        sessions: vec![live("hetz.demo")],
        inbox_expected_during_kill: Some(inbox.join(filename)),
        ..Default::default()
    };
    let retry = up_once(tmp.path(), "hetz", &retry_runner).unwrap();
    assert!(retry.errors.is_empty(), "{:?}", retry.errors);
    assert!(!inbox.join(filename).exists());
    assert_eq!(
        fs::read_to_string(archive.join(filename)).unwrap(),
        "first receipt"
    );
}

#[test]
fn retired_compact_agent_stops_agent_and_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    let retired =
        COMPACT_AGENT_WITH_DING.replacen("  host \"hetz\"", "  host \"hetz\"\n  retired #true", 1);
    write(tmp.path(), "agents/hetz/demo/agent.kdl", &retired);
    let runner = FakeRunner {
        sessions: vec![live("hetz.demo"), live("hetz.demo.ding")],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(report.torn_down, ["hetz.demo", "hetz.demo.ding"]);
    assert!(report.launched.is_empty());
}

#[test]
fn suspend_and_resume_cover_derived_ding_sibling_continuity_and_inbox_retention() {
    let tmp = tempfile::tempdir().unwrap();
    let running = COMPACT_AGENT_WITH_DING;
    let suspended = running.replacen(
        "  host \"hetz\"",
        "  host \"hetz\"\n  desired-state \"suspended\" reason=\"Waiting for capacity\"",
        1,
    );
    write(tmp.path(), "agents/hetz/demo/agent.kdl", &suspended);
    write(
        tmp.path(),
        "agents/hetz/sibling/agent.kdl",
        "agent \"sibling\" { host \"hetz\"; command \"true\" }\n",
    );
    write(
        tmp.path(),
        "agents/hetz/demo/resources/inbox/1234567890000-proof.md",
        "---\nfrom: hetz.sibling\n---\nretained\n",
    );
    let suspend_runner = FakeRunner {
        sessions: vec![
            live("hetz.demo"),
            live("hetz.demo.ding"),
            live("hetz.sibling"),
        ],
        ..Default::default()
    };

    let suspended_report = up_once(tmp.path(), "hetz", &suspend_runner).unwrap();
    assert_eq!(suspended_report.torn_down, ["hetz.demo", "hetz.demo.ding"]);
    assert_eq!(suspended_report.adopted, ["sibling"]);
    assert!(suspended_report.launched.is_empty());
    assert!(
        tmp.path()
            .join("agents/hetz/demo/resources/inbox/1234567890000-proof.md")
            .is_file()
    );

    write(tmp.path(), "agents/hetz/demo/agent.kdl", running);
    let resume_runner = FakeRunner {
        sessions: vec![
            dead("hetz.demo"),
            dead("hetz.demo.ding"),
            live("hetz.sibling"),
        ],
        ..Default::default()
    };
    let resumed_report = up_once(tmp.path(), "hetz", &resume_runner).unwrap();
    assert_eq!(resumed_report.restarted, ["hetz.demo", "hetz.demo.ding"]);
    assert_eq!(resumed_report.adopted, ["sibling"]);
    assert!(
        tmp.path()
            .join("agents/hetz/demo/resources/inbox/1234567890000-proof.md")
            .is_file()
    );
}

#[test]
fn up_once_skips_other_host_specs() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/here/agent.toml",
        "identity=\"here\"\n[pty.agent]\ncommand=\"x\"\n",
    );
    write(
        tmp.path(),
        "agents/silber/there/agent.toml",
        "identity=\"there\"\n[pty.agent]\ncommand=\"y\"\n",
    );
    let runner = FakeRunner::default();
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert_eq!(report.launched.len(), 1);
    assert_eq!(report.other_host, vec!["there"]);
}

#[test]
fn up_once_collects_spawn_errors_without_aborting() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        fail_spawn: Some("hetz.demo-claude".into()),
        ..Default::default()
    };
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert_eq!(report.launched, vec!["hetz.demo.ding"]);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("hetz.demo-claude"));
}

#[test]
fn up_once_reports_a_successful_replacement_only_as_restarted() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo-claude"), dead("hetz.demo.ding")],
        ..Default::default()
    };
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    let mut reaped = runner.reaped.borrow().clone();
    reaped.sort();
    assert_eq!(reaped, vec!["hetz.demo-claude", "hetz.demo.ding"]);
    assert!(
        runner.removed.borrow().is_empty(),
        "a restart must not remove final retirement state"
    );
    assert!(report.launched.is_empty());
    let mut restarted = report.restarted.clone();
    restarted.sort();
    assert_eq!(restarted, vec!["hetz.demo-claude", "hetz.demo.ding"]);
    assert!(
        report.gc.is_empty(),
        "a successful restart must not be reported as final garbage collection"
    );
    assert_eq!(
        runner.ops.borrow().as_slice(),
        [
            "reap:hetz.demo-claude",
            "spawn:hetz.demo-claude",
            "reap:hetz.demo.ding",
            "spawn:hetz.demo.ding",
        ],
        "st2 must reap each dead record before it starts the replacement"
    );
}

#[test]
fn up_once_does_not_restart_a_task_when_diagnostic_reap_fails() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo-claude"), dead("hetz.demo.ding")],
        fail_reap: Some("hetz.demo-claude".into()),
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert!(report.launched.is_empty());
    assert_eq!(report.restarted, vec!["hetz.demo.ding"]);
    assert!(report.gc.is_empty());
    assert_eq!(runner.spawned.borrow().as_slice(), ["hetz.demo.ding"]);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error == "reap hetz.demo-claude for restart: reap broke")
    );
}

#[test]
fn up_once_does_not_report_failed_replacement_as_restarted() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo-claude"), live("hetz.demo.ding")],
        fail_spawn: Some("hetz.demo-claude".into()),
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(
        runner.reaped.borrow().as_slice(),
        ["hetz.demo-claude"],
        "st2 must reap the stale record before it starts a replacement"
    );
    assert!(report.launched.is_empty());
    assert!(report.restarted.is_empty());
    assert!(report.gc.is_empty());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error == "spawn hetz.demo-claude: simulated spawn failure")
    );
}

#[test]
fn failed_compact_agent_restart_stops_its_live_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.ding")],
        fail_spawn: Some("hetz.demo".into()),
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(report.torn_down, ["hetz.demo.ding"]);
    assert_eq!(runner.killed.borrow().as_slice(), ["hetz.demo.ding"]);
}

#[test]
fn failed_compact_agent_reap_stops_its_live_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.ding")],
        fail_reap: Some("hetz.demo".into()),
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(report.torn_down, ["hetz.demo.ding"]);
    assert_eq!(runner.killed.borrow().as_slice(), ["hetz.demo.ding"]);
}

#[test]
fn up_once_finally_removes_dead_retired_tasks_without_restarting_them() {
    let tmp = tempfile::tempdir().unwrap();
    let retired = AGENT.replacen(
        "type = \"service\"",
        "type = \"service\"\nretired = true\nkeep = true",
        1,
    );
    write(tmp.path(), "agents/hetz/demo/agent.toml", &retired);
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo-claude"), dead("hetz.demo.ding")],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    let mut removed = runner.removed.borrow().clone();
    removed.sort();
    assert_eq!(removed, vec!["hetz.demo-claude", "hetz.demo.ding"]);
    assert!(runner.reaped.borrow().is_empty());
    assert!(report.launched.is_empty());
    assert!(report.restarted.is_empty());
    assert_eq!(report.gc.len(), 2);
}

#[test]
fn flapping_cap_parks_a_fail_mode_task_that_keeps_dying() {
    let tmp = tempfile::tempdir().unwrap();
    // mode=fail → parks after attempts (the default mode=delay would rate-limit instead).
    write(
        tmp.path(),
        "agents/hetz/demo/agent.toml",
        "identity=\"demo\"\nsupervisor=\"cos-claude\"\n[restart]\nattempts=3\ninterval=\"60s\"\nmode=\"fail\"\n[pty.agent]\nid=\"hetz.demo-claude\"\ncommand=\"x\"\n",
    );
    let found = discover(tmp.path());
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo-claude")],
        ..Default::default()
    };
    let mut cap = FlappingCap::default();

    let mut last = UpReport::default();
    for _ in 0..5 {
        let plan = reconcile(&found.specs, &runner.sessions, "hetz");
        last = UpReport::default();
        execute(&plan, &runner, &mut cap, &mut last);
    }
    assert_eq!(last.launched.len(), 0);
    assert_eq!(last.flapping, vec!["hetz.demo-claude"]);
    assert!(last.gc.is_empty(), "parked flapper keeps its corpse");
    assert_eq!(runner.spawned.borrow().len(), 3); // limit
    assert_eq!(runner.reaped.borrow().len(), 3);
    assert!(
        runner.removed.borrow().is_empty(),
        "crash-loop handling must never take the final-retirement cleanup path"
    );

    // The rich crash-loop record carries what surfacing needs — the parked task, its agent, and the
    // supervisor to notify — recorded once (not per pass).
    assert_eq!(last.crash_loops.len(), 1);
    let cl = &last.crash_loops[0];
    assert_eq!(cl.pty_id, "hetz.demo-claude");
    assert_eq!(cl.identity, "demo");
    assert_eq!(cl.supervisor.as_deref(), Some("cos-claude"));
    assert_eq!(cl.agent_bus_id("hetz"), "hetz.demo");
}

#[test]
fn parked_compact_agent_stops_its_live_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let found = discover(tmp.path());
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.ding")],
        ..Default::default()
    };
    let mut cap = FlappingCap::default();

    let first = reconcile(&found.specs, &runner.sessions, "hetz");
    execute(&first, &runner, &mut cap, &mut UpReport::default());
    let second = reconcile(&found.specs, &runner.sessions, "hetz");
    let mut report = UpReport::default();
    execute(&second, &runner, &mut cap, &mut report);

    assert_eq!(report.flapping, ["hetz.demo"]);
    assert_eq!(runner.killed.borrow().as_slice(), ["hetz.demo.ding"]);
}

#[test]
fn parked_compact_agent_does_not_relaunch_its_exited_derived_ding() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    let found = discover(tmp.path());
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), dead("hetz.demo.ding")],
        ..Default::default()
    };
    let mut cap = FlappingCap::default();
    cap.record("hetz.demo", Instant::now());

    let plan = reconcile(&found.specs, &runner.sessions, "hetz");
    let mut report = UpReport::default();
    execute(&plan, &runner, &mut cap, &mut report);

    assert_eq!(report.flapping, ["hetz.demo"]);
    assert!(
        !runner
            .spawned
            .borrow()
            .contains(&"hetz.demo.ding".to_string())
    );
}

/// #204 at the supervisor's own seam: a fail-mode task crash-loops into a terminal park, the park
/// becomes legible, an operator clears exactly that task, and it comes back — with a healthy peer
/// never touched, which is what "without restarting the supervisor" means in operations.
///
/// The peer is the negative control. A host-wide restart would have cold-booted it too, so the claim
/// that recovery is targeted is exactly the claim that nothing in the runner's op log names it across
/// either phase. That is a stronger check than comparing a pid, because it would also catch a kill
/// followed by an identical relaunch.
///
/// One `FlappingCap` spans both phases deliberately. Parking is scoped to a supervisor run, so a test
/// that built a fresh cap per phase would be testing the restart it is supposed to avoid — and
/// `up_once` cannot be used here at all for the same reason: both single-pass entry points construct
/// `FlappingCap::default()`, so a one-shot reconcile can never park anything.
#[test]
fn an_operator_recovers_one_parked_task_without_disturbing_a_healthy_peer() {
    let tmp = tempfile::tempdir().unwrap();
    // The projection and request dirs are temporaries, never this machine's real state dir: a test
    // that reached the live path could clear a real operator's park.
    let state = tempfile::tempdir().unwrap();
    let projection = ParkProjection::current(state.path().join("parked")).unwrap();
    let requests = state.path().join("unpark");

    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_DING,
    );
    write(
        tmp.path(),
        "agents/hetz/peer/agent.kdl",
        "agent \"peer\" {\n  host \"hetz\"\n  command \"serve\"\n}\n",
    );
    let found = discover(tmp.path());
    let mut cap = FlappingCap::default();

    // Phase 1 — the flapper dies before every pass; the peer is up and stays up.
    let crashing = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.ding"), live("hetz.peer")],
        ..Default::default()
    };
    let mut parked_report = UpReport::default();
    for _ in 0..4 {
        let plan = reconcile(&found.specs, &crashing.sessions, "hetz");
        parked_report = UpReport::default();
        execute(&plan, &crashing, &mut cap, &mut parked_report);
        publish_parks(&cap, &projection, &mut parked_report);
    }
    assert_eq!(parked_report.flapping, ["hetz.demo"]);
    assert!(cap.is_parked("hetz.demo"));

    // The park is legible to a separate reader — the entire point of #204.
    let observer = DirParkObserver::new(state.path().join("parked"));
    let batch = observer.observe(&["hetz.demo".to_string(), "hetz.peer".to_string()]);
    assert!(
        batch.complete,
        "a park is a known fault, not missing evidence"
    );
    let ParkState::Parked(record) = batch.state("hetz.demo") else {
        panic!("the parked task is not visible in the projection");
    };
    assert_eq!(record.reason, st2::run::PARK_REASON);
    assert_eq!(batch.state("hetz.peer"), &ParkState::NotParked);

    assert_eq!(
        lifecycle_ops(&crashing, "hetz.peer"),
        Vec::<String>::new(),
        "the healthy peer was restarted while its neighbour crash-looped"
    );

    // The operator fixes the cause and clears this one task.
    st2::park::request_unpark(&requests, "hetz.demo").unwrap();

    // Phase 2 — same supervisor run, same cap. The fixed task now comes up and stays up.
    let recovered = FakeRunner {
        sessions: vec![dead("hetz.demo"), dead("hetz.demo.ding"), live("hetz.peer")],
        ..Default::default()
    };
    let mut report = UpReport::default();
    grant_unpark_requests(&mut cap, &requests, &mut report);
    assert_eq!(report.unparked, ["hetz.demo"]);
    let plan = reconcile(&found.specs, &recovered.sessions, "hetz");
    execute(&plan, &recovered, &mut cap, &mut report);
    publish_parks(&cap, &projection, &mut report);

    assert!(
        report.flapping.is_empty(),
        "the granted task was still refused a launch"
    );
    let spawned = recovered.spawned.borrow().clone();
    assert!(
        spawned.contains(&"hetz.demo".to_string()),
        "the recovery did not actually relaunch the task: {spawned:?}"
    );
    // A recovery that brings the agent back but leaves its DING dead would silently stop delivering
    // that agent's messages — the derived companion is suppressed *because* the agent is parked, so
    // clearing the park has to bring it back with the agent.
    assert!(
        spawned.contains(&"hetz.demo.ding".to_string()),
        "the derived ding stayed dead after its agent recovered: {spawned:?}"
    );
    assert_eq!(
        lifecycle_ops(&recovered, "hetz.peer"),
        Vec::<String>::new(),
        "recovering one task restarted the healthy peer, which is a host-wide restart by another name"
    );

    // The fault clears truthfully rather than lingering as a stale marker.
    let batch = observer.observe(&["hetz.demo".to_string()]);
    assert_eq!(batch.state("hetz.demo"), &ParkState::NotParked);
    assert!(batch.complete);

    // And it stays recovered past `interval` (60s here) rather than for one pass: uptime forgives the
    // budget, so a task that genuinely came back is not one failure away from parking again.
    let up = FakeRunner {
        sessions: vec![live("hetz.demo"), live("hetz.demo.ding"), live("hetz.peer")],
        ..Default::default()
    };
    for _ in 0..5 {
        let plan = reconcile(&found.specs, &up.sessions, "hetz");
        let mut report = UpReport::default();
        execute(&plan, &up, &mut cap, &mut report);
        publish_parks(&cap, &projection, &mut report);
        assert!(report.flapping.is_empty());
    }
    assert!(!cap.is_parked("hetz.demo"));
    assert!(
        lifecycle_ops(&up, "hetz.").is_empty(),
        "a settled fleet was churned: {:?}",
        up.ops.borrow()
    );
}

/// Spawns, kills, reaps and removals naming `id` — the ops that actually restart or destroy a task.
/// Presentation patching is deliberately excluded: it re-labels a live session and is not a restart,
/// so counting it would make an unrelated cosmetic write look like collateral damage.
fn lifecycle_ops(runner: &FakeRunner, id: &str) -> Vec<String> {
    runner
        .ops
        .borrow()
        .iter()
        .filter(|op| !op.starts_with("patch:") && op.contains(id))
        .cloned()
        .collect()
}

/// A granted unpark that names nothing parked must not be silently swallowed: an operator who typo'd
/// a task id has to learn that nothing was recovered, or they will wait forever for a task to return.
#[test]
fn an_unpark_request_for_a_task_that_is_not_parked_says_so() {
    let requests = tempfile::tempdir().unwrap();
    st2::park::request_unpark(requests.path(), "hetz.typo").unwrap();

    let mut cap = FlappingCap::default();
    let mut report = UpReport::default();
    grant_unpark_requests(&mut cap, requests.path(), &mut report);

    assert!(report.unparked.is_empty());
    assert_eq!(report.warnings.len(), 1);
    assert!(
        report.warnings[0].contains("hetz.typo"),
        "{:?}",
        report.warnings
    );
}

/// A parked crash-loop is surfaced to the agent's supervisor over the native bus: a `crash-loop`-tagged
/// message from the runner lands in the supervisor's inbox. (This is the M2.4 guarantee — a crash-loop
/// isn't only an stderr line an operator has to be watching.)
#[test]
fn surface_crash_loop_notifies_the_supervisor_over_the_bus() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.toml",
        "identity=\"demo\"\nsupervisor=\"cos-claude\"\n[pty.agent]\nid=\"hetz.demo-claude\"\ncommand=\"x\"\n",
    );
    write(
        tmp.path(),
        "agents/hetz/cos-claude/agent.toml",
        "identity=\"cos-claude\"\n[pty.agent]\nid=\"hetz.cos\"\ncommand=\"x\"\n",
    );

    let cl = CrashLoop {
        pty_id: "hetz.demo-claude".to_string(),
        identity: "demo".to_string(),
        host: Some("hetz".to_string()),
        supervisor: Some("cos-claude".to_string()),
    };
    surface_crash_loop(tmp.path(), "hetz", &cl);

    let inbox = message::inbox_dir(&tmp.path().join("agents/hetz/cos-claude"));
    let msgs = message::list_dir(&inbox).unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "supervisor gets exactly one crash-loop message"
    );
    let m = &msgs[0];
    assert_eq!(m.from.as_deref(), Some("st2.hetz")); // the runner is the sender
    assert_eq!(m.subject.as_deref(), Some("crash-loop: hetz.demo parked"));
    assert!(m.tags.contains(&"crash-loop".to_string()));
    assert!(
        m.body.contains("hetz.demo-claude"),
        "body names the parked task"
    );
}

/// A park whose cause is structural must not advise `st2 unpark`.
///
/// `unpark` relaunches the task, and a session socket path over the portable `sun_path` bound
/// fails the identical bind every time, so acting on that advice restarts the storm rather than
/// recovering anything. The notice says what has to change instead. The recoverable case keeps
/// the recovery verb, which is what makes this a distinction rather than a removal.
///
/// Both notices land in one inbox, and two sends inside the same millisecond are ordered by their
/// random filename suffix, so the notice under test is selected by which file is NEW rather than
/// by position. Reading the last entry made this test a coin flip that failed roughly a third of
/// the time.
#[test]
fn a_structurally_unrecoverable_park_does_not_advise_unpark() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.toml",
        "identity=\"demo\"\nsupervisor=\"cos\"\n[pty.agent]\nid=\"hetz.demo\"\ncommand=\"x\"\n",
    );
    write(
        tmp.path(),
        "agents/hetz/cos/agent.toml",
        "identity=\"cos\"\n[pty.agent]\nid=\"hetz.cos\"\ncommand=\"x\"\n",
    );

    // The recoverable case must be UNABLE to cross the bound under any resolved pty root, not
    // merely short enough under this one. Asserting it with the shipped predicate keeps the
    // control from being contaminated by the very condition under test: a root long enough to
    // blow even this id would otherwise make the control silently structural.
    let resolved_root = st2::run::effective_pty_root(tmp.path());
    assert!(
        st2::run::session_socket_overage(&resolved_root, "hetz.demo").is_none(),
        "fixture precondition: the recoverable task must be bindable under the resolved pty root \
         {}, or this test cannot tell the two notices apart",
        resolved_root.display()
    );

    let inbox = message::inbox_dir(&tmp.path().join("agents/hetz/cos"));
    let notify = |pty_id: &str| {
        let before = message::list_dir(&inbox)
            .unwrap()
            .into_iter()
            .map(|message| message.filename)
            .collect::<Vec<_>>();
        surface_crash_loop(
            tmp.path(),
            "hetz",
            &CrashLoop {
                pty_id: pty_id.to_string(),
                identity: "demo".to_string(),
                host: Some("hetz".to_string()),
                supervisor: Some("cos".to_string()),
            },
        );
        let mut fresh = message::list_dir(&inbox)
            .unwrap()
            .into_iter()
            .filter(|message| !before.contains(&message.filename))
            .collect::<Vec<_>>();
        assert_eq!(fresh.len(), 1, "one park sends exactly one notice");
        fresh.pop().expect("the notice was just counted").body
    };

    // Long enough that no resolved pty root can bring it under the limit.
    let unbindable = format!("hetz.{}", "a".repeat(200));
    let structural = notify(&unbindable);
    assert!(
        !structural.contains(&format!("st2 unpark {unbindable}")),
        "a structural park must not offer a recovery verb that reproduces it: {structural}"
    );
    assert!(
        structural.contains("`st2 unpark` cannot recover it"),
        "the notice must say why the recovery verb is not offered: {structural}"
    );
    assert!(
        structural.contains("exceeding the 104-byte portable limit by"),
        "the notice must state the overage: {structural}"
    );

    let recoverable = notify("hetz.demo");
    assert!(
        recoverable.contains("st2 unpark hetz.demo"),
        "an ordinary crash-loop keeps its recovery verb: {recoverable}"
    );
}

/// `st2 down` kills every LIVE task of THIS host's catalog agents (the explicit teardown), skips
/// other hosts, and is idempotent about already-dead tasks.
#[test]
fn down_tears_down_this_hosts_live_tasks_only() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.toml",
        "identity=\"demo\"\n[pty.agent]\nid=\"hetz.demo-claude\"\ncommand=\"x\"\n",
    );
    write(
        tmp.path(),
        "agents/hetz/dead/agent.toml",
        "identity=\"dead\"\n[pty.agent]\nid=\"hetz.dead\"\ncommand=\"x\"\n",
    );
    write(
        tmp.path(),
        "agents/silber/other/agent.toml",
        "identity=\"other\"\nhost=\"silber\"\n[pty.agent]\nid=\"silber.other\"\ncommand=\"x\"\n",
    );

    // demo is live, dead is dead, other belongs to another host + is live.
    let runner = FakeRunner {
        sessions: vec![
            live("hetz.demo-claude"),
            dead("hetz.dead"),
            live("silber.other"),
        ],
        ..Default::default()
    };
    let report = down(tmp.path(), "hetz", &runner).unwrap();

    // Only hetz's LIVE task is killed. The dead one isn't (idempotent); the other host is skipped.
    assert_eq!(runner.killed.borrow().as_slice(), ["hetz.demo-claude"]);
    assert_eq!(report.torn_down, vec!["hetz.demo-claude"]);
    assert!(report.other_host.contains(&"other".to_string()));
    assert!(report.errors.is_empty());
}

/// No supervisor → nothing is sent (and it doesn't panic); the stderr line is the only surface.
#[test]
fn surface_crash_loop_without_supervisor_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.toml",
        "identity=\"demo\"\n[pty.agent]\nid=\"hetz.demo-claude\"\ncommand=\"x\"\n",
    );
    let cl = CrashLoop {
        pty_id: "hetz.demo-claude".to_string(),
        identity: "demo".to_string(),
        host: Some("hetz".to_string()),
        supervisor: None,
    };
    // Must not panic; there is simply nobody to notify.
    surface_crash_loop(tmp.path(), "hetz", &cl);
}

#[test]
fn up_once_surfaces_discovery_errors_and_unrunnable() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/good/agent.toml",
        "identity=\"good\"\n[pty.agent]\ncommand=\"x\"\n",
    );
    write(
        tmp.path(),
        "agents/hetz/bad/agent.toml",
        "identity=\"b\"\nnot valid =",
    );
    write(
        tmp.path(),
        "agents/hetz/nr/agent.toml",
        "identity=\"nr\"\ntype=\"service\"\n",
    );
    let runner = FakeRunner::default();
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert_eq!(report.launched, vec!["hetz.good.agent"]);
    assert_eq!(report.unrunnable, vec!["nr"]);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("bad/agent.toml"));
}

#[test]
fn up_once_marks_a_list_failure_as_a_skipped_pass() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "agents/hetz/demo/agent.toml", AGENT);
    let runner = FakeRunner {
        fail_list: true,
        ..Default::default()
    };
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();
    assert!(report.skipped);
    assert!(report.launched.is_empty());
    assert!(runner.spawned.borrow().is_empty());
    assert_eq!(
        report.errors,
        vec!["list sessions (pass skipped): simulated list failure"]
    );
}

// ---------------------------------------------------------------------------------------------
// Streams (DQ1 spike) — a declared event SOURCE lowers to a derived exec companion, so it inherits
// the derived-companion lifecycle wholesale. Every test below is the derived-DING proof for the
// same guarantee, re-run against `stream-gh-ci`, plus the two claims a stream adds that DING does not:
// it must not make an otherwise-empty agent runnable, and it must not disturb its agent when it
// crash-loops.
// ---------------------------------------------------------------------------------------------

/// An agent with BOTH companions. Every stream test carries the ding too, so a claim about the stream
/// is also a claim that the two derived siblings stay independent.
const COMPACT_AGENT_WITH_STREAM: &str = r#"
agent "demo" {
  host "hetz"
  supervisor "cos-claude"
  command "true"
  ding
  stream "gh-ci" { command "poll-gh-ci.sh" }
  restart { attempts 1; interval "60s"; delay "0s"; mode "fail" }
}
"#;

const COMPACT_STREAM_ONLY_AGENT: &str = r#"
agent "sourceless" {
  host "hetz"
  stream "gh-ci" { command "poll-gh-ci.sh" }
}
"#;

/// 4a. One reconcile pass launches the agent and BOTH derived companions — no second pass, no
/// ordering ceremony at the call site, with the declared adapter launch carried through verbatim.
#[test]
fn fresh_compact_agent_launches_with_its_derived_stream() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_STREAM,
    );

    let runner = FakeRunner::default();
    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(
        report.launched,
        ["hetz.demo", "hetz.demo.ding", "hetz.demo.stream-gh-ci"]
    );
    let targets = runner.spawned_targets.borrow();
    let stream = targets
        .iter()
        .find(|target| target.pty_id == "hetz.demo.stream-gh-ci")
        .unwrap();
    assert_eq!(
        stream.kind,
        TaskKind::Exec,
        "a stream source needs no terminal"
    );
    assert!(stream.derived, "a stream companion is runner-generated");
    assert_eq!(&stream.launch, &TaskLaunch::Shell("poll-gh-ci.sh".into()));
    // Runner-owned task identity reaches the stream exactly as it reaches every other task.
    assert_eq!(
        stream.env.get("ST_AGENT").map(String::as_str),
        Some("hetz.demo")
    );
}

/// 4b (retire). Retirement tears down the agent and BOTH companions in the same pass.
#[test]
fn retired_compact_agent_stops_agent_and_derived_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let retired = COMPACT_AGENT_WITH_STREAM.replacen(
        "  host \"hetz\"",
        "  host \"hetz\"\n  retired #true",
        1,
    );
    write(tmp.path(), "agents/hetz/demo/agent.kdl", &retired);
    let runner = FakeRunner {
        sessions: vec![
            live("hetz.demo"),
            live("hetz.demo.ding"),
            live("hetz.demo.stream-gh-ci"),
        ],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(
        report.torn_down,
        ["hetz.demo", "hetz.demo.ding", "hetz.demo.stream-gh-ci"]
    );
    assert!(report.launched.is_empty());
}

/// 4b (suspend). A suspended agent stops its stream with it, and a sibling agent is untouched.
#[test]
fn suspended_compact_agent_stops_its_derived_stream_without_touching_a_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let suspended = COMPACT_AGENT_WITH_STREAM.replacen(
        "  host \"hetz\"",
        "  host \"hetz\"\n  desired-state \"suspended\" reason=\"Waiting for CI budget\"",
        1,
    );
    write(tmp.path(), "agents/hetz/demo/agent.kdl", &suspended);
    write(
        tmp.path(),
        "agents/hetz/sibling/agent.kdl",
        "agent \"sibling\" { host \"hetz\"; command \"true\" }\n",
    );
    let runner = FakeRunner {
        sessions: vec![
            live("hetz.demo"),
            live("hetz.demo.ding"),
            live("hetz.demo.stream-gh-ci"),
            live("hetz.sibling"),
        ],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(
        report.torn_down,
        ["hetz.demo", "hetz.demo.ding", "hetz.demo.stream-gh-ci"]
    );
    assert_eq!(report.adopted, ["sibling"]);
    assert!(report.launched.is_empty());
}

#[test]
fn suspend_and_resume_relaunch_the_agent_and_stream_together() {
    let tmp = tempfile::tempdir().unwrap();
    let suspended = COMPACT_AGENT_WITH_STREAM.replacen(
        "  host \"hetz\"",
        "  host \"hetz\"\n  desired-state \"suspended\" reason=\"Waiting for capacity\"",
        1,
    );
    write(tmp.path(), "agents/hetz/demo/agent.kdl", &suspended);
    let suspend_runner = FakeRunner {
        sessions: vec![
            live("hetz.demo"),
            live("hetz.demo.ding"),
            live("hetz.demo.stream-gh-ci"),
        ],
        ..Default::default()
    };
    let suspended_report = up_once(tmp.path(), "hetz", &suspend_runner).unwrap();
    assert_eq!(
        suspended_report.torn_down,
        ["hetz.demo", "hetz.demo.ding", "hetz.demo.stream-gh-ci"]
    );

    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_STREAM,
    );
    let resume_runner = FakeRunner {
        sessions: vec![
            dead("hetz.demo"),
            dead("hetz.demo.ding"),
            dead("hetz.demo.stream-gh-ci"),
        ],
        ..Default::default()
    };
    let resumed_report = up_once(tmp.path(), "hetz", &resume_runner).unwrap();
    assert_eq!(
        resumed_report.restarted,
        ["hetz.demo", "hetz.demo.ding", "hetz.demo.stream-gh-ci"]
    );
}

/// A held (adopt-only) agent stops a live stream, exactly as it stops a live ding.
#[test]
fn held_adopt_only_compact_agent_stops_its_live_derived_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let held = COMPACT_AGENT_WITH_STREAM.replacen(
        "  command \"true\"",
        "  command \"true\"\n  lifecycle \"adopt-only\"",
        1,
    );
    write(tmp.path(), "agents/hetz/demo/agent.kdl", &held);
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.stream-gh-ci")],
        ..Default::default()
    };

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert_eq!(report.held, ["hetz.demo"]);
    assert_eq!(report.torn_down, ["hetz.demo.stream-gh-ci"]);
    assert!(report.launched.is_empty());
}

/// 4c. THE claim the hypothesis rests on: a stream source that keeps dying exhausts the agent's
/// `mode = fail` budget, parks, and is surfaced as a crash-loop record — while its agent and its
/// ding sibling are never touched.
///
/// `up_once` cannot express this (both single-pass entry points build a fresh `FlappingCap`, so a
/// one-shot reconcile can never park anything), so this drives `discover` + `reconcile`/`execute`
/// with ONE cap across passes, exactly as `flapping_cap_parks_a_fail_mode_task_that_keeps_dying`
/// does. The live agent and live ding are the negative controls: a host-wide or agent-wide reaction
/// would name them in the runner's op log, and nothing does.
#[test]
fn a_crash_looping_stream_parks_and_surfaces_without_disturbing_its_agent() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_STREAM,
    );
    let found = discover(tmp.path());
    let runner = FakeRunner {
        sessions: vec![
            live("hetz.demo"),
            live("hetz.demo.ding"),
            dead("hetz.demo.stream-gh-ci"),
        ],
        ..Default::default()
    };
    let mut cap = FlappingCap::default();

    let mut last = UpReport::default();
    for _ in 0..4 {
        let plan = reconcile(&found.specs, &runner.sessions, "hetz");
        last = UpReport::default();
        execute(&plan, &runner, &mut cap, &mut last);
    }

    assert_eq!(last.flapping, ["hetz.demo.stream-gh-ci"]);
    assert!(last.launched.is_empty());
    assert!(
        last.gc.is_empty(),
        "a parked stream keeps its corpse as evidence"
    );
    assert_eq!(
        runner.spawned.borrow().len(),
        1,
        "attempts = 1 bounds the relaunches"
    );

    // The agent and its ding sibling take NO lifecycle op — not spawned, not killed, not reaped.
    // This is the "without affecting the agent" half of the claim, and it is a stronger check than
    // comparing a pid because it also catches a kill followed by an identical relaunch.
    //
    // `patch:` is deliberately excluded: presentation patching is the pass's ordinary cosmetic
    // batch for every live task and carries no lifecycle meaning. Measured, not assumed — the
    // agent DOES appear in the raw op log, as `patch:hetz.demo` and nothing else.
    let ops = runner.ops.borrow();
    let lifecycle_ops = ops
        .iter()
        .filter(|op| !op.starts_with("patch:"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle_ops,
        [
            "reap:hetz.demo.stream-gh-ci",
            "spawn:hetz.demo.stream-gh-ci"
        ],
        "only the stream may be touched while it crash-loops"
    );
    assert!(
        ops.iter().filter(|op| !op.starts_with("patch:")).count() < ops.len(),
        "the agent is still presented; this test's claim is about lifecycle ops"
    );

    // …and it surfaces. The crash-loop record carries the parked TASK, its owning agent, and the
    // supervisor to notify, so `surface_crash_loop` can deliver it over the bus unchanged.
    assert_eq!(last.crash_loops.len(), 1);
    let cl = &last.crash_loops[0];
    assert_eq!(cl.pty_id, "hetz.demo.stream-gh-ci");
    assert_eq!(cl.identity, "demo");
    assert_eq!(cl.supervisor.as_deref(), Some("cos-claude"));
    assert_eq!(cl.agent_bus_id("hetz"), "hetz.demo");

    write(
        tmp.path(),
        "agents/hetz/cos-claude/agent.kdl",
        "agent \"cos-claude\" { host \"hetz\"; command \"true\" }\n",
    );
    surface_crash_loop(tmp.path(), "hetz", cl);
    let inbox = message::inbox_dir(&tmp.path().join("agents/hetz/cos-claude"));
    let msgs = message::list_dir(&inbox).unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].tags.contains(&"crash-loop".to_string()));
    assert!(
        msgs[0].body.contains("hetz.demo.stream-gh-ci"),
        "the supervisor is told WHICH task parked, not just which agent"
    );
}

/// A parked AGENT still stops its live stream — the coupling runs in both directions.
#[test]
fn parked_compact_agent_stops_its_live_derived_stream() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_STREAM,
    );
    let found = discover(tmp.path());
    let runner = FakeRunner {
        sessions: vec![dead("hetz.demo"), live("hetz.demo.stream-gh-ci")],
        ..Default::default()
    };
    let mut cap = FlappingCap::default();

    let first = reconcile(&found.specs, &runner.sessions, "hetz");
    execute(&first, &runner, &mut cap, &mut UpReport::default());
    let second = reconcile(&found.specs, &runner.sessions, "hetz");
    let mut report = UpReport::default();
    execute(&second, &runner, &mut cap, &mut report);

    assert_eq!(report.flapping, ["hetz.demo"]);
    assert!(
        runner
            .killed
            .borrow()
            .contains(&"hetz.demo.stream-gh-ci".to_string())
    );
}

/// A missing stream under targeted reconciliation is HELD, never broadened to its agent — the same
/// guarantee `selected_missing_derived_ding_is_held_without_broadening_to_its_agent` states.
#[test]
fn selected_missing_derived_stream_is_held_without_broadening_to_its_agent() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        COMPACT_AGENT_WITH_STREAM,
    );
    let runner = FakeRunner::default();

    let report = up_once_selected(tmp.path(), "hetz.demo.stream-gh-ci", "hetz", &runner).unwrap();

    assert_eq!(runner.list_calls.get(), 1);
    assert_eq!(report.held, ["hetz.demo.stream-gh-ci"]);
    assert!(report.launched.is_empty());
    assert!(
        runner.spawned.borrow().is_empty(),
        "targeted reconciliation must not broaden to the agent"
    );
    assert!(runner.reaped.borrow().is_empty());
}

/// A launched stream is a companion, not work, and discovery rejects it without a canonical agent
/// task instead of silently treating the derived adapter as a runnable primary.
#[test]
fn a_launched_stream_alone_is_rejected_before_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/sourceless/agent.kdl",
        COMPACT_STREAM_ONLY_AGENT,
    );
    let runner = FakeRunner::default();

    let report = up_once(tmp.path(), "hetz", &runner).unwrap();

    assert!(report.unrunnable.is_empty());
    assert!(
        report.errors.iter().any(
            |error| error.contains("launched stream 'gh-ci' requires a canonical `agent` task")
        ),
        "{:?}",
        report.errors
    );
    assert!(report.launched.is_empty());
    assert!(runner.spawned.borrow().is_empty());
}

/// A stream must not accidentally claim a delivery transport: `has_delivery_transport` is scoped to
/// the derived `ding` companion, and an agent with a stream but no `ding` still has no transport.
#[test]
fn a_stream_does_not_claim_a_delivery_transport() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "agents/hetz/demo/agent.kdl",
        "agent \"demo\" { host \"hetz\"; command \"true\"; stream \"gh-ci\" { command \"poll.sh\" } }\n",
    );
    let found = discover(tmp.path());
    let spec = &found.specs[0];

    assert!(!spec.has_delivery_transport());
    assert!(spec.is_runnable());
    let names = spec
        .tasks
        .iter()
        .map(|task| task.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["agent", "stream-gh-ci"]);
}

/// The "unsupported derived task" gate stays fail-closed: extending it for streams must not have
/// turned it into a permissive fall-through.
#[test]
fn an_unknown_derived_task_still_refuses_the_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = task_spec("demo", Some("hetz"), "hetz.demo.mystery");
    spec.tasks[0].derived = true;
    spec.tasks[0].name = "mystery".to_string();
    let context = TaskCompileContext::current(tmp.path().to_path_buf()).unwrap();
    let mut specs = vec![spec];

    let error = compile_generated_tasks(&mut specs, "hetz", &context).unwrap_err();

    assert!(
        format!("{error:#}").contains("unsupported derived task: mystery"),
        "got: {error:#}"
    );
}
