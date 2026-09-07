use super::*;
use std::os::fd::AsRawFd as _;
use std::process::ChildStdin;
use agent_spec::spec::{
    AgentSpec, Driver, JobType, OmpDriver, Task, TaskKind, TaskLifecycle,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc;


/// The bound is derived from the resolved pty root, never a fixed maximum identity length.
///
/// `pty` binds `<PTY_ROOT>/<session-id>.sock`, so the separator plus the five-byte suffix is
/// the fixed overhead and the usable identity length is whatever remains of the limit. These
/// numbers are measured against the pty binary itself: with a 21-byte root it accepts a
/// 77-byte id and refuses a 78-byte one as "a socket path of 105 bytes, which exceeds the
/// 104-byte kernel limit by 1".
#[test]
fn session_socket_overage_is_derived_from_the_resolved_root() {
    let short_root = Path::new("/tmp/ptyprobe-1960953");
    let fits = "a".repeat(77);
    let over = "a".repeat(78);

    assert_eq!(
        session_socket_path(short_root, &fits)
            .as_os_str()
            .as_encoded_bytes()
            .len(),
        PORTABLE_SOCKET_PATH_LIMIT,
        "the accepted id must land exactly on the limit"
    );
    assert!(session_socket_overage(short_root, &fits).is_none());

    let (path, overage) =
        session_socket_overage(short_root, &over).expect("one byte over is refused");
    assert_eq!(overage, 1);
    assert_eq!(path, short_root.join(format!("{over}.sock")));

    // A deeper root shrinks every identity's budget on that host: the same id that fitted
    // above is now 26 bytes over.
    let deep_root = Path::new("/home/user/.local/state/st2/default/catalog/pty");
    assert_eq!(
        session_socket_overage(deep_root, &fits).map(|(_, over)| over),
        Some(26)
    );
}

#[cfg(target_os = "linux")]
fn linux_process_state(pid: i32) -> Option<char> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()?
        .rsplit_once(") ")?
        .1
        .chars()
        .next()
}

/// Block until the fixture publishes `marker`, which its script creates by an atomic rename so
/// the barrier never observes a half-written file. Called from `on_spawn`, which runs before
/// [`run_captured`] starts the child deadline: fork+exec scheduling is therefore paid here and
/// not out of the deadline the test then measures. The ceiling is deliberately far larger than
/// any plausible fork+exec — it bounds a fixture that never ran at all, and is not itself the
/// behaviour under test, so a loaded host cannot turn it into a failure.
fn await_fixture_ready(pid: i32, marker: &Path, what: &str) {
    const CEILING: Duration = Duration::from_secs(30);
    let deadline = Instant::now() + CEILING;
    while !marker.exists() {
        if Instant::now() >= deadline {
            // Do not leak the fixture's long sleeper into the test host on the way out.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            panic!(
                "{what} within {CEILING:?}: {} never appeared",
                marker.display()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn fixture_barrier_path(executable: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{suffix}", executable.display()))
}

fn reset_fixture_barrier(executable: &Path) {
    for suffix in ["ready", "release"] {
        let marker = fixture_barrier_path(executable, suffix);
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove stale fixture marker {}: {error}", marker.display()),
        }
    }
}

fn release_ready_fixture(pid: i32, executable: &Path, what: &str) {
    let ready = fixture_barrier_path(executable, "ready");
    await_fixture_ready(pid, &ready, what);
    std::fs::write(fixture_barrier_path(executable, "release"), b"go\n").unwrap();
}

fn process_can_retain_cleanup_resources(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if linux_process_state(pid) == Some('Z') {
        return false;
    }
    crate::host_lock::process_alive(pid)
}

fn target(id: &str, cmd: &str) -> TaskTarget {
    TaskTarget {
        kind: TaskKind::Pty,
        pty_id: id.to_string(),
        bus_id: "hetz.demo".to_string(),
        name: "agent".to_string(),
        derived: false,
        launch: TaskLaunch::Shell(cmd.to_string()),
        cwd: None,
        workspace: None,
        tags: BTreeMap::new(),
        env: BTreeMap::new(),
        keep: false,
        presentation: None,
    }
}

struct GateRunner {
    list_calls: Cell<usize>,
}

impl Runner for GateRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.list_calls.set(self.list_calls.get() + 1);
        Ok(Vec::new())
    }

    fn spawn(&self, _target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
        panic!("gate runner must not spawn")
    }

    fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
        panic!("gate runner must not kill")
    }

    fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
        panic!("gate runner must not remove")
    }
}

#[derive(Default)]
struct PersistentPatchRunner {
    patched: RefCell<Vec<String>>,
}

impl Runner for PersistentPatchRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        unreachable!("presentation execution does not list sessions")
    }

    fn spawn(&self, _target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
        unreachable!("presentation-only plan must not spawn")
    }

    fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
        unreachable!("presentation-only plan must not kill")
    }

    fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
        unreachable!("presentation-only plan must not remove")
    }

    fn patch_presentation(&self, presentation: &PtyPresentation) -> anyhow::Result<()> {
        self.patched.borrow_mut().push(presentation.pty_id.clone());
        if presentation.pty_id.as_str() < "host.presented.08" {
            anyhow::bail!("simulated persistent metadata failure");
        }
        Ok(())
    }
}

#[test]
fn bounded_presentation_batches_are_deterministic_and_do_not_starve() {
    let plan = ReconcilePlan {
        presentation: (0..12)
            .rev()
            .map(|index| PtyPresentation {
                pty_id: format!("host.presented.{index:02}"),
                display_name: None,
                tags: BTreeMap::new(),
            })
            .collect(),
        ..ReconcilePlan::default()
    };
    let runner = PersistentPatchRunner::default();
    let mut cap = FlappingCap::default();
    let mut cursor = PresentationPatchCursor::default();

    for _ in 0..2 {
        execute_with_presentation_cursor(
            &plan,
            &runner,
            &mut cap,
            &mut cursor,
            &mut UpReport::default(),
            &mut |_| {},
        );
    }

    let attempted = runner.patched.borrow();
    assert_eq!(
        &attempted[..8],
        &(0..8)
            .map(|index| format!("host.presented.{index:02}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(attempted.len(), 16);
    for index in 8..12 {
        assert!(attempted.contains(&format!("host.presented.{index:02}")));
    }
}

#[test]
fn lifecycle_hook_consumer_is_a_closed_enum() {
    let consumers = [
        lifecycle_hook_consumer(true, false, false),
        lifecycle_hook_consumer(false, true, false),
        lifecycle_hook_consumer(false, false, true),
        lifecycle_hook_consumer(true, true, false),
        lifecycle_hook_consumer(true, false, true),
        lifecycle_hook_consumer(false, true, true),
        lifecycle_hook_consumer(true, true, true),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(
        consumers,
        BTreeSet::from([
            "codex",
            "pi",
            "omp",
            "codex+pi",
            "codex+omp",
            "pi+omp",
            "codex+pi+omp",
        ])
    );
}

#[test]
fn selected_codex_gate_suppresses_launch_on_stale_hooks() {
    let spec = AgentSpec {
        id: None,
        address: None,
        identity: "codex".into(),
        name: None,
        description: None,
        host: None,
        role: None,
        job_type: JobType::Service,
        workspace: None,
        supervisor: None,
        desired_state: crate::AgentDesiredState::Running,
        keep: false,
        restart: None,
        delivery: None,
        session_driver: None,
        driver: None,
        delivery_readiness: None,
        resources: vec![],
        streams: Vec::new(),
        tasks: vec![Task {
            kind: TaskKind::Pty,
            derived: false,
            name: "agent".into(),
            id: Some("test.codex.agent".into()),
            command: None,
            argv: Some(vec!["$CATALOG/bin/codex".into(), "--version".into()]),
            cwd: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
            lifecycle: TaskLifecycle::Service,
        }],
        path: "/tmp/spec.kdl".into(),
    };
    let runner = GateRunner {
        list_calls: Cell::new(0),
    };
    let report = up_once_selected_specs_with_gates(
        Path::new("/tmp"),
        &[spec],
        "test.codex.agent",
        "test",
        &runner,
        |consumer| {
            assert_eq!(consumer, None);
            anyhow::bail!("stale receipt")
        },
    )
    .unwrap();
    assert_eq!(runner.list_calls.get(), 1);
    assert!(report.launched.is_empty());
    assert!(report.errors.iter().any(|error| {
        error.contains("stale receipt") && error.contains("launch suppressed")
    }));
}

#[test]
fn selected_identity_conflict_refuses_before_hook_verification_or_inventory() {
    let mut spec = AgentSpec {
        id: None,
        address: None,
        identity: "codex".into(),
        name: None,
        description: None,
        host: None,
        role: None,
        job_type: JobType::Service,
        workspace: None,
        supervisor: None,
        desired_state: crate::AgentDesiredState::Running,
        keep: false,
        restart: None,
        delivery: None,
        session_driver: None,
        driver: None,
        delivery_readiness: None,
        resources: vec![],
        streams: Vec::new(),
        tasks: vec![Task {
            kind: TaskKind::Pty,
            derived: false,
            name: "agent".into(),
            id: Some("test.codex.agent".into()),
            command: None,
            argv: Some(vec!["$CATALOG/bin/codex".into(), "--version".into()]),
            cwd: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
            lifecycle: TaskLifecycle::Service,
        }],
        path: "/tmp/spec.kdl".into(),
    };
    spec.tasks[0]
        .env
        .insert("ST_AGENT".into(), "wrong.actor".into());
    let runner = GateRunner {
        list_calls: Cell::new(0),
    };
    let verify_calls = Cell::new(0);

    let error = up_once_selected_specs_with_gates(
        Path::new("/tmp"),
        &[spec],
        "test.codex.agent",
        "test",
        &runner,
        |_| {
            verify_calls.set(verify_calls.get() + 1);
            Ok(())
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("conflicting ST_AGENT"));
    assert_eq!(verify_calls.get(), 0);
    assert_eq!(runner.list_calls.get(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn idle_supervisor_does_not_spin_on_its_own_catalog_reads() {
    let catalog = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let mut passes = 0usize;

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(350));
            stop.store(true, Ordering::SeqCst);
        });
        up_loop_until(
            catalog.path(),
            "test-host",
            &GateRunner {
                list_calls: Cell::new(0),
            },
            Duration::from_secs(60),
            &stop,
            best_effort_catalog_watcher,
            |_| passes += 1,
        )
        .unwrap();
    });

    assert!(
        passes <= 2,
        "idle supervisor must wait instead of reconciling its own read events: {passes} passes"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn failed_watch_installation_keeps_supervisor_on_timer_cadence() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = catalog.path().join("agents/test-host/live");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("agent.kdl"),
        r#"agent "live" { host "test-host"; command "x" }"#,
    )
    .unwrap();
    let stop = AtomicBool::new(false);
    let mut passes = 0usize;
    let (started_tx, started_rx) = mpsc::sync_channel(1);

    std::thread::scope(|scope| {
        let stop = &stop;
        scope.spawn(move || {
            started_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(350));
            stop.store(true, Ordering::SeqCst);
        });
        up_loop_until(
            catalog.path(),
            "test-host",
            &SpawnCountingRunner::default(),
            Duration::from_millis(100),
            &stop,
            |_, _| None, // watcher installation fails, as it did on dev3's oversized catalog
            |_| {
                passes += 1;
                let _ = started_tx.try_send(());
            },
        )
        .unwrap();
    });

    assert!(
        (2..=6).contains(&passes),
        "a disconnected watcher channel must fall back to timer cadence, not spin: \
             {passes} passes in ~350ms at a 100ms interval"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn supervisor_wakes_and_launches_a_new_direct_declaration() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = catalog.path().join("agents/test-host/live");
    let stop = AtomicBool::new(false);
    let runner = SpawnCountingRunner::default();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let mut passes = 0usize;

    std::thread::scope(|scope| {
        let watchdog_stop = &stop;
        scope.spawn(move || {
            if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                watchdog_stop.store(true, Ordering::SeqCst);
            }
        });
        up_loop_until(
            catalog.path(),
            "test-host",
            &runner,
            Duration::from_secs(60),
            &stop,
            best_effort_catalog_watcher,
            |report| {
                passes += 1;
                match passes {
                    1 => {
                        std::fs::create_dir_all(&agent).unwrap();
                        std::fs::write(
                            agent.join("agent.kdl"),
                            r#"agent "live" { host "test-host"; command "x" }"#,
                        )
                        .unwrap();
                    }
                    2 => {
                        assert_eq!(report.launched, ["test-host.live"]);
                        let _ = done_tx.send(());
                        stop.store(true, Ordering::SeqCst);
                    }
                    _ => panic!("one direct declaration must cause exactly one prompt pass"),
                }
            },
        )
        .unwrap();
    });

    assert_eq!(passes, 2, "the 60s timer must not be the publication path");
    assert_eq!(runner.spawned.borrow().as_slice(), ["test-host.live"]);
}

#[test]
fn resident_loop_reloads_added_changed_removed_and_malformed_profiles() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = catalog.path().join("agents/test-host/live");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("agent.kdl"),
        r#"agent "live" {
  host "test-host"
  command "true"
  resource "alpha" uri="alpha://test-host/live" reason="Alpha."
  resource "beta" uri="beta://test-host/live" reason="Beta."
}"#,
    )
    .unwrap();
    let missing = catalog.path().join("missing.wasm");
    let profile = |scheme: &str| {
        format!(
            "profile {scheme:?} {{ wasm {:?} }}\n",
            missing.display().to_string()
        )
    };
    let config = crate::catalog::config_path(catalog.path());
    let runner = SpawnCountingRunner::default();
    runner
        .sessions
        .borrow_mut()
        .push(sess("test-host.live.agent", true));
    let stop = AtomicBool::new(false);
    let mut reports = Vec::new();

    up_loop_until(
        catalog.path(),
        "test-host",
        &runner,
        Duration::from_millis(5),
        &stop,
        |_, _| None,
        |report| {
            let pass = reports.len();
            reports.push((report.warnings.clone(), report.errors.clone()));
            match pass {
                0 => std::fs::write(&config, profile("alpha")).unwrap(),
                1 => std::fs::write(&config, profile("beta")).unwrap(),
                2 => std::fs::write(&config, "").unwrap(),
                3 => stop.store(true, Ordering::SeqCst),
                _ => unreachable!("profile removal run stops after four passes"),
            }
        },
    )
    .unwrap();

    let profile_warnings = |reports: &Vec<(Vec<String>, Vec<String>)>, pass: usize| {
        reports[pass]
            .0
            .iter()
            .filter(|warning| warning.contains("resync profile"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(
        profile_warnings(&reports, 0).is_empty(),
        "no profile is initially declared"
    );
    assert!(
        profile_warnings(&reports, 1)
            .iter()
            .any(|warning| warning.contains("resource 'alpha'")),
        "an added profile takes effect: {:?}",
        reports[1]
    );
    assert!(
        profile_warnings(&reports, 2)
            .iter()
            .any(|warning| warning.contains("resource 'beta'"))
            && !profile_warnings(&reports, 2)
                .iter()
                .any(|warning| warning.contains("resource 'alpha'")),
        "changing definitions replaces the registry: {:?}",
        reports[2]
    );
    assert!(
        profile_warnings(&reports, 3).is_empty(),
        "removing every profile removes the old resolution semantics: {:?}",
        reports[3]
    );

    // A separate resident lifetime starts valid, then makes the envelope malformed. The
    // initial hard parse still accepts the valid declaration; the later edit must clear its
    // active semantics rather than silently carrying them forward.
    std::fs::write(&config, profile("alpha")).unwrap();
    stop.store(false, Ordering::SeqCst);
    let mut malformed_reports = Vec::new();
    up_loop_until(
        catalog.path(),
        "test-host",
        &runner,
        Duration::from_millis(5),
        &stop,
        |_, _| None,
        |report| {
            let pass = malformed_reports.len();
            malformed_reports.push((report.warnings.clone(), report.errors.clone()));
            match pass {
                0 => std::fs::write(
                    &config,
                    r#"profiel "alpha" { wasm "missing.wasm" }"#,
                )
                .unwrap(),
                1 => stop.store(true, Ordering::SeqCst),
                _ => unreachable!("malformed profile run stops after two passes"),
            }
        },
    )
    .unwrap();
    assert!(
        profile_warnings(&malformed_reports, 0)
            .iter()
            .any(|warning| warning.contains("resource 'alpha'")),
        "the profile is active before the malformed edit: {:?}",
        malformed_reports[0]
    );
    assert!(
        malformed_reports[1]
            .1
            .iter()
            .any(|error| error.contains("unknown catalog.kdl top-level node 'profiel'"))
            && profile_warnings(&malformed_reports, 1).is_empty(),
        "malformed catalog state is reported and fails closed instead of retaining alpha: {:?}",
        malformed_reports[1]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn disconnected_watcher_channel_waits_out_the_interval_instead_of_spinning() {
    let (_tx, rx) = channel::<()>();
    let started = Instant::now();
    assert_eq!(
        wait_for_reconcile(&rx, Duration::from_millis(80), &AtomicBool::new(false)),
        ReconcileWake::Interval,
        "disconnection must be treated as silence, not as a change"
    );
    assert!(started.elapsed() >= Duration::from_millis(75));
}

// ── liveness debounce (R21c): a transient `pty list` not-alive flicker under load must not
//    destructively GC/relaunch a HEALTHY agent; a stable death must still be reaped ──────────────

use crate::reconcile::Launch;
fn sess(id: &str, alive: bool) -> Session {
    Session {
        pty_id: id.to_string(),
        alive,
        exit_code: None,
        presentation: None,
    }
}

/// Records spawns and reports every launch as succeeding, so a pass can be driven repeatedly.
#[derive(Default)]
struct SpawnCountingRunner {
    sessions: RefCell<Vec<Session>>,
    spawned: RefCell<Vec<String>>,
}

impl Runner for SpawnCountingRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(self.sessions.borrow().clone())
    }

    fn spawn(&self, target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
        self.spawned.borrow_mut().push(target.pty_id.clone());
        Ok(())
    }

    fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn patch_presentation(&self, _presentation: &PtyPresentation) -> anyhow::Result<()> {
        Ok(())
    }
}

/// `execute` must close every pass, or a task that recovers is never forgiven and eventually
/// parks even though it is healthy — the opposite of the crash-loop bug the cap exists for.
///
/// The unit tests in `flapping.rs` call `end_pass` by hand, so they cannot catch it never being
/// called from a reconcile pass. This one drives the real `execute` path. `interval = 0s` makes
/// any survived pass count as recovery, keeping the test free of wall-clock sleeping.
#[test]
fn execute_closes_each_pass_so_a_recovered_task_regains_its_fail_budget() {
    let mut spec = spec_fixture();
    spec.restart = Some(agent_spec::spec::Restart {
        attempts: 3,
        interval: Duration::from_secs(0),
        delay: Duration::from_secs(0),
        mode: agent_spec::spec::RestartMode::Fail,
    });
    let runner = SpawnCountingRunner::default();
    let mut cap = FlappingCap::default();

    fn dying(spec: &AgentSpec) -> ReconcilePlan<'_> {
        ReconcilePlan {
            launch: vec![Launch {
                spec,
                tasks: vec![target("hetz.demo.agent", "x")],
                live_derived: Vec::new(),
            }],
            ..ReconcilePlan::default()
        }
    }

    // Two failing passes: two of three launches spent.
    for _ in 0..2 {
        execute(&dying(&spec), &runner, &mut cap, &mut UpReport::default());
    }
    assert_eq!(runner.spawned.borrow().len(), 2, "two launches spent");

    // A pass that launches nothing because it found the task alive. That observation — not the
    // empty launch set — is what forgives the budget.
    execute(
        &ReconcilePlan {
            live: vec!["hetz.demo.agent".to_string()],
            ..ReconcilePlan::default()
        },
        &runner,
        &mut cap,
        &mut UpReport::default(),
    );

    // Having recovered, it gets the full budget back: three more launches, then parked. Without
    // the pass being closed it would park after only one more.
    let mut last = UpReport::default();
    for _ in 0..4 {
        last = UpReport::default();
        execute(&dying(&spec), &runner, &mut cap, &mut last);
    }
    assert_eq!(
        runner.spawned.borrow().len(),
        5,
        "recovery must restore the full `attempts` budget, not leave it partly spent"
    );
    assert_eq!(
        last.flapping,
        vec!["hetz.demo.agent".to_string()],
        "and it still parks in the end"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn persistent_advisory_warnings_surface_once_not_per_pass() {
    let catalog = tempfile::tempdir().unwrap();
    let agent = catalog.path().join("agents/test-host/live");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(catalog.path().join("workspace")).unwrap();
    std::fs::write(
        agent.join("agent.kdl"),
        r#"agent "live" {
  host "test-host"
  command "true"
  workspace "$CATALOG/workspace"
  render { git-exclude "scratch.txt" }
}"#,
    )
    .unwrap();
    let stop = AtomicBool::new(false);
    let mut passes = 0usize;
    let mut warnings_seen = 0usize;
    let (started_tx, started_rx) = mpsc::sync_channel(1);

    std::thread::scope(|scope| {
        let stop = &stop;
        scope.spawn(move || {
            started_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            stop.store(true, Ordering::SeqCst);
        });
        up_loop_until(
            catalog.path(),
            "test-host",
            &SpawnCountingRunner::default(),
            Duration::from_millis(50),
            &stop,
            |_, _| None,
            |report| {
                passes += 1;
                warnings_seen += report.warnings.len();
                let _ = started_tx.try_send(());
            },
        )
        .unwrap();
    });

    assert!(
        passes >= 3,
        "the loop must have run several passes for this to say anything: {passes}"
    );
    assert_eq!(
        warnings_seen, 1,
        "an unchanged advisory failure must be diagnosed once across {passes} passes"
    );
}
/// A pass can execute a plan the task was never in: `up_once` drops an owner whose
/// materialization failed, `gate_harness_launches_on_hooks` strips gated launches, and
/// `defer_flickers` removes debounced ones — each after the pass is already committed to
/// running. Silence about a task is not evidence it is alive, and crediting uptime for it lets
/// a permanently-dead task refill its budget on every gated pass and never park. Identical to
/// the recovery test above except that the quiet pass does not report the task live.
#[test]
fn a_pass_that_omits_a_task_does_not_credit_it_with_uptime() {
    let mut spec = spec_fixture();
    spec.restart = Some(agent_spec::spec::Restart {
        attempts: 3,
        interval: Duration::from_secs(0),
        delay: Duration::from_secs(0),
        mode: agent_spec::spec::RestartMode::Fail,
    });
    let runner = SpawnCountingRunner::default();
    let mut cap = FlappingCap::default();

    fn dying(spec: &AgentSpec) -> ReconcilePlan<'_> {
        ReconcilePlan {
            launch: vec![Launch {
                spec,
                tasks: vec![target("hetz.demo.agent", "x")],
                live_derived: Vec::new(),
            }],
            ..ReconcilePlan::default()
        }
    }

    // Two failing passes: two of three launches spent.
    for _ in 0..2 {
        execute(&dying(&spec), &runner, &mut cap, &mut UpReport::default());
    }
    assert_eq!(runner.spawned.borrow().len(), 2, "two launches spent");

    // The task is dropped from this pass — not launched, and not observed alive either.
    execute(
        &ReconcilePlan::default(),
        &runner,
        &mut cap,
        &mut UpReport::default(),
    );

    // The budget must be where the failures left it: one launch remains, then it parks.
    let mut last = UpReport::default();
    for _ in 0..4 {
        last = UpReport::default();
        execute(&dying(&spec), &runner, &mut cap, &mut last);
    }
    assert_eq!(
        runner.spawned.borrow().len(),
        3,
        "an unobserved pass must not forgive the failure budget"
    );
    assert_eq!(
        last.flapping,
        vec!["hetz.demo.agent".to_string()],
        "and the task must still park"
    );
}

fn spec_fixture() -> AgentSpec {
    AgentSpec {
        id: None,
        address: None,
        identity: "demo".into(),
        name: None,
        description: None,
        host: Some("hetz".into()),
        role: None,
        job_type: JobType::Service,
        workspace: None,
        supervisor: None,
        desired_state: crate::AgentDesiredState::Running,
        keep: false,
        restart: None,
        delivery: None,
        session_driver: None,
        driver: None,
        delivery_readiness: None,
        resources: vec![],
        streams: Vec::new(),
        tasks: vec![],
        path: std::path::PathBuf::from("/x"),
    }
}

#[test]
fn driver_labels_include_typed_and_argv_omp_but_remain_bounded() {
    let legacy_spec = spec_fixture();
    let legacy_launch = Launch {
        spec: &legacy_spec,
        tasks: Vec::new(),
        live_derived: Vec::new(),
    };
    let mut omp_argv = target("hetz.demo.agent", "unused");
    omp_argv.launch = TaskLaunch::Argv(vec![
        "st2".into(),
        "driver".into(),
        "omp-session".into(),
    ]);
    let mut exec = target("hetz.demo.agent", "codex");
    exec.kind = TaskKind::Exec;
    let targets = [
        target("hetz.demo.agent", "codex"),
        target("hetz.demo.agent", "claude"),
        target("hetz.demo.agent", "opencode"),
        target("hetz.demo.agent", "pi"),
        omp_argv,
        exec,
        target("hetz.demo.agent", "unrecognized"),
    ];
    let labels = targets
        .iter()
        .map(|target| driver_label(&legacy_launch, target))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        labels,
        BTreeSet::from(["codex", "claude", "opencode", "pi", "omp", "exec", "other"])
    );

    let mut typed_spec = spec_fixture();
    typed_spec.driver = Some(Driver::Omp(OmpDriver {
        model: None,
        effort: None,
        prompt: String::new(),
        args: Vec::new(),
    }));
    let typed_launch = Launch {
        spec: &typed_spec,
        tasks: Vec::new(),
        live_derived: Vec::new(),
    };
    assert_eq!(
        driver_label(&typed_launch, &target("hetz.demo.agent", "claude")),
        "omp",
        "typed driver identity must take precedence over argv heuristics"
    );
}

#[test]
fn resync_watch_eligibility_requires_a_proven_live_agent_seat() {
    let spec = |identity: &str, explicit_id: Option<&str>| {
        let mut spec = spec_fixture();
        spec.identity = identity.to_owned();
        spec.tasks = vec![Task {
            kind: TaskKind::Pty,
            derived: false,
            name: "agent".into(),
            id: explicit_id.map(str::to_owned),
            command: Some("agent".into()),
            argv: None,
            cwd: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
            lifecycle: TaskLifecycle::Service,
        }];
        spec
    };
    let specs = vec![
        spec("desired", None),
        spec("dead-adopted", None),
        spec("observed-live", None),
        spec("launched", None),
        spec("restarted", Some("custom-seat")),
    ];
    let sessions = vec![
        sess("hetz.dead-adopted.agent", false),
        // A live canonical seat remains eligible even when a missing companion means the
        // whole spec was not adopted and the companion later fails to launch.
        sess("hetz.observed-live.agent", true),
    ];
    let report = UpReport {
        adopted: vec!["dead-adopted".into()],
        launched: vec![
            "hetz.launched.agent".into(),
            // A successfully launched companion is not evidence of a live agent seat.
            "hetz.desired.ding".into(),
        ],
        restarted: vec!["custom-seat".into()],
        ..UpReport::default()
    };

    let eligible = live_resync_specs(&specs, "hetz", &sessions, &report)
        .into_iter()
        .map(|spec| spec.identity)
        .collect::<Vec<_>>();
    assert_eq!(eligible, vec!["observed-live", "launched", "restarted"]);
}

#[test]
fn subscription_eligibility_excludes_non_running_agents_even_with_a_live_seat() {
    // A retired or suspended agent whose canonical seat is still alive mid-teardown owns no
    // live subscription work: its resync installs and resource-Profile bindings must be
    // stripped this pass, not left running until the seat dies (dotfiles#1535). The declaration
    // (including its `resource` bindings) is untouched — only the runtime work stops.
    let seat = || Task {
        kind: TaskKind::Pty,
        derived: false,
        name: "agent".into(),
        id: None,
        command: Some("agent".into()),
        argv: None,
        cwd: None,
        tags: BTreeMap::new(),
        env: BTreeMap::new(),
        keep: false,
        lifecycle: TaskLifecycle::Service,
    };
    let with_state = |identity: &str, state: crate::AgentDesiredState| {
        let mut spec = spec_fixture();
        spec.identity = identity.to_owned();
        spec.tasks = vec![seat()];
        spec.desired_state = state;
        spec
    };
    let specs = vec![
        with_state("running", crate::AgentDesiredState::Running),
        with_state(
            "retired",
            crate::AgentDesiredState::Retired {
                reason: Some("Mission complete".into()),
            },
        ),
        with_state(
            "suspended",
            crate::AgentDesiredState::Suspended {
                reason: "Waiting for capacity".into(),
            },
        ),
    ];
    // Every seat is observed alive, so only desired state can distinguish them.
    let sessions = vec![
        sess("hetz.running.agent", true),
        sess("hetz.retired.agent", true),
        sess("hetz.suspended.agent", true),
    ];
    let eligible = live_resync_specs(&specs, "hetz", &sessions, &UpReport::default())
        .into_iter()
        .map(|spec| spec.identity)
        .collect::<Vec<_>>();
    assert_eq!(eligible, vec!["running"]);
}

struct BlockingLaunchRunner {
    sessions: RefCell<Vec<Session>>,
    fail_id: Option<String>,
    block_id: String,
    entered: mpsc::SyncSender<()>,
    release: RefCell<mpsc::Receiver<()>>,
}

impl Runner for BlockingLaunchRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(self.sessions.borrow().clone())
    }

    fn spawn(&self, target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
        if self.fail_id.as_deref() == Some(&target.pty_id) {
            anyhow::bail!("simulated launch failure");
        }
        if target.pty_id == self.block_id {
            self.entered.send(()).unwrap();
            self.release.borrow_mut().recv().unwrap();
        }
        Ok(())
    }

    fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, feature = "wasm-resolver"))]
struct SteadyChainRunner {
    sessions: std::sync::Mutex<Vec<Session>>,
    block_id: String,
    entered: mpsc::SyncSender<()>,
    release: std::sync::Mutex<mpsc::Receiver<()>>,
}

#[cfg(all(test, feature = "wasm-resolver"))]
impl Runner for SteadyChainRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn spawn(&self, target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
        if target.pty_id == self.block_id {
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv()
                .unwrap();
        }
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sess(&target.pty_id, true));
        Ok(())
    }

    fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

fn write_resync_agent(catalog: &Path, identity: &str) -> (PathBuf, PathBuf) {
    let agent_dir = catalog.join("agents/hetz").join(identity);
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        format!(
            r#"agent "{identity}" {{
  host "hetz"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}}"#
        ),
    )
    .unwrap();
    let goal = resources.join("goal.md");
    std::fs::write(&goal, "before\n").unwrap();
    (agent_dir, goal)
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn write_notify_chain_profile(catalog: &Path) {
    let resolver_dir = catalog.join("resolvers");
    std::fs::create_dir_all(&resolver_dir).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates/agent-spec/tests/fixtures/demo_resolver.wasm"),
        resolver_dir.join("goal.wasm"),
    )
    .unwrap();
    std::fs::write(
        crate::catalog::config_path(catalog),
        r#"profile "dev.schickling.agent-goal" {
  wasm "resolvers/goal.wasm"
  class "immediate"
  notify-chain #true
}
"#,
    )
    .unwrap();
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn write_notify_chain_agent(
    catalog: &Path,
    identity: &str,
    supervisor: Option<&str>,
    later_task: bool,
) -> (PathBuf, PathBuf) {
    write_notify_chain_agent_with_state(catalog, identity, supervisor, later_task, None)
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn write_notify_chain_agent_with_state(
    catalog: &Path,
    identity: &str,
    supervisor: Option<&str>,
    later_task: bool,
    desired_state: Option<&str>,
) -> (PathBuf, PathBuf) {
    let agent_dir = catalog.join("agents/hetz").join(identity);
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    let supervisor = supervisor
        .map(|supervisor| format!("  supervisor {supervisor:?}\n"))
        .unwrap_or_default();
    let desired_state = desired_state
        .map(|desired_state| format!("  {desired_state}\n"))
        .unwrap_or_default();
    let later_task = if later_task {
        "  exec \"later\" { command \"true\" }\n"
    } else {
        ""
    };
    std::fs::write(
        agent_dir.join("agent.kdl"),
        format!(
            r#"agent "{identity}" {{
  host "hetz"
{supervisor}{desired_state}  command "agent"
{later_task}  resource "goal" uri="dev.schickling.agent-goal://hetz/{identity}" reason="Layer."
}}
"#
        ),
    )
    .unwrap();
    let goal = resources.join("goal.md");
    std::fs::write(&goal, "before\n").unwrap();
    (agent_dir, goal)
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn current_resync_event_for_key(agent_dir: &Path, key: &str) -> Option<String> {
    let expected = format!("key: {key}");
    std::fs::read_dir(agent_dir.join("resources/inbox"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .find(|body| {
            body.lines().any(|line| line == "stream: resync")
                && body.lines().any(|line| line == expected)
        })
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn wait_for_resync_event_for_key(agent_dir: &Path, key: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(body) = current_resync_event_for_key(agent_dir, key) {
            return Some(body);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn wait_for_resync_event_key_change(
    agent_dir: &Path,
    key: &str,
    prior: &str,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(body) = current_resync_event_for_key(agent_dir, key)
            && body != prior
        {
            return Some(body);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn current_resync_event(agent_dir: &Path) -> Option<String> {
    std::fs::read_dir(agent_dir.join("resources/inbox"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .find(|body| body.lines().any(|line| line == "stream: resync"))
}

fn wait_for_resync_event(agent_dir: &Path) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(body) = current_resync_event(agent_dir) {
            return Some(body);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_resync_event_change(agent_dir: &Path, prior: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(body) = current_resync_event(agent_dir)
            && body != prior
        {
            return Some(body);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(feature = "wasm-resolver")]
#[test]
fn up_loop_keeps_complete_notify_chain_sets_during_steady_reconcile() {
    let catalog = tempfile::tempdir().unwrap();
    write_notify_chain_profile(catalog.path());
    let (root_dir, root_goal) =
        write_notify_chain_agent(catalog.path(), "root", None, false);
    let (lead_dir, lead_goal) =
        write_notify_chain_agent(catalog.path(), "lead", Some("hetz.root"), false);
    let (worker_dir, _worker_goal) =
        write_notify_chain_agent(catalog.path(), "worker", Some("hetz.lead"), false);
    let specs = crate::discover_strict(catalog.path()).specs;
    let task_id = |spec: &AgentSpec, task: &Task| {
        task.id
            .clone()
            .unwrap_or_else(|| format!("{}.{}", spec.bus_id("hetz"), task.name))
    };
    let mut sessions = Vec::new();
    for spec in &specs {
        for task in &spec.tasks {
            sessions.push(sess(&task_id(spec, task), true));
        }
    }
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let runner = SteadyChainRunner {
        sessions: std::sync::Mutex::new(sessions),
        block_id: "hetz.worker.later".to_owned(),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    };
    let stop = AtomicBool::new(false);
    let (first_report_tx, first_report_rx) = mpsc::sync_channel(1);
    let observer_catalog = catalog.path().to_path_buf();

    let evidence = std::thread::scope(|scope| {
        let observer_stop = &stop;
        let observer = scope.spawn(move || {
            first_report_rx.recv().unwrap();

            std::fs::write(&root_goal, "steady baseline transition\n").unwrap();
            let root_initial = wait_for_resync_event_for_key(&root_dir, "goal");
            let lead_initial =
                wait_for_resync_event_for_key(&lead_dir, "goal@hetz.root");
            let worker_initial =
                wait_for_resync_event_for_key(&worker_dir, "goal@hetz.root");

            write_notify_chain_agent(
                &observer_catalog,
                "worker",
                Some("hetz.lead"),
                true,
            );
            let entered = entered_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            let root_after_reconcile = root_initial.as_deref().and_then(|prior| {
                std::fs::write(&root_goal, "transition during steady reconcile\n").unwrap();
                wait_for_resync_event_key_change(&root_dir, "goal", prior)
            });
            let lead_after_reconcile = lead_initial.as_deref().and_then(|prior| {
                wait_for_resync_event_key_change(&lead_dir, "goal@hetz.root", prior)
            });
            let worker_after_reconcile = worker_initial.as_deref().and_then(|prior| {
                wait_for_resync_event_key_change(&worker_dir, "goal@hetz.root", prior)
            });

            std::fs::write(&lead_goal, "lead transition during steady reconcile\n").unwrap();
            let lead_own = wait_for_resync_event_for_key(&lead_dir, "goal");
            let worker_from_lead =
                wait_for_resync_event_for_key(&worker_dir, "goal@hetz.lead");

            let _ = release_tx.send(());
            observer_stop.store(true, Ordering::SeqCst);
            (
                entered,
                root_initial,
                lead_initial,
                worker_initial,
                root_after_reconcile,
                lead_after_reconcile,
                worker_after_reconcile,
                lead_own,
                worker_from_lead,
            )
        });
        up_loop_until(
            catalog.path(),
            "hetz",
            &runner,
            Duration::from_millis(25),
            &stop,
            |_, _| None,
            |_| {
                let _ = first_report_tx.try_send(());
            },
        )
        .unwrap();
        observer.join().unwrap()
    });

    assert!(evidence.0, "the steady-state reconcile must reach its later task");
    assert!(evidence.1.is_some(), "root must receive its own transition");
    assert!(
        evidence.2.is_some() && evidence.3.is_some(),
        "root transition must fan out through lead and worker"
    );
    assert!(
        evidence.4.is_some() && evidence.5.is_some() && evidence.6.is_some(),
        "a steady reconcile must not replace chain sets with self-only sets"
    );
    assert!(
        evidence.7.is_some() && evidence.8.is_some(),
        "lead transition must reach lead and worker"
    );
    assert_up_loop_full_refresh_keeps_a_retired_middle_as_live_child_topology();
}

#[cfg(all(test, feature = "wasm-resolver"))]
fn assert_up_loop_full_refresh_keeps_a_retired_middle_as_live_child_topology() {
    for retirement in [
        "retired #true",
        "desired-state \"retired\" reason=\"fixture\"",
    ] {
        let catalog = tempfile::tempdir().unwrap();
        write_notify_chain_profile(catalog.path());
        let (root_dir, root_goal) =
            write_notify_chain_agent(catalog.path(), "root", None, false);
        let (middle_dir, _middle_goal) = write_notify_chain_agent_with_state(
            catalog.path(),
            "middle",
            Some("hetz.root"),
            false,
            Some(retirement),
        );
        let (child_dir, _child_goal) =
            write_notify_chain_agent(catalog.path(), "child", Some("hetz.middle"), false);
        let specs = crate::discover_strict(catalog.path()).specs;
        let sessions = specs
            .iter()
            .filter(|spec| spec.desired_state.is_running())
            .flat_map(|spec| {
                spec.tasks.iter().map(|task| {
                    let id = task
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("{}.{}", spec.bus_id("hetz"), task.name));
                    sess(&id, true)
                })
            })
            .collect();
        let (entered_tx, _entered_rx) = mpsc::sync_channel(1);
        let (_release_tx, release_rx) = mpsc::channel();
        let runner = SteadyChainRunner {
            sessions: std::sync::Mutex::new(sessions),
            block_id: "never-block".to_owned(),
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        };
        let stop = AtomicBool::new(false);
        let missing_supervisor = AtomicBool::new(false);
        let (first_report_tx, first_report_rx) = mpsc::sync_channel(1);

        let evidence = std::thread::scope(|scope| {
            let observer_stop = &stop;
            let observer = scope.spawn(move || {
                first_report_rx.recv().unwrap();
                // Let the asynchronous full refresh replace the synchronous install before
                // mutating the root carrier. The child must retain the complete catalog chain.
                std::thread::sleep(Duration::from_millis(300));
                std::fs::write(&root_goal, "root transition after full refresh\n").unwrap();
                let root_event = wait_for_resync_event_for_key(&root_dir, "goal");
                let child_event =
                    wait_for_resync_event_for_key(&child_dir, "goal@hetz.root");
                let middle_event = current_resync_event_for_key(&middle_dir, "goal@hetz.root");
                observer_stop.store(true, Ordering::SeqCst);
                (root_event, child_event, middle_event)
            });
            up_loop_until(
                catalog.path(),
                "hetz",
                &runner,
                Duration::from_millis(25),
                &stop,
                |_, _| None,
                |report| {
                    if report
                        .errors
                        .iter()
                        .chain(&report.warnings)
                        .any(|message| message.contains("MissingSupervisor"))
                    {
                        missing_supervisor.store(true, Ordering::SeqCst);
                    }
                    let _ = first_report_tx.try_send(());
                },
            )
            .unwrap();
            observer.join().unwrap()
        });

        assert!(
            evidence.0.is_some(),
            "root must receive its own event ({retirement})"
        );
        assert!(
            evidence.1.is_some(),
            "the live child must receive exactly its owner-qualified root event through the \
                 retired middle after full refresh ({retirement})"
        );
        assert!(
            evidence.2.is_none(),
            "the retired middle must own no active subscription ({retirement})"
        );
        assert!(
            !missing_supervisor.load(Ordering::SeqCst),
            "the complete catalog graph must prevent MissingSupervisor ({retirement})"
        );
    }
}

#[test]
fn compile_invalid_seat_does_not_block_existing_live_resync_watch() {
    let catalog = tempfile::tempdir().unwrap();
    let (live_dir, live_goal) = write_resync_agent(catalog.path(), "live");
    let broken_dir = catalog.path().join("agents/hetz/broken");
    let broken_resources = broken_dir.join("resources");
    std::fs::create_dir_all(&broken_resources).unwrap();
    std::fs::create_dir_all(catalog.path().join("broken-workspace")).unwrap();
    let broken_declaration = broken_dir.join("agent.kdl");
    std::fs::write(
        &broken_declaration,
        r#"agent "broken" {
  host "hetz"
  deliver "mcp"
  workspace "$CATALOG/broken-workspace"
  exec "agent" { command "true" }
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let broken_goal = broken_resources.join("goal.md");
    std::fs::write(&broken_goal, "before\n").unwrap();
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();

    let runner = SpawnCountingRunner {
        sessions: RefCell::new(vec![
            sess("hetz.live", true),
            sess("hetz.broken.agent", true),
        ]),
        ..SpawnCountingRunner::default()
    };
    let task_context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());
    let mut cap = FlappingCap::default();
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    let mut presentation_cursor = PresentationPatchCursor::default();

    let first = reconcile_pass(catalog.path(),
    "hetz",
    &task_context,
    &runner,
    &mut cap,
    &mut debounce,
    &mut presentation_cursor,
    Some(&resync), None);
    assert!(
        first.errors.iter().any(|error| {
            error.contains("compile generated tasks")
                && error.contains("non-PTY canonical task")
        }),
        "{first:#?}"
    );
    assert!(!first.skipped, "the valid subset completed its pass");
    assert!(
        runner.spawned.borrow().is_empty(),
        "the compile-invalid seat must not launch"
    );

    std::fs::write(&live_goal, "changed while compile failed\n").unwrap();
    let first_event = wait_for_resync_event(&live_dir)
        .expect("the already-live valid seat must stay watched across the compile error");
    assert!(first_event.contains(r#""binding":"goal""#), "{first_event}");

    std::fs::write(&broken_goal, "invalid seat changed\n").unwrap();
    std::thread::sleep(Duration::from_millis(750));
    assert!(
        current_resync_event(&broken_dir).is_none(),
        "a compile-invalid seat must not be watched even when its canonical task is live"
    );

    std::fs::write(&live_goal, "changed while declaration is corrected\n").unwrap();
    std::fs::write(
        &broken_declaration,
        r#"agent "broken" {
  host "hetz"
  deliver "mcp"
  workspace "$CATALOG/broken-workspace"
  pty "agent" { command "true" }
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let corrected = reconcile_pass(catalog.path(),
    "hetz",
    &task_context,
    &runner,
    &mut cap,
    &mut debounce,
    &mut presentation_cursor,
    Some(&resync), None);
    assert!(
        corrected
            .errors
            .iter()
            .all(|error| !error.contains("compile generated tasks")),
        "{corrected:#?}"
    );
    assert!(corrected.launched.is_empty(), "{corrected:#?}");
    assert!(
        corrected.adopted.iter().any(|identity| identity == "broken"),
        "the corrected already-live seat should be adopted: {corrected:#?}"
    );
    let corrected_event = wait_for_resync_event_change(&live_dir, &first_event)
        .expect("correcting another declaration must not reseed and hide the live transition");
    assert!(corrected_event.contains(r#""binding":"goal""#), "{corrected_event}");
}

#[test]
fn materialization_failure_retains_only_the_observed_live_resync_watch() {
    let catalog = tempfile::tempdir().unwrap();
    let write_broken_agent = |identity: &str| {
        let (agent_dir, goal) = write_resync_agent(catalog.path(), identity);
        let workspace = catalog.path().join(format!("{identity}-workspace"));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            agent_dir.join("agent.kdl"),
            format!(
                r#"agent "{identity}" {{
  host "hetz"
  workspace "{}"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
  render {{
    copy "_templates/{identity}.md" "AGENTS.md"
  }}
}}"#,
                workspace.display()
            ),
        )
        .unwrap();
        (agent_dir, goal)
    };
    let (live_dir, live_goal) = write_broken_agent("live");
    let (dormant_dir, dormant_goal) = write_broken_agent("dormant");
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();

    let runner = SpawnCountingRunner {
        sessions: RefCell::new(vec![sess("hetz.live", true)]),
        ..SpawnCountingRunner::default()
    };
    let task_context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());
    let mut cap = FlappingCap::default();
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    let mut presentation_cursor = PresentationPatchCursor::default();

    let failed = reconcile_pass(catalog.path(),
    "hetz",
    &task_context,
    &runner,
    &mut cap,
    &mut debounce,
    &mut presentation_cursor,
    Some(&resync), None);
    assert!(
        failed
            .errors
            .iter()
            .filter(|error| error.contains("copy source"))
            .count()
            >= 2,
        "{failed:#?}"
    );
    assert!(
        runner.spawned.borrow().is_empty(),
        "materialization-failed seats must not launch"
    );

    std::fs::write(&live_goal, "changed while materialization failed\n").unwrap();
    std::fs::write(&dormant_goal, "unwatched while materialization failed\n").unwrap();
    let first_event = wait_for_resync_event(&live_dir)
        .expect("the observed live seat must remain watched through materialization failure");
    assert!(first_event.contains(r#""binding":"goal""#), "{first_event}");
    std::thread::sleep(Duration::from_millis(750));
    assert!(
        current_resync_event(&dormant_dir).is_none(),
        "a materialization-failed seat without an observed live session must stay unwatched"
    );

    std::fs::write(&live_goal, "changed immediately before recovery\n").unwrap();
    std::fs::create_dir_all(catalog.path().join("_templates")).unwrap();
    std::fs::write(catalog.path().join("_templates/live.md"), "rendered\n").unwrap();
    let recovered = reconcile_pass(catalog.path(),
    "hetz",
    &task_context,
    &runner,
    &mut cap,
    &mut debounce,
    &mut presentation_cursor,
    Some(&resync), None);
    assert!(
        recovered
            .errors
            .iter()
            .all(|error| !error.contains("_templates/live.md")),
        "{recovered:#?}"
    );
    assert!(recovered.launched.is_empty(), "{recovered:#?}");
    let recovered_event = wait_for_resync_event_change(&live_dir, &first_event)
        .expect("recovery must preserve the pending transition instead of silently reseeding");
    assert!(
        recovered_event.contains(r#""binding":"goal""#),
        "{recovered_event}"
    );
}

fn execute_resync_plan(
    plan: &ReconcilePlan<'_>,
    runner: &dyn Runner,
    specs: &[AgentSpec],
    resync: &crate::resync::ResyncSupervisor,
) -> UpReport {
    let mut report = UpReport::default();
    let mut install_count = 0;
    execute_with_presentation_cursor(
        plan,
        runner,
        &mut FlappingCap::default(),
        &mut PresentationPatchCursor::default(),
        &mut report,
        &mut |spec| {
            install_count += 1;
            assert!(resync.install_live(spec, specs, "hetz").is_empty());
        },
    );
    assert!(
        resync
            .refresh(
                specs,
                &live_resync_specs(specs, "hetz", &[], &report),
                "hetz",
                &[],
                &[],
            )
            .is_empty()
    );
    assert!(install_count > 0 || report.launched.is_empty());
    report
}

#[cfg(feature = "wasm-resolver")]
#[test]
fn notify_chain_launch_boundary_installs_ancestors_before_a_later_task_finishes() {
    let catalog = tempfile::tempdir().unwrap();
    write_notify_chain_profile(catalog.path());
    let (_root_dir, root_goal) =
        write_notify_chain_agent(catalog.path(), "root", None, false);
    write_notify_chain_agent(catalog.path(), "lead", Some("hetz.root"), false);
    let (worker_dir, _worker_goal) =
        write_notify_chain_agent(catalog.path(), "worker", Some("hetz.lead"), false);
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let specs = crate::discover_strict(catalog.path()).specs;
    let worker = specs
        .iter()
        .find(|spec| spec.identity == "worker")
        .unwrap();
    let mut later = target("hetz.worker.later", "later");
    later.name = "later".into();
    later.derived = true;
    let plan = ReconcilePlan {
        launch: vec![Launch {
            spec: worker,
            tasks: vec![target("hetz.worker.agent", "agent"), later],
            live_derived: Vec::new(),
        }],
        ..ReconcilePlan::default()
    };
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let runner = BlockingLaunchRunner {
        sessions: RefCell::new(Vec::new()),
        fail_id: None,
        block_id: "hetz.worker.later".to_owned(),
        entered: entered_tx,
        release: RefCell::new(release_rx),
    };
    let resync = crate::resync::ResyncSupervisor::with_profiles(
        catalog.path().to_path_buf(),
        "hetz".into(),
        crate::catalog::declared_profiles(catalog.path()).unwrap(),
    );

    let event = std::thread::scope(|scope| {
        let observer = scope.spawn(move || {
            entered_rx.recv().unwrap();
            std::fs::write(&root_goal, "changed while later task launches\n").unwrap();
            let event =
                wait_for_resync_event_for_key(&worker_dir, "goal@hetz.root");
            release_tx.send(()).unwrap();
            event
        });
        let report = execute_resync_plan(&plan, &runner, &specs, &resync);
        assert_eq!(
            report.launched,
            ["hetz.worker.agent", "hetz.worker.later"]
        );
        observer.join().unwrap()
    })
    .expect("the fresh worker must receive its ancestor transition before full refresh");
    assert!(event.contains("key: goal@hetz.root"), "{event}");
}

#[test]
fn resync_launch_boundary_seeds_first_seat_before_later_seat_finishes() {
    let catalog = tempfile::tempdir().unwrap();
    let (first_dir, first_goal) = write_resync_agent(catalog.path(), "first");
    write_resync_agent(catalog.path(), "second");
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let specs = crate::discover_strict(catalog.path()).specs;
    let first = specs.iter().find(|spec| spec.identity == "first").unwrap();
    let second = specs.iter().find(|spec| spec.identity == "second").unwrap();
    let plan = ReconcilePlan {
        launch: vec![
            Launch {
                spec: first,
                tasks: vec![target("hetz.first.agent", "agent")],
                live_derived: Vec::new(),
            },
            Launch {
                spec: second,
                tasks: vec![target("hetz.second.agent", "agent")],
                live_derived: Vec::new(),
            },
        ],
        ..ReconcilePlan::default()
    };
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let runner = BlockingLaunchRunner {
        sessions: RefCell::new(Vec::new()),
        fail_id: None,
        block_id: "hetz.second.agent".to_owned(),
        entered: entered_tx,
        release: RefCell::new(release_rx),
    };
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());

    std::thread::scope(|scope| {
        scope.spawn(move || {
            entered_rx.recv().unwrap();
            std::fs::write(&first_goal, "changed while second launches\n").unwrap();
            std::thread::sleep(Duration::from_secs(1));
            release_tx.send(()).unwrap();
        });
        let report = execute_resync_plan(&plan, &runner, &specs, &resync);
        assert_eq!(
            report.launched,
            ["hetz.first.agent", "hetz.second.agent"]
        );
    });

    let event = wait_for_resync_event(&first_dir)
        .expect("the first seat must observe a carrier transition during the later launch");
    assert!(event.contains(r#""binding":"goal""#), "{event}");
}

#[test]
fn resync_launch_boundary_excludes_failed_canonical_seat() {
    let catalog = tempfile::tempdir().unwrap();
    let (first_dir, first_goal) = write_resync_agent(catalog.path(), "first");
    write_resync_agent(catalog.path(), "second");
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let specs = crate::discover_strict(catalog.path()).specs;
    let first = specs.iter().find(|spec| spec.identity == "first").unwrap();
    let second = specs.iter().find(|spec| spec.identity == "second").unwrap();
    let plan = ReconcilePlan {
        launch: vec![
            Launch {
                spec: first,
                tasks: vec![target("hetz.first.agent", "agent")],
                live_derived: Vec::new(),
            },
            Launch {
                spec: second,
                tasks: vec![target("hetz.second.agent", "agent")],
                live_derived: Vec::new(),
            },
        ],
        ..ReconcilePlan::default()
    };
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let runner = BlockingLaunchRunner {
        sessions: RefCell::new(Vec::new()),
        fail_id: Some("hetz.first.agent".to_owned()),
        block_id: "hetz.second.agent".to_owned(),
        entered: entered_tx,
        release: RefCell::new(release_rx),
    };
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());

    std::thread::scope(|scope| {
        scope.spawn(move || {
            entered_rx.recv().unwrap();
            std::fs::write(&first_goal, "changed after failed launch\n").unwrap();
            std::thread::sleep(Duration::from_secs(1));
            release_tx.send(()).unwrap();
        });
        let report = execute_resync_plan(&plan, &runner, &specs, &resync);
        assert_eq!(report.launched, ["hetz.second.agent"]);
        assert!(report.errors.iter().any(|error| {
            error.contains("hetz.first.agent") && error.contains("simulated launch failure")
        }));
    });

    std::thread::sleep(Duration::from_millis(750));
    assert!(
        current_resync_event(&first_dir).is_none(),
        "desired-but-failed canonical seats must remain unwatched"
    );
}

#[test]
fn dead_resync_seat_is_deactivated_before_its_relaunch_blocks() {
    let catalog = tempfile::tempdir().unwrap();
    let (agent_dir, goal) = write_resync_agent(catalog.path(), "worker");
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let runner = BlockingLaunchRunner {
        sessions: RefCell::new(vec![sess("hetz.worker", true)]),
        fail_id: None,
        block_id: "hetz.worker".to_owned(),
        entered: entered_tx,
        release: RefCell::new(release_rx),
    };
    let task_context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());
    let mut cap = FlappingCap::default();
    let mut debounce = LivenessDebounce::new(Duration::ZERO);
    let mut presentation_cursor = PresentationPatchCursor::default();

    let seeded = reconcile_pass(catalog.path(),
    "hetz",
    &task_context,
    &runner,
    &mut cap,
    &mut debounce,
    &mut presentation_cursor,
    Some(&resync), None);
    assert!(seeded.adopted.iter().any(|identity| identity == "worker"));
    *runner.sessions.borrow_mut() = vec![sess("hetz.worker", false)];
    let blocked_goal = goal.clone();

    let relaunched = std::thread::scope(|scope| {
        scope.spawn(move || {
            entered_rx.recv().unwrap();
            std::fs::write(&blocked_goal, "changed while replacement launch blocks\n").unwrap();
            std::thread::sleep(Duration::from_secs(1));
            release_tx.send(()).unwrap();
        });
        reconcile_pass(catalog.path(),
        "hetz",
        &task_context,
        &runner,
        &mut cap,
        &mut debounce,
        &mut presentation_cursor,
        Some(&resync), None)
    });
    assert_eq!(relaunched.restarted, ["hetz.worker"]);
    std::thread::sleep(Duration::from_millis(750));
    assert!(
        current_resync_event(&agent_dir).is_none(),
        "a carrier mutation while no canonical seat is live must not emit"
    );

    std::fs::write(&goal, "changed after replacement launch\n").unwrap();
    let event = wait_for_resync_event(&agent_dir)
        .expect("the successful replacement must receive a fresh silent baseline");
    assert!(event.contains(r#""binding":"goal""#), "{event}");
}

/// A publication the resync worker cannot finish must not hold up a reconcile pass.
///
/// Every per-seat `install_live` handshake is answered by the same worker thread that runs
/// publications, so a publication in progress serializes the whole pass behind it. That is the
/// coupling which let a terminal-refusal loop keep every pass from completing for two hours
/// (#431): the refusals only had power because they denied the pass that would have ended
/// them. Blocking one real publication on the recipient's stream lock is the sharpest form of
/// the same coupling — a slow publication makes a pass late, a stuck one makes it never
/// finish — and it holds the pass at exactly the point `emit_admitted` serializes.
#[test]
fn reconcile_pass_completes_while_a_resync_publication_is_blocked() {
    use std::os::fd::AsRawFd as _;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    let catalog = tempfile::tempdir().unwrap();
    let (agent_dir, goal) = write_resync_agent(catalog.path(), "worker");
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let runner = SpawnCountingRunner {
        sessions: RefCell::new(vec![sess("hetz.worker", true)]),
        ..Default::default()
    };
    let task_context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());
    let mut cap = FlappingCap::default();
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    let mut presentation_cursor = PresentationPatchCursor::default();

    let seeded = reconcile_pass(
        catalog.path(),
        "hetz",
        &task_context,
        &runner,
        &mut cap,
        &mut debounce,
        &mut presentation_cursor,
        Some(&resync),
        None,
    );
    assert!(
        seeded.adopted.iter().any(|identity| identity == "worker"),
        "{seeded:#?}"
    );

    // One completed publication first: it creates the recipient's resync stream state
    // directory, whose `.lock` is the gate below, and proves the publication path is live.
    std::fs::write(&goal, "changed before the gate closes\n").unwrap();
    let published = wait_for_resync_event(&agent_dir)
        .expect("the live seat must observe its first carrier transition");

    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("resources/streams/resync/.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) },
        0,
        "the test must own the stream lock the publication path takes"
    );
    let gate_fd = gate.as_raw_fd();

    // The worker has no other work and `IMMEDIATE_WINDOW` is 500 ms, so it is inside the
    // blocked publication well before this wait ends; the unchanged event is the positive
    // evidence that the publication has not completed.
    std::fs::write(&goal, "changed while the gate is closed\n").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        current_resync_event(&agent_dir).as_deref(),
        Some(published.as_str()),
        "the gate must hold the second publication open"
    );

    // The watchdog releases the gate only when the pass fails to complete on its own, which
    // is what separates a decoupled pass from one that merely finished after the rescue.
    let rescued = AtomicBool::new(false);
    let rescued_flag = &rescued;
    let (finished_tx, finished_rx) = mpsc::channel::<()>();
    let pass = std::thread::scope(|scope| {
        scope.spawn(move || {
            if finished_rx.recv_timeout(Duration::from_secs(20)).is_err() {
                rescued_flag.store(true, AtomicOrdering::SeqCst);
                unsafe { libc::flock(gate_fd, libc::LOCK_UN) };
            }
        });
        let pass = reconcile_pass(
            catalog.path(),
            "hetz",
            &task_context,
            &runner,
            &mut cap,
            &mut debounce,
            &mut presentation_cursor,
            Some(&resync),
            None,
        );
        let _ = finished_tx.send(());
        pass
    });
    assert!(
        !rescued.load(AtomicOrdering::SeqCst),
        "the pass only completed after the blocked publication was released: {pass:#?}"
    );
    assert!(
        pass.adopted.iter().any(|identity| identity == "worker"),
        "{pass:#?}"
    );
    drop(gate);
}

#[test]
fn resync_launch_boundary_preserves_baseline_across_derived_companion() {
    let catalog = tempfile::tempdir().unwrap();
    let (agent_dir, goal) = write_resync_agent(catalog.path(), "worker");
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let specs = crate::discover_strict(catalog.path()).specs;
    let spec = &specs[0];
    let mut derived = target("hetz.worker.ding", "ding");
    derived.name = "ding".into();
    derived.derived = true;
    let plan = ReconcilePlan {
        launch: vec![Launch {
            spec,
            tasks: vec![target("hetz.worker.agent", "agent"), derived],
            live_derived: Vec::new(),
        }],
        ..ReconcilePlan::default()
    };
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let runner = BlockingLaunchRunner {
        sessions: RefCell::new(Vec::new()),
        fail_id: None,
        block_id: "hetz.worker.ding".to_owned(),
        entered: entered_tx,
        release: RefCell::new(release_rx),
    };
    let resync =
        crate::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".into());
    let installs = AtomicUsize::new(0);
    let mut report = UpReport::default();

    std::thread::scope(|scope| {
        scope.spawn(move || {
            entered_rx.recv().unwrap();
            std::fs::write(&goal, "changed while companion launches\n").unwrap();
            std::thread::sleep(Duration::from_secs(1));
            release_tx.send(()).unwrap();
        });
        execute_with_presentation_cursor(
            &plan,
            &runner,
            &mut FlappingCap::default(),
            &mut PresentationPatchCursor::default(),
            &mut report,
            &mut |spec| {
                installs.fetch_add(1, AtomicOrdering::SeqCst);
                assert!(resync.install_live(spec, &specs, "hetz").is_empty());
            },
        );
    });
    assert!(
        resync
            .refresh(
                &specs,
                &live_resync_specs(&specs, "hetz", &[], &report),
                "hetz",
                &[],
                &[],
            )
            .is_empty()
    );

    assert_eq!(
        installs.load(AtomicOrdering::SeqCst),
        1,
        "only the canonical task transition may install its watch set"
    );
    let event = wait_for_resync_event(&agent_dir)
        .expect("the companion launch and final refresh must preserve the canonical baseline");
    assert!(event.contains(r#""binding":"goal""#), "{event}");
}

#[test]
fn debounce_absorbs_a_gc_flicker_but_reaps_a_stable_death() {
    let t0 = Instant::now();
    let mut db = LivenessDebounce::new(Duration::from_secs(10));
    db.observe(&[sess("hetz.demo.agent", true)], t0);

    // Flicker: reads not-alive 1s later but was alive within the grace → deferred (left running).
    let mut plan = ReconcilePlan::default();
    plan.gc.push("hetz.demo.agent".into());
    let deferred = db.defer_flickers(&mut plan, t0 + Duration::from_secs(1));
    assert!(
        plan.gc.is_empty(),
        "a recently-alive flicker must NOT be GC'd"
    );
    assert_eq!(deferred, vec!["hetz.demo.agent".to_string()]);

    // CENTRAL anti-over-correction check: a STABLE death past the grace IS still reaped — the
    // debounce must never MASK a real death.
    let mut plan = ReconcilePlan::default();
    plan.gc.push("hetz.demo.agent".into());
    let deferred = db.defer_flickers(&mut plan, t0 + Duration::from_secs(11));
    assert_eq!(
        plan.gc,
        vec!["hetz.demo.agent".to_string()],
        "a stable death must still be reaped"
    );
    assert!(deferred.is_empty());
}

#[test]
fn effective_pty_root_prefers_an_exported_ambient_root_else_catalog_pty() {
    let cat = std::path::Path::new("/deep/sandbox/st-root");
    // No ambient PTY_ROOT → the rendered default `<catalog>/pty`.
    assert_eq!(effective_pty_root_from(cat, None), cat.join("pty"));
    assert_eq!(
        effective_pty_root_from(cat, Some("".into())),
        cat.join("pty"),
        "empty is treated as unset"
    );
    // An exported ambient PTY_ROOT (e.g. an eval's short decoupled root) WINS — so a deep catalog
    // path can't blow the unix-socket limit, and spawn agrees with list/kill.
    let short = std::ffi::OsString::from("/tmp/stev-abc123");
    assert_eq!(
        effective_pty_root_from(cat, Some(short)),
        std::path::PathBuf::from("/tmp/stev-abc123")
    );
}

#[test]
fn a_catalog_declared_root_outranks_the_default_but_never_an_ambient_one() {
    let tmp = tempfile::tempdir().unwrap();
    let cat = tmp.path();
    std::fs::write(
        cat.join(crate::catalog::CONFIG_FILE),
        "catalog { pty-root \"/run/agents/pty\" }\n",
    )
    .unwrap();

    // The declaration replaces the `<catalog>/pty` default for every st2 pty op — so a reader
    // that resolves the catalog finds the sessions without being handed an env var.
    assert_eq!(
        effective_pty_root_from(cat, None),
        std::path::PathBuf::from("/run/agents/pty")
    );
    // An explicit ambient root still wins: an eval run's short decoupled partition must be able
    // to override a catalog it copied from.
    assert_eq!(
        effective_pty_root_from(cat, Some("/tmp/stev-abc123".into())),
        std::path::PathBuf::from("/tmp/stev-abc123")
    );
}

#[test]
fn debounce_never_defers_a_never_seen_task() {
    let t0 = Instant::now();
    let db = LivenessDebounce::new(Duration::from_secs(10));
    // A genuinely-new target (never observed alive) is handled immediately, not deferred.
    let mut plan = ReconcilePlan::default();
    plan.gc.push("hetz.brandnew.agent".into());
    let deferred = db.defer_flickers(&mut plan, t0);
    assert_eq!(plan.gc, vec!["hetz.brandnew.agent".to_string()]);
    assert!(deferred.is_empty());
}

#[test]
fn debounce_defers_a_flickering_launch_target_too() {
    let t0 = Instant::now();
    let mut db = LivenessDebounce::new(Duration::from_secs(10));
    db.observe(&[sess("hetz.demo.agent", true)], t0);

    // The same recently-alive id showing up as a launch target (Absent/Dead) is also deferred —
    // no noisy "already in use" re-launch of a live session.
    let spec = spec_fixture();
    let mut plan = ReconcilePlan::default();
    plan.launch.push(Launch {
        spec: &spec,
        tasks: vec![target("hetz.demo.agent", "x")],
        live_derived: Vec::new(),
    });
    let deferred = db.defer_flickers(&mut plan, t0 + Duration::from_secs(2));
    assert!(
        plan.launch.is_empty(),
        "a recently-alive flicker must NOT be re-launched"
    );
    assert_eq!(deferred, vec!["hetz.demo.agent".to_string()]);
}

#[test]
fn codex_hook_gate_accepts_new_agents_without_mutating_the_launch_plan() {
    let mut left = spec_fixture();
    left.identity = "left".into();
    left.path = PathBuf::from("/catalog/node/left/agent.kdl");
    let mut right = spec_fixture();
    right.identity = "right".into();
    right.path = PathBuf::from("/catalog/node/right/agent.kdl");
    let mut left_agent = target("node.left.agent", "exec codex --model gpt-5");
    left_agent.workspace = Some("/workspaces/shared".into());
    let mut right_agent = target("node.right.agent", "/opt/bin/codex --model gpt-5");
    right_agent.workspace = Some("/workspaces/shared".into());
    let mut plan = ReconcilePlan::default();
    plan.launch.push(Launch {
        spec: &left,
        tasks: vec![left_agent],
        live_derived: Vec::new(),
    });
    plan.launch.push(Launch {
        spec: &right,
        tasks: vec![right_agent],
        live_derived: Vec::new(),
    });
    let expected = plan
        .launch
        .iter()
        .map(|launch| launch.spec.identity.clone())
        .collect::<Vec<_>>();
    let mut report = UpReport::default();

    gate_harness_launches_on_hooks(&mut plan, Path::new("/catalog"), &mut report, |_| Ok(()));

    assert_eq!(
        plan.launch
            .iter()
            .map(|launch| launch.spec.identity.clone())
            .collect::<Vec<_>>(),
        expected,
        "successful hook verification must leave the launch plan unchanged"
    );
    assert_eq!(plan.launch.len(), 2);
    assert!(report.errors.is_empty());
}

#[test]
fn codex_hook_gate_does_not_touch_adopted_agents_or_sidecar_only_repairs() {
    let mut spec = spec_fixture();
    spec.identity = "root".into();
    let mut ding = target("node.root.ding", "st2 ding");
    ding.name = "ding".into();
    let mut plan = ReconcilePlan::default();
    plan.adopt.push(&spec);
    plan.launch.push(Launch {
        spec: &spec,
        tasks: vec![ding],
        live_derived: Vec::new(),
    });
    let mut report = UpReport::default();

    gate_harness_launches_on_hooks(&mut plan, Path::new("/catalog"), &mut report, |_| {
        panic!("an already-live Codex agent must not enter the hook gate")
    });

    assert_eq!(plan.adopt, [&spec]);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks[0].name, "ding");
    assert!(report.errors.is_empty());
}

#[test]
fn hook_verification_failure_suppresses_only_new_codex_agents() {
    let mut codex = spec_fixture();
    codex.identity = "codex".into();
    codex.path = PathBuf::from("/catalog/node/codex/agent.kdl");
    let mut claude = spec_fixture();
    claude.identity = "claude".into();
    claude.path = PathBuf::from("/catalog/node/claude/agent.kdl");
    let mut codex_agent = target("node.codex.agent", "exec codex");
    codex_agent.workspace = Some("/workspaces/codex".into());
    let claude_agent = target("node.claude.agent", "exec claude");
    let mut plan = ReconcilePlan::default();
    plan.launch.push(Launch {
        spec: &codex,
        tasks: vec![codex_agent],
        live_derived: Vec::new(),
    });
    plan.launch.push(Launch {
        spec: &claude,
        tasks: vec![claude_agent],
        live_derived: Vec::new(),
    });
    let mut report = UpReport::default();

    gate_harness_launches_on_hooks(&mut plan, Path::new("/catalog"), &mut report, |_| {
        anyhow::bail!("stale receipt")
    });

    assert_eq!(
        plan.launch
            .iter()
            .map(|launch| launch.spec.identity.as_str())
            .collect::<Vec<_>>(),
        ["claude"]
    );
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("stale receipt"));
    assert!(report.errors[0].contains("launch suppressed"));
}

/// The built `pty run` argv runs the command verbatim under `sh -c`, detached, with the pinned id
/// and the established fallback presentation when no Agent Spec name is projected.
#[test]
fn build_run_command_wraps_command_in_sh_c() {
    let cli = PtyCli::default();
    let t = target(
        "hetz.demo.agent",
        "exec claude --permission-mode bypassPermissions 'boot'",
    );
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

    assert_eq!(cmd.get_program(), OsStr::new("pty"));
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // Stable launch/config arguments precede the persisted environment and command separator.
    assert_eq!(&args[0..2], &["run", "-d"]);
    assert!(args.contains(&"--force".to_string()));
    let id_pos = args.iter().position(|a| a == "--id").unwrap();
    assert_eq!(args[id_pos + 1], "hetz.demo.agent");
    let name_pos = args.iter().position(|a| a == "--name").unwrap();
    assert_eq!(args[name_pos + 1], "hetz.demo");
    let sep = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(
        &args[sep + 1..],
        &[
            "sh",
            "-c",
            "exec claude --permission-mode bypassPermissions 'boot'"
        ]
    );
}

#[test]
fn build_run_command_projects_primary_name_and_owned_tags_at_spawn() {
    let key = "ST2_TEST_PRESENTATION_LITERAL_71c";
    unsafe { std::env::set_var(key, "expanded") }

    let cli = PtyCli::default();
    let mut t = target("hetz.demo", "codex");
    t.bus_id = "hetz.demo".to_owned();
    t.tags
        .insert("unrelated".to_owned(), "preserved".to_owned());
    t.presentation = Some(PtyPresentation {
        pty_id: "hetz.demo".to_owned(),
        display_name: Some(Some("Build owner".to_owned())),
        tags: BTreeMap::from([
            ("agent.presentation.schema".to_owned(), Some("1".to_owned())),
            ("agent.actor.path".to_owned(), Some("hetz.demo".to_owned())),
            (
                "agent.presentation.description".to_owned(),
                Some(format!("${key}")),
            ),
        ]),
    });
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
    let args = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    let name = args.iter().position(|arg| arg == "--name").unwrap();
    assert_eq!(args[name + 1], "Build owner");
    let tags = args
        .windows(2)
        .filter(|pair| pair[0] == "--tag")
        .map(|pair| pair[1].as_str())
        .collect::<BTreeSet<_>>();
    assert!(tags.contains("unrelated=preserved"));
    assert!(tags.contains("agent.presentation.schema=1"));
    assert!(tags.contains("agent.actor.path=hetz.demo"));
    assert!(tags.contains("agent.presentation.description=$ST2_TEST_PRESENTATION_LITERAL_71c"));
}

#[test]
fn metadata_patch_uses_exact_id_and_one_json_stdin_payload() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("pty-capture");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$0.args"
printf '' > "$0.ready.tmp"
mv "$0.ready.tmp" "$0.ready"
while [ ! -e "$0.release" ]; do sleep 0.01; done
cat > "$0.stdin"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    reset_fixture_barrier(&executable);
    let observed_executable = executable.clone();
    let cli = PtyCli {
        bin: executable.display().to_string(),
        catalog_root: temporary.path().to_path_buf(),
        on_command_spawn: Some(std::sync::Arc::new(move |pid| {
            release_ready_fixture(
                pid,
                &observed_executable,
                "fake PTY metadata command was not ready",
            );
        })),
    };
    let presentation = PtyPresentation {
        pty_id: "stable.agent.id".to_owned(),
        display_name: Some(None),
        tags: BTreeMap::from([
            ("agent.presentation.schema".to_owned(), Some("1".to_owned())),
            ("agent.presentation.description".to_owned(), None),
        ]),
    };

    cli.patch_presentation(&presentation).unwrap();

    assert_eq!(
        std::fs::read_to_string(executable.with_extension("args")).unwrap(),
        "metadata\npatch\n--id\nstable.agent.id\n"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&std::fs::read(executable.with_extension("stdin")).unwrap())
            .unwrap();
    assert_eq!(payload["displayName"], serde_json::Value::Null);
    assert_eq!(payload["tags"]["agent.presentation.schema"], "1");
    assert_eq!(
        payload["tags"]["agent.presentation.description"],
        serde_json::Value::Null
    );
}

#[test]
fn input_write_failure_terminates_and_reaps_the_child() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("close-stdin");
    let stdin_closed = temporary.path().join("stdin-closed");
    // The script signals only AFTER closing its stdin, so the barrier below returns exactly when
    // the read end is gone and the parent's very next write must fail with EPIPE.
    std::fs::write(
        &executable,
        "#!/bin/sh\nexec 0<&-\n: > \"$READY.tmp\"\nmv \"$READY.tmp\" \"$READY\"\nsleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let input = vec![b'x'; 1024 * 1024];
    // The pid comes from the parent at spawn, and the barrier makes the deadline measure only
    // the behaviour under test. Without it the 1s budget also had to cover fork+exec of the
    // shell, so a loaded host reported `timed out after 1.0s` instead of `Broken pipe` — the
    // fixture's scheduling consumed the deadline the assertion is about.
    let mut spawned = None;
    let error = output_with_input_timeout_observed(
        Command::new(&executable).env("READY", &stdin_closed),
        Duration::from_secs(1),
        Some(input),
        |pid| {
            spawned = Some(pid);
            await_fixture_ready(pid, &stdin_closed, "the child never closed its stdin");
        },
    )
    .unwrap_err();
    let pid = spawned.expect("the child was spawned before the input write failed");

    assert!(
        format!("{error:#}").contains("Broken pipe"),
        "unexpected write error: {error:#}"
    );
    assert!(
        !crate::host_lock::process_alive(pid),
        "failed metadata child {pid} was not terminated and reaped"
    );
}

/// The process-group kill is the entire stated reason [`terminate_and_reap_before`] exists — its
/// docstring is about an escaped descendant that inherited stdout/stderr and would otherwise
/// block cleanup. Nothing constructed such a descendant, so `kill(-pid, SIGKILL)` was asserted
/// by no test: removing it alone left the suite green, because `child.kill()` already satisfies
/// every assertion that only looks at the direct child.
#[test]
fn the_group_kill_reaps_a_descendant_that_outlives_the_direct_child() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("spawn-descendant");
    let descendant_pidfile = temporary.path().join("descendant.pid");
    // The descendant inherits stdout/stderr and outlives the direct child, which is exactly the
    // shape the docstring describes. `child.kill()` cannot reach it; only the group signal can.
    // It publishes its pid by atomic rename, so the barrier never reads a truncated file.
    std::fs::write(
        &executable,
        "#!/bin/sh\nsh -c 'printf \"%s\" \"$$\" > \"$DESCENDANT_PIDFILE.tmp\"; mv \"$DESCENDANT_PIDFILE.tmp\" \"$DESCENDANT_PIDFILE\"; sleep 60' &\nsleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

    // This test *requires* the child to have run — a descendant it never forked is nothing to
    // reap. Waiting for the pidfile inside `on_spawn` makes that a barrier instead of a race:
    // the deadline then only has to outlast a `sleep`, never a fork+exec, so a loaded host can
    // no longer end the run before the fixture has built the thing under test.
    let error = output_with_input_timeout_observed(
        Command::new(&executable).env("DESCENDANT_PIDFILE", &descendant_pidfile),
        Duration::from_millis(500),
        None,
        |pid| {
            await_fixture_ready(
                pid,
                &descendant_pidfile,
                "the child never forked a descendant, so this case would test nothing",
            )
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("timed out"),
        "unexpected error: {error:#}"
    );

    let descendant = std::fs::read_to_string(&descendant_pidfile)
        .expect("the readiness barrier returned without a pidfile")
        .parse::<i32>()
        .unwrap();

    // Generous on purpose: the descendant is orphaned by the same group kill, so its exit is
    // observable only once the reparenting init reaps it. That latency is not the behaviour
    // under test, and waiting longer costs nothing when the kill did reach it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_can_retain_cleanup_resources(descendant) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survived = process_can_retain_cleanup_resources(descendant);
    if survived {
        // Do not leak a 60s sleeper into the test host when the assertion is about to fail.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
    }
    assert!(
        !survived,
        "escaped descendant {descendant} survived cleanup: the process-group kill did not reach it"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_zombie_cannot_retain_cleanup_resources() {
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
    let pid = child.id() as i32;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut state = None;
    while Instant::now() < deadline {
        state = linux_process_state(pid);
        if state == Some('Z') {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let kill_probe_considered_alive = crate::host_lock::process_alive(pid);
    let retained_cleanup_resources = process_can_retain_cleanup_resources(pid);
    let _ = child.wait();

    assert_eq!(state, Some('Z'), "child did not become a zombie");
    assert!(
        kill_probe_considered_alive,
        "the fixture must expose kill(pid, 0) treating a zombie as alive"
    );
    assert!(
        !retained_cleanup_resources,
        "a terminated zombie cannot retain cleanup resources"
    );
}

#[test]
fn input_write_obeys_the_child_deadline() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("ignore-stdin");
    std::fs::write(&executable, "#!/bin/sh\nsleep 60\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let input = vec![b'x'; 1024 * 1024];
    // The pid comes from the parent at spawn, not from the child, and this case cannot use a
    // readiness barrier: it is precisely the one where the child may never be scheduled. The
    // write blocks as soon as the pipe buffer fills, which needs no execution by the child at
    // all, and the deadline then terminates the whole group. Anything the child was supposed to
    // record would never be written, so a test that waits for it fails on exactly the condition
    // it exists to cover. The observed spawn instant is therefore also the clock: timing from
    // before the call would charge fork+exec to the 1s budget this assertion polices.
    let mut spawned = None;
    let error = output_with_input_timeout_observed(
        &mut Command::new(&executable),
        Duration::from_millis(100),
        Some(input),
        |pid| spawned = Some((pid, Instant::now())),
    )
    .unwrap_err();
    let (pid, started) =
        spawned.expect("the child was spawned before the input deadline expired");

    assert!(
        format!("{error:#}").contains("timed out"),
        "unexpected write error: {error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "blocked stdin write ignored the child deadline"
    );
    let reap_deadline = Instant::now() + Duration::from_secs(1);
    while crate::host_lock::process_alive(pid) && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!crate::host_lock::process_alive(pid));
}

/// The two lifecycle tests above block in `on_spawn` until their fixture reached the state under
/// test, which only keeps them load-insensitive because [`run_captured`] starts the child
/// deadline AFTER `on_spawn` returns. Nothing else proves that order: reversing it leaves every
/// other test green on an idle host and silently puts both back on a race with the scheduler.
#[test]
fn the_spawn_observer_runs_before_the_child_deadline_starts() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("close-stdin");
    let stdin_closed = temporary.path().join("stdin-closed");
    std::fs::write(
        &executable,
        "#!/bin/sh\nexec 0<&-\n: > \"$READY.tmp\"\nmv \"$READY.tmp\" \"$READY\"\nsleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let timeout = Duration::from_millis(200);

    // The barrier deliberately outlasts `timeout`, so the outcome depends on the order alone and
    // on nothing the host's scheduler does. Deadline after `on_spawn`: the write meets a closed
    // read end and fails with EPIPE at once. Deadline before `on_spawn`: it has already expired
    // when the barrier returns, so the write never runs and the call reports a timeout instead.
    let error = output_with_input_timeout_observed(
        Command::new(&executable).env("READY", &stdin_closed),
        timeout,
        Some(vec![b'x'; 1024]),
        |pid| {
            await_fixture_ready(pid, &stdin_closed, "the child never closed its stdin");
            std::thread::sleep(timeout * 2);
        },
    )
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("Broken pipe"),
        "the child deadline started before `on_spawn` returned: {error:#}"
    );
}

#[test]
fn bounded_capture_keeps_the_tail_of_an_oversized_stream() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("flood");
    // Start marker, 1 MiB of filler (4x the cap, so both streams truncate), end marker.
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf START; head -c 1048576 /dev/zero; printf END\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output =
        output_with_timeout(&mut Command::new(&executable), Duration::from_secs(5)).unwrap();

    assert_eq!(output.stdout.len(), CAPTURE_CAP_BYTES);
    assert!(
        output.stdout.ends_with(b"END"),
        "capped stdout lost the tail"
    );
    assert!(
        !output.stdout.starts_with(b"START"),
        "capped stdout kept the head instead of the tail"
    );
    // stderr is empty here, so only the stdout read-back may have been capped.
}

#[test]
fn full_stdout_variant_returns_complete_output_larger_than_the_cap() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("flood");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf START; head -c 1048576 /dev/zero; printf END\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output =
        output_full_stdout_with_timeout(&mut Command::new(&executable), Duration::from_secs(5))
            .unwrap();

    assert!(output.stdout.len() > CAPTURE_CAP_BYTES);
    assert!(
        output.stdout.starts_with(b"START") && output.stdout.ends_with(b"END"),
        "full-stdout variant truncated structured output: {} bytes",
        output.stdout.len()
    );
}

/// Proves the shared reaper actually waits: the killed child is observed as a zombie BEFORE
/// `reap_detached` runs, so only the reaper's `wait()` can clear that state.
#[cfg(target_os = "linux")]
#[test]
fn the_shared_reaper_reaps_a_killed_child() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(1);
    while linux_process_state(pid) != Some('Z') && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        linux_process_state(pid),
        Some('Z'),
        "fixture did not produce a zombie"
    );

    reap_detached(child);
    let deadline = Instant::now() + Duration::from_secs(2);
    while linux_process_state(pid) == Some('Z') && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(
        linux_process_state(pid),
        Some('Z'),
        "the shared reaper did not reap the killed child {pid}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn undrained_reader_does_not_retain_the_nonblocking_writer() {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    let mut pipe_fds = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let reader = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
    let pipe = std::fs::read_link(format!("/proc/self/fd/{}", reader.as_raw_fd())).unwrap();
    let started = Instant::now();

    assert!(
        !write_all_before(
            ChildStdin::from(writer),
            &vec![b'x'; 1024 * 1024],
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap()
    );
    let retained_writers = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| target == &pipe)
        .count();

    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        retained_writers, 1,
        "the undrained pipe retained a writer after the deadline"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn expired_write_deadline_prevents_further_progress() {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    let mut pipe_fds = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let _reader = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };

    assert!(
        !write_all_before(ChildStdin::from(writer), b"x", Instant::now()).unwrap(),
        "an expired child deadline still allowed stdin progress"
    );
}

#[test]
fn expired_cleanup_deadline_hands_reaping_off_without_blocking() {
    let child = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id() as i32;
    let started = Instant::now();
    terminate_and_reap_before(child, pid, Instant::now());

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "expired cleanup deadline blocked the caller"
    );
    let reap_deadline = Instant::now() + Duration::from_secs(1);
    while crate::host_lock::process_alive(pid) && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !crate::host_lock::process_alive(pid),
        "background reaper did not collect child {pid}"
    );
}

#[test]
fn build_run_command_passes_direct_argv_without_a_shell() {
    let cli = PtyCli::new(PathBuf::from("/my/catalog"));
    let mut t = target("hetz.demo.agent", "unused");
    t.launch = TaskLaunch::Argv(vec![
        "axe".into(),
        "agent".into(),
        "exec".into(),
        "--".into(),
        "claude".into(),
        "--resume".into(),
        "$CATALOG/session id".into(),
    ]);
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
    let args = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let sep = args.iter().position(|arg| arg == "--").unwrap();

    assert_eq!(
        &args[sep + 1..],
        [
            "axe",
            "agent",
            "exec",
            "--",
            "claude",
            "--resume",
            "/my/catalog/session id"
        ]
    );
    assert!(!args[sep + 1..].iter().any(|arg| arg == "sh"));
}

#[test]
fn build_run_command_expands_direct_argv_with_the_managed_agent_environment() {
    let cli = PtyCli::new(PathBuf::from("/eval/catalog"));
    let mut t = target("local.worker", "unused");
    t.env.insert("ST_AGENT".into(), "local.worker".into());
    t.env.insert("ST_ROOT".into(), "/eval/catalog".into());
    t.launch = TaskLaunch::Argv(vec![
        "claude".into(),
        "$ST_AGENT reads $ST_ROOT and $CATALOG".into(),
    ]);

    let cmd = cli.build_run_command(&t, Path::new("/eval/catalog/local/worker"));
    let args = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let separator = args.iter().position(|arg| arg == "--").unwrap();

    assert_eq!(
        &args[separator + 1..],
        [
            "claude",
            "local.worker reads /eval/catalog and /eval/catalog"
        ]
    );
}

#[test]
fn build_run_command_persists_the_complete_managed_environment_before_the_command() {
    let cli = PtyCli::new(PathBuf::from("/my/catalog"));
    let mut t = target("hetz.demo.agent", "exec codex 'boot'");
    t.env.insert("CUSTOM".into(), "task-value".into());
    t.env.insert("ST_AGENT".into(), "hetz.demo".into());
    t.env.insert("ST_ROOT".into(), "$CATALOG/custom-bus".into());
    t.env.insert("TERM".into(), "screen-256color".into());
    t.env
        .insert("PTY_ROOT".into(), "/declared/root/must-not-win".into());
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let separator = args.iter().position(|arg| arg == "--").unwrap();
    let mut persisted = BTreeMap::new();
    let mut index = 0;
    while index < separator {
        if args[index] == "--env" {
            let (key, value) = args[index + 1].split_once('=').unwrap();
            assert!(
                persisted
                    .insert(key.to_string(), value.to_string())
                    .is_none(),
                "the final managed overlay needs only one persisted value per key"
            );
            index += 2;
        } else {
            index += 1;
        }
    }
    let inherited = cmd
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        persisted, inherited,
        "initial process env and restart-persisted env must be the same resolved overlay"
    );
    assert_eq!(
        persisted.get("CATALOG").map(String::as_str),
        Some("/my/catalog")
    );
    assert_eq!(
        persisted.get("ST_ROOT").map(String::as_str),
        Some("/my/catalog/custom-bus")
    );
    assert_eq!(
        persisted.get("PTY_ROOT").map(String::as_str),
        Some(
            effective_pty_root(&cli.catalog_root)
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(
        persisted.get("TERM").map(String::as_str),
        Some("screen-256color")
    );
    assert_eq!(
        persisted.get("ST_AGENT").map(String::as_str),
        Some("hetz.demo")
    );
    assert_eq!(
        persisted.get("CUSTOM").map(String::as_str),
        Some("task-value")
    );
    assert!(persisted.contains_key("ST_HOOKS"));
}

#[test]
fn build_run_command_omits_an_alias_equal_to_the_lifecycle_id() {
    let cli = PtyCli::default();
    let mut t = target("hetz.demo", "exec codex 'boot'");
    t.bus_id = t.pty_id.clone();
    t.presentation = Some(PtyPresentation {
        pty_id: t.pty_id.clone(),
        display_name: Some(Some(t.pty_id.clone())),
        tags: BTreeMap::new(),
    });
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        args.iter()
            .position(|arg| arg == "--id")
            .map(|position| args[position + 1].as_str()),
        Some("hetz.demo")
    );
    assert!(
        !args.iter().any(|arg| arg == "--name"),
        "pty rejects a display name equal to the stable session id"
    );
    assert!(
        args.iter().any(|arg| arg == "--no-display-name"),
        "without this flag pty would create an unrelated automatic alias"
    );
}

#[test]
fn build_run_command_defaults_cwd_to_spec_dir_and_passes_tags_and_env() {
    let cli = PtyCli::default();
    let mut t = target("hetz.demo.agent", "exec claude 'boot'");
    t.tags.insert("role".into(), "agent".into());
    t.env.insert("ST_AGENT".into(), "hetz.demo-claude".into());
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cwd_pos = args.iter().position(|a| a == "--cwd").unwrap();
    assert_eq!(args[cwd_pos + 1], "/cat/hetz/demo"); // no cwd, no workspace → spec dir
    let tag_pos = args.iter().position(|a| a == "--tag").unwrap();
    assert_eq!(args[tag_pos + 1], "role=agent");

    // env injected onto the child process
    let envs: BTreeMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(
        envs.get("ST_AGENT"),
        Some(&Some("hetz.demo-claude".to_string()))
    );
    assert_eq!(
        envs.get("TERM"),
        Some(&Some("xterm-256color".to_string())),
        "headless st2 launches must not pass TERM=dumb into an interactive harness"
    );
    assert!(
        envs.get("ST_HOOKS")
            .and_then(Option::as_deref)
            .is_some_and(|path| !path.contains("/sets/sha256-")),
        "managed tasks keep ST_HOOKS at the receipt-bearing root; only rendered hook commands use a versioned set"
    );
}

#[test]
fn managed_agent_scrubs_ambient_no_color_unless_explicitly_declared() {
    let cli = PtyCli::default();
    let agent = target("hetz.demo.agent", "exec claude 'boot'");
    let command = cli.build_run_command(&agent, Path::new("/cat/hetz/demo"));
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("NO_COLOR"))
            .map(|(_, value)| value),
        Some(None),
        "ambient NO_COLOR must not silently disable an interactive agent's color"
    );
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--unset-env", "NO_COLOR"]),
        "the removal must be persisted for PTY restart"
    );

    let mut explicit = target("hetz.explicit.agent", "exec claude 'boot'");
    explicit.env.insert("NO_COLOR".into(), "1".into());
    let command = cli.build_run_command(&explicit, Path::new("/cat/hetz/explicit"));
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("NO_COLOR"))
            .and_then(|(_, value)| value),
        Some(OsStr::new("1")),
        "an explicit Agent Spec NO_COLOR remains authoritative"
    );
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        !args
            .windows(2)
            .any(|pair| pair == ["--unset-env", "NO_COLOR"]),
        "an explicit assignment must not also persist a removal"
    );
}

#[test]
fn non_agent_task_does_not_claim_no_color_policy() {
    let cli = PtyCli::default();
    let mut task = target("hetz.demo.sidecar", "exec sleep 1");
    task.name = "sidecar".into();
    let command = cli.build_run_command(&task, Path::new("/cat/hetz/demo"));

    assert!(
        command
            .get_envs()
            .all(|(key, _)| key != OsStr::new("NO_COLOR")),
        "non-agent services keep the caller's ambient NO_COLOR semantics"
    );
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        !args
            .windows(2)
            .any(|pair| pair == ["--unset-env", "NO_COLOR"]),
        "non-agent services must not persist an st2-owned removal"
    );
}

#[test]
fn isolation_wrapper_preserves_environment_removals() {
    let mut inner = Command::new("pty");
    inner.env("TERM", "xterm-256color").env_remove("NO_COLOR");
    let mut outer = Command::new("systemd-run");

    apply_command_env(&inner, &mut outer);

    let env = outer
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        env.get(OsStr::new("TERM")).and_then(Option::as_deref),
        Some(OsStr::new("xterm-256color"))
    );
    assert_eq!(env.get(OsStr::new("NO_COLOR")), Some(&None));
}

#[test]
fn build_run_command_allows_a_task_to_override_the_default_term() {
    let cli = PtyCli::default();
    let mut t = target("hetz.demo.agent", "exec codex 'boot'");
    t.env.insert("TERM".into(), "screen-256color".into());
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
    let term = cmd
        .get_envs()
        .find(|(key, _)| *key == OsStr::new("TERM"))
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned());
    assert_eq!(term.as_deref(), Some("screen-256color"));
}

#[test]
fn build_run_command_defaults_cwd_to_workspace_when_task_cwd_absent() {
    let cli = PtyCli::default();
    let mut t = target("hetz.demo.agent", "exec claude 'boot'");
    t.workspace = Some("/repos/demo".into()); // no task cwd → workspace (spec.md §2)
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cwd_pos = args.iter().position(|a| a == "--cwd").unwrap();
    assert_eq!(args[cwd_pos + 1], "/repos/demo");
}

#[test]
fn build_run_command_expands_catalog_var_and_sets_it_in_env() {
    let cli = PtyCli::new(PathBuf::from("/my/catalog"));
    let mut t = target("hetz.demo.agent", "run");
    t.env.insert("DATA".into(), "$CATALOG/evals/x".into());
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
    let envs: BTreeMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(
        envs.get("DATA"),
        Some(&Some("/my/catalog/evals/x".to_string()))
    );
    assert_eq!(envs.get("CATALOG"), Some(&Some("/my/catalog".to_string())));
}

#[test]
fn build_run_command_expands_vars_in_env_cwd_and_tags_but_not_command() {
    // Unique var name so the process-global set_var can't collide with a parallel test.
    let key = "ST2_TEST_EXPAND_NET_9f3";
    unsafe { std::env::set_var(key, "/net/xyz") }

    let cli = PtyCli::default();
    let mut t = target("hetz.demo.agent", "exec claude $ST2_TEST_EXPAND_NET_9f3/go");
    t.cwd = Some(format!("${key}/work"));
    t.tags.insert("net".into(), format!("${key}"));
    t.env.insert("ST_ROOT".into(), format!("${key}/custom-bus"));
    let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // cwd expanded (absolute → replaces the spec dir)
    let cwd_pos = args.iter().position(|a| a == "--cwd").unwrap();
    assert_eq!(args[cwd_pos + 1], "/net/xyz/work");
    // tag value expanded
    let tag_pos = args.iter().position(|a| a == "--tag").unwrap();
    assert_eq!(args[tag_pos + 1], "net=/net/xyz");
    // command left verbatim for sh -c to expand at spawn
    assert_eq!(
        args.last().unwrap(),
        "exec claude $ST2_TEST_EXPAND_NET_9f3/go"
    );

    // env value expanded
    let envs: std::collections::BTreeMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(
        envs.get("ST_ROOT"),
        Some(&Some("/net/xyz/custom-bus".to_string()))
    );

    unsafe { std::env::remove_var(key) }
}

#[test]
fn resolve_cwd_honors_relative_absolute_workspace_and_default() {
    let cli = PtyCli::default();
    let mut t = target("x", "y");
    // relative cwd → joined onto the spec dir
    t.cwd = Some("sub".into());
    assert_eq!(
        cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
        Path::new("/cat/hetz/demo/sub")
    );
    // absolute cwd → replaces
    t.cwd = Some("/repos/fabric".into());
    assert_eq!(
        cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
        Path::new("/repos/fabric")
    );
    // no cwd but a workspace → workspace
    t.cwd = None;
    t.workspace = Some("/repos/ws".into());
    assert_eq!(
        cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
        Path::new("/repos/ws")
    );
    // neither → spec dir
    t.workspace = None;
    assert_eq!(
        cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
        Path::new("/cat/hetz/demo")
    );
}

#[test]
fn detect_host_returns_a_nonempty_short_name() {
    let h = detect_host();
    assert!(!h.is_empty());
    assert!(!h.contains('.'), "short name only, got {h}");
}

#[test]
fn task_observation_of_missing_pty_root_is_complete_and_does_not_create_it() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    std::fs::create_dir(&catalog).unwrap();
    std::fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            tmp.path().join("missing-pty").display().to_string()
        ),
    )
    .unwrap();
    let root = effective_pty_root_from(&catalog, None);
    assert!(!root.exists());
    let batch = PtyCli::new(catalog).task_observations(&HashSet::from(["h.worker"]));
    assert!(batch.complete, "{:?}", batch.errors);
    assert!(batch.observations.is_empty());
    assert!(!root.exists(), "read-only observation created the PTY root");
}

#[test]
fn unreadable_pty_root_evidence_is_indeterminate_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let loop_path = tmp.path().join("pty-loop");
    std::fs::create_dir(&catalog).unwrap();
    std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
    std::fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            loop_path.display().to_string()
        ),
    )
    .unwrap();
    let batch = PtyCli::new(catalog)
        .task_observations_at_root(&HashSet::from(["h.worker"]), &loop_path);
    assert!(!batch.complete);
    assert!(batch.observations.is_empty());
    assert!(
        batch.errors[0].contains("cannot inspect PTY root"),
        "{:?}",
        batch.errors
    );
}

#[test]
fn removed_and_recreated_pty_root_is_indeterminate_not_absent() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("pty");
    std::fs::create_dir(&root).unwrap();
    let fake = tmp.path().join("pty-bin");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
rmdir "$PTY_ROOT"
mkdir "$PTY_ROOT"
printf '%s\n' '[]'
printf '' > "$0.ready.tmp"
mv "$0.ready.tmp" "$0.ready"
while [ ! -e "$0.release" ]; do sleep 0.01; done
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake, permissions).unwrap();

    reset_fixture_barrier(&fake);
    let observed_fake = fake.clone();
    let batch = PtyCli {
        bin: fake.display().to_string(),
        catalog_root: tmp.path().join("catalog"),
        on_command_spawn: Some(std::sync::Arc::new(move |pid| {
            release_ready_fixture(
                pid,
                &observed_fake,
                "fake PTY inventory was not published",
            );
        })),
    }
    .task_observations_at_root(&HashSet::from(["h.worker"]), &root);
    assert!(!batch.complete);
    assert!(batch.observations.is_empty());
    assert!(
        batch.errors[0].contains("changed identity during observation"),
        "{:?}",
        batch.errors
    );
}

#[test]
fn pty_stats_rejects_pid_reuse_between_registry_snapshot_and_start_token_capture() {
    let initial = PtyListEntry {
        name: "h.worker".into(),
        status: "running".into(),
        exit_code: None,
        pid: Some(41),
        created_at: Some("2026-09-05T10:00:00.000Z".into()),
        display_name: None,
        tags: BTreeMap::new(),
    };
    let stats = |alive: bool, created_at: &str| PtyStatsEntry {
        name: "h.worker".into(),
        process: Some(PtyStatsProcess { alive }),
        daemon: Some(PtyStatsDaemon { pid: 41 }),
        created_at: Some(created_at.into()),
    };

    // PID 41 has been reused. A token captured after the registry snapshot
    // would describe the replacement, but its live socket reports a new
    // creation generation and prevents that token from being admitted.
    let replacement = stats(true, "2026-09-05T10:00:01.000Z");
    assert_eq!(
        confirm_pty_generation(&initial, &[replacement]),
        Err(ResourceTargetUnavailableReason::GenerationChanged)
    );

    let exited = stats(false, "2026-09-05T10:00:00.000Z");
    assert_eq!(
        confirm_pty_generation(&initial, &[exited]),
        Err(ResourceTargetUnavailableReason::ProcessUnavailable)
    );
    let stable = stats(true, "2026-09-05T10:00:00.000Z");
    assert_eq!(confirm_pty_generation(&initial, &[stable]), Ok(()));
}

#[test]
fn pty_resource_observation_uses_one_stats_snapshot_and_preserves_generation_id() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("pty");
    std::fs::create_dir(&root).unwrap();
    let invocations = tmp.path().join("invocations");
    let failed_once = tmp.path().join("stats-failed-once");
    let pid = std::process::id();
    let fake = tmp.path().join("pty-bin");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {invocations:?}
case "$1" in
  list)
    printf '%s\n' '[{{"name":"h.a","status":"running","pid":{pid},"createdAt":"2026-09-05T10:00:00.000Z"}},{{"name":"h.b","status":"running","pid":{pid},"createdAt":"2026-09-05T10:00:01.000Z"}}]'
    ;;
  stats)
    if [ ! -e {failed_once:?} ]; then
      : > {failed_once:?}
      exit 1
    fi
    printf '%s\n' '[{{"name":"h.a","process":{{"alive":true}},"daemon":{{"pid":{pid}}},"createdAt":"2026-09-05T10:00:00.000Z"}},{{"name":"h.b","process":{{"alive":true}},"daemon":{{"pid":{pid}}},"createdAt":"2026-09-05T10:00:01.000Z"}}]'
    ;;
esac
"#,
            invocations = invocations,
            failed_once = failed_once,
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake, permissions).unwrap();

    let cli = PtyCli {
        bin: fake.display().to_string(),
        catalog_root: tmp.path().join("catalog"),
        on_command_spawn: None,
    };
    let desired_ids = HashSet::from(["h.a", "h.b"]);
    let unavailable = cli.task_observations_at_root(&desired_ids, &root);
    let available = cli.task_observations_at_root(&desired_ids, &root);
    assert!(unavailable.complete, "{:?}", unavailable.errors);
    assert!(available.complete, "{:?}", available.errors);
    assert_eq!(available.observations.len(), 2);
    assert_eq!(
        std::fs::read_to_string(invocations).unwrap(),
        "list --json\nstats --json\nlist --json\nstats --json\n"
    );
    for (before, after) in unavailable
        .observations
        .iter()
        .zip(&available.observations)
    {
        let (ObservedState::Running(before), ObservedState::Running(after)) =
            (&before.state, &after.state)
        else {
            panic!("live PTY lost generation");
        };
        assert!(matches!(
            before.resource_target(),
            ResourceTarget::Unavailable { .. }
        ));
        assert!(!matches!(
            after.resource_target(),
            ResourceTarget::Unavailable { .. }
        ));
        assert_eq!(
            before.generation_id(),
            after.generation_id(),
            "transient target proof changed stable PTY generation identity"
        );
    }
}

#[test]
fn pty_task_observation_preserves_exact_generation_and_closed_states() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let pty_root = tmp.path().join("pty");
    std::fs::create_dir_all(&catalog).unwrap();
    std::fs::create_dir(&pty_root).unwrap();
    std::fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            pty_root.display().to_string()
        ),
    )
    .unwrap();
    let fake = tmp.path().join("pty-bin");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
printf '%s\n' '[{"name":"h.live","status":"running","pid":41,"createdAt":"2026-07-31T10:00:00.000Z","displayName":"Build owner","tags":{"agent.presentation.schema":"1","unrelated":"preserved"}},{"name":"h.exit","status":"exited","exitCode":0,"pid":42,"createdAt":"2026-07-31T09:00:00.000Z"},{"name":"h.gone","status":"vanished","pid":43,"createdAt":"2026-07-31T08:00:00.000Z"}]'
printf '' > "$0.ready.tmp"
mv "$0.ready.tmp" "$0.ready"
while [ ! -e "$0.release" ]; do sleep 0.01; done
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake, permissions).unwrap();

    let observed_fake = fake.clone();
    let cli = PtyCli {
        bin: fake.display().to_string(),
        catalog_root: catalog,
        on_command_spawn: Some(std::sync::Arc::new(move |pid| {
            release_ready_fixture(
                pid,
                &observed_fake,
                "fake PTY inventory was not published",
            );
        })),
    };
    let desired = HashSet::from(["h.live", "h.exit", "h.gone"]);
    reset_fixture_barrier(&fake);
    let first = cli.task_observations(&desired);
    reset_fixture_barrier(&fake);
    let second = cli.task_observations(&desired);
    assert!(first.complete, "{:?}", first.errors);
    assert_eq!(first, second, "same PTY evidence changed generation");
    let ObservedState::Running(generation) = &first.observations[0].state else {
        panic!("running PTY lost generation: {:?}", first.observations[0]);
    };
    assert_eq!(generation.pid(), 41);
    assert_eq!(generation.created_at(), "2026-07-31T10:00:00.000Z");
    assert!(generation.generation_id().starts_with("sha256:"));
    assert_eq!(first.observations[1].state, ObservedState::Exited);
    assert_eq!(first.observations[2].state, ObservedState::Vanished);

    reset_fixture_barrier(&fake);
    let sessions = cli.list_sessions().unwrap();
    let presentation = sessions[0].presentation.as_ref().unwrap();
    assert_eq!(presentation.display_name.as_deref(), Some("Build owner"));
    assert_eq!(
        presentation
            .tags
            .get("agent.presentation.schema")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        presentation.tags.get("unrelated").map(String::as_str),
        Some("preserved")
    );
}

#[test]
fn running_pty_without_complete_generation_is_indeterminate() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let pty_root = tmp.path().join("pty");
    std::fs::create_dir_all(&catalog).unwrap();
    std::fs::create_dir(&pty_root).unwrap();
    std::fs::write(
        catalog.join("catalog.kdl"),
        format!(
            "catalog {{ pty-root {:?} }}\n",
            pty_root.display().to_string()
        ),
    )
    .unwrap();
    let fake = tmp.path().join("pty-bin");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
printf '%s\n' '[{"name":"h.live","status":"running"}]'
printf '' > "$0.ready.tmp"
mv "$0.ready.tmp" "$0.ready"
while [ ! -e "$0.release" ]; do sleep 0.01; done
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake, permissions).unwrap();
    reset_fixture_barrier(&fake);
    let observed_fake = fake.clone();
    let batch = PtyCli {
        bin: fake.display().to_string(),
        catalog_root: catalog,
        on_command_spawn: Some(std::sync::Arc::new(move |pid| {
            release_ready_fixture(
                pid,
                &observed_fake,
                "fake PTY inventory was not published",
            );
        })),
    }
    .task_observations(&HashSet::from(["h.live"]));
    assert!(!batch.complete);
    assert!(matches!(
        batch.observations[0].state,
        ObservedState::Indeterminate(_)
    ));
}
