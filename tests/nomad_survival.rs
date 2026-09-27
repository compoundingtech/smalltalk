//! The supervisor-decoupling gate.
//!
//! st2 is a *decoupled* supervisor: stopping or crashing the runner MUST leave its running tasks
//! alive, and a fresh runner MUST ADOPT them rather than cold-booting. The ONLY thing that kills a
//! task is an explicit teardown (a `retired` spec). This guarantee is proven with real processes —
//! installing the real `st2` binary, stopping it with SIGTERM *and* SIGKILL, atomically replacing
//! that binary, and asserting the task pids and creation identities survive and get re-adopted
//! without duplicate boots — not with a fake runner.
//!
//! Two spawn paths, both covered:
//!   - `exec` — st2 supervises the process directly and detaches it with `setsid`, so it survives
//!     st2's death (this is st2's OWN code providing the property).
//!   - `pty`  — st2 shells out to the `pty` daemon, which owns the session independently of st2.
//!
//! Isolation: every fixture points `$XDG_STATE_HOME` (exec pids) and `$PTY_ROOT` (pty sessions) at its
//! own temp dirs and uses a throwaway host, so these tests can NEVER touch the live fleet's sessions.
//! Tasks are `sleep 120` — long enough for the test, short enough to self-reap if cleanup is ever
//! skipped; the fixture's `Drop` also force-kills anything it spawned, even on a panic.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use st2::host_lock::process_alive;

const HOST: &str = "nomadtest";
// `exec` so `sh -c` replaces itself with sleep (dash otherwise FORKS a bare command, leaving the real
// workload as a grandchild the recorded pid isn't) — so the recorded pid IS the workload and the
// survival/teardown assertions are about the real task, not a shell wrapper.
const TASK_CMD: &str = "exec sleep 120";
const SPAWN_TIMEOUT: Duration = Duration::from_secs(15);
const DEATH_TIMEOUT: Duration = Duration::from_secs(10);

// ── fixture: an env-isolated catalog + a handle to the real st2 binary ───────────────────────────

struct Fixture {
    catalog: PathBuf,
    xdg: PathBuf,
    pty_root: PathBuf,
    /// pty session ids this fixture may have created — force-killed on drop.
    pty_sessions: std::cell::RefCell<Vec<String>>,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (catalog, xdg, pty_root) =
            (root.join("catalog"), root.join("xdg"), root.join("ptyroot"));
        for d in [&catalog, &xdg, &pty_root] {
            std::fs::create_dir_all(d).unwrap();
        }
        Fixture {
            catalog,
            xdg,
            pty_root,
            pty_sessions: Default::default(),
            _tmp: tmp,
        }
    }

    /// An `st2` invocation with this fixture's isolated state env.
    fn st2(&self) -> Command {
        self.st2_from(Path::new(env!("CARGO_BIN_EXE_st2")))
    }

    fn st2_from(&self, binary: &Path) -> Command {
        let mut c = Command::new(binary);
        c.env("XDG_STATE_HOME", &self.xdg)
            .env("PTY_ROOT", &self.pty_root)
            .env("ST_HOOKS", self.xdg.join("st2/hooks"))
            .env("ST2_TEST_AMBIENT_ONLY", "initial-launch-only");
        c
    }

    /// Write a one-task service agent (`kind` = "exec" | "pty"), optionally retired.
    fn write_agent(&self, identity: &str, kind: &str, retired: bool) {
        if kind == "pty" {
            self.pty_sessions
                .borrow_mut()
                .push(format!("{HOST}.{identity}.task"));
        }
        let retired_line = if retired { "retired #true" } else { "" };
        let kdl = format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{HOST}\"\n  \
             type \"service\"\n  {retired_line}\n  {kind} \"task\" {{ command \"{TASK_CMD}\" }}\n}}\n"
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
    }

    /// Write an agent with an external boot receipt and heartbeat. The receipt proves a replacement
    /// control plane did not duplicate the task; a growing heartbeat proves the original task remains
    /// usable while no control plane is running and after adoption.
    fn write_replacement_agent(&self, identity: &str, kind: &str) -> (PathBuf, PathBuf) {
        if kind == "pty" {
            self.pty_sessions
                .borrow_mut()
                .push(format!("{HOST}.{identity}.task"));
        }
        let boots = self.xdg.join(format!("{identity}.boots"));
        let heartbeat = self.xdg.join(format!("{identity}.heartbeat"));
        let kdl = format!(
            r##"agent "{identity}" {{
  identity "{identity}"
  host "{HOST}"
  type "service"
  {kind} "task" {{
    command #"printf 'boot\n' >> "$XDG_STATE_HOME/{identity}.boots"; while :; do printf . >> "$XDG_STATE_HOME/{identity}.heartbeat"; sleep 0.05; done"#
  }}
}}
"##
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
        (boots, heartbeat)
    }

    /// Write a compact canonical agent whose primary pty id is exactly its bus identity.
    fn write_compact_agent(&self, identity: &str) {
        self.pty_sessions
            .borrow_mut()
            .push(format!("{HOST}.{identity}"));
        let kdl = format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{HOST}\"\n  \
             type \"service\"\n  command \"{TASK_CMD}\"\n}}\n"
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
    }

    fn write_color_env_agent(&self, identity: &str, explicit_no_color: bool) -> PathBuf {
        self.pty_sessions
            .borrow_mut()
            .push(format!("{HOST}.{identity}"));
        let snapshot = self.catalog.join(format!("{identity}.color-env"));
        let explicit_env = if explicit_no_color {
            "  env { NO_COLOR \"1\" }\n"
        } else {
            Default::default()
        };
        let kdl = format!(
            r##"agent "{identity}" {{
  identity "{identity}"
  host "{HOST}"
  type "service"
{explicit_env}  command #"(printenv NO_COLOR || printf 'unset\n') >> "$CATALOG/{identity}.color-env""#
}}
"##
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
        snapshot
    }

    fn write_presented_compact_agent(&self, identity: &str, name: &str, description: &str) {
        self.pty_sessions
            .borrow_mut()
            .push(format!("{HOST}.{identity}"));
        let kdl = format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{HOST}\"\n  \
             type \"service\"\n  name {name:?}\n  description {description:?}\n  command \"{TASK_CMD}\"\n}}\n"
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
    }

    /// Write a PTY task that records every st2-managed environment value and its cwd on each boot.
    /// The append-only snapshot lets a test compare the initial launch with a manual `pty restart`.
    fn write_restart_env_agent(&self, identity: &str) -> PathBuf {
        self.pty_sessions
            .borrow_mut()
            .push(format!("{HOST}.{identity}.task"));
        let workspace = self.catalog.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let snapshot = self.catalog.join("restart-env.txt");
        let kdl = format!(
            r##"agent "{identity}" {{
  identity "{identity}"
  host "{HOST}"
  type "service"
  workspace "$CATALOG/workspace"
  supervisor "{HOST}.supervisor"
  env {{
    ST_AGENT "{HOST}.{identity}"
    ST_ROOT "$CATALOG/custom-bus"
    TERM "screen-256color"
    PTY_ROOT "$CATALOG/declared-root-must-not-win"
    CUSTOM "task-value"
  }}
  pty "task" {{
    tags purpose="restart-env"
    command #"printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$CATALOG" "$ST_ROOT" "$PTY_ROOT" "$TERM" "$ST_HOOKS" "$ST_AGENT" "$ST_SUPERVISOR" "$CUSTOM" "$PWD" "$ST2_TEST_AMBIENT_ONLY" >> "$CATALOG/restart-env.txt"; exec sleep 120"#
  }}
}}
"##
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
        snapshot
    }

    /// The pidfile whose liveness == "the task is running": the exec child's pid, or the pty daemon's
    /// pid (which owns the session). Both survive st2's death when the property holds.
    fn task_pidfile(&self, kind: &str, identity: &str) -> PathBuf {
        match kind {
            "exec" => self
                .xdg
                .join("st2")
                .join(HOST)
                .join("exec")
                .join(format!("{HOST}.{identity}.task.pid")),
            "pty" => self.pty_root.join(format!("{HOST}.{identity}.task.pid")),
            other => panic!("unknown task kind {other}"),
        }
    }

    /// Install the built binary at a deployment-like path owned by this fixture.
    fn install_control_plane(&self) -> PathBuf {
        let bin_dir = self.xdg.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let installed = bin_dir.join("st2");
        let staged = installed.with_extension("installing");
        std::fs::copy(env!("CARGO_BIN_EXE_st2"), &staged).unwrap();
        std::fs::File::open(&staged).unwrap().sync_all().unwrap();
        std::fs::rename(&staged, &installed).unwrap();
        installed
    }

    /// Atomically replace the installed binary and prove the selected file identity changed.
    fn replace_control_plane(&self, installed: &Path) {
        let before = binary_file_identity(installed);
        let staged = installed.with_extension("next");
        std::fs::copy(env!("CARGO_BIN_EXE_st2"), &staged).unwrap();
        std::fs::File::open(&staged).unwrap().sync_all().unwrap();
        std::fs::rename(&staged, installed).unwrap();
        let after = binary_file_identity(installed);
        assert_ne!(
            after, before,
            "the control-plane binary path did not select a replacement file"
        );
    }

    /// Spawn the real `st2 up` supervisor loop (long interval — the immediate first pass does the
    /// spawning; the loop just gives us a live runner to kill).
    fn spawn_loop(&self) -> Child {
        self.spawn_loop_from(Path::new(env!("CARGO_BIN_EXE_st2")))
    }

    fn spawn_loop_from(&self, binary: &Path) -> Child {
        for attempt in 0..5 {
            let result = self
                .st2_from(binary)
                .arg("up")
                .arg(&self.catalog)
                .args(["--host", HOST, "--interval", "60"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match result {
                Ok(child) => return child,
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempt + 1 < 5 => {
                    // Some Linux filesystems briefly retain the writer exclusion after installing
                    // a copied executable. Retry only that transient; every other spawn error stays
                    // loud and immediate.
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("spawning fixture control plane failed: {error}"),
            }
        }
        unreachable!("the bounded spawn loop always returns or panics")
    }

    /// One `st2 up --once` pass; returns its stdout (where launched/adopted/torn-down is reported).
    fn up_once(&self) -> String {
        self.up_once_from(Path::new(env!("CARGO_BIN_EXE_st2")))
    }

    fn up_once_from(&self, binary: &Path) -> String {
        let out = self
            .st2_from(binary)
            .arg("up")
            .arg(&self.catalog)
            .args(["--host", HOST, "--once"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "replacement `st2 up --once` failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best-effort teardown so nothing outlives the test, even on a panic.
        let quiet = |mut c: Command| {
            let _ = c.stdout(Stdio::null()).stderr(Stdio::null()).status();
        };
        let exec_dir = self.xdg.join("st2").join(HOST).join("exec");
        if let Ok(entries) = std::fs::read_dir(&exec_dir) {
            for e in entries.flatten() {
                if e.path().extension().and_then(|x| x.to_str()) == Some("pid")
                    && let Some(pid) = read_pid(&e.path())
                {
                    // Kill the process group (setsid leader) and the pid, so nothing is left behind.
                    for target in [format!("-{pid}"), pid.to_string()] {
                        let mut c = Command::new("kill");
                        c.arg("-KILL").arg(target);
                        quiet(c);
                    }
                }
            }
        }
        for sess in self.pty_sessions.borrow().iter() {
            for verb in ["kill", "rm"] {
                let mut c = Command::new("pty");
                c.env("PTY_ROOT", &self.pty_root).args([verb, sess]);
                quiet(c);
            }
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

/// Owns the spawned `st2` supervisor child so a panicking assertion can never leave it running (a
/// bare `Child`'s drop neither kills nor reaps). On the happy path the test kills+reaps it explicitly;
/// this just backstops failures.
struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill(); // SIGKILL if still alive (no-op once already exited)
        let _ = self.0.wait(); // reap
    }
}

fn read_pid(path: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse().ok().or_else(|| {
        serde_json::from_str::<serde_json::Value>(&raw).ok()?["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
    })
}

fn read_alive(pidfile: &Path) -> bool {
    read_pid(pidfile).is_some_and(process_alive)
}

fn binary_file_identity(path: &Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

#[cfg(target_os = "linux")]
fn process_creation_identity(pid: i32) -> String {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after_name = &stat[stat.rfind(')').unwrap() + 1..];
    let fields = after_name.split_whitespace().collect::<Vec<_>>();
    // `/proc/<pid>/stat` field 22 is the process start time. The first field after `)` is field 3.
    format!("linux-start-ticks:{}", fields[19])
}

#[cfg(not(target_os = "linux"))]
fn process_creation_identity(pid: i32) -> String {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ps could not inspect task pid {pid}"
    );
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !started.is_empty(),
        "ps returned no creation identity for task pid {pid}"
    );
    format!("ps-lstart:{started}")
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

fn signal_pid(pid: i32, sig: &str) {
    let ok = Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .unwrap()
        .success();
    assert!(ok, "failed to send SIG{sig} to pid {pid}");
}

fn poll_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

fn pty_available() -> bool {
    Command::new("pty")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// These tests drive the real `st2` binary, whose SystemRunner lists `pty` sessions every reconcile
/// pass — so `pty` must be on PATH for ANY of them to prove anything (without it every pass is skipped
/// and nothing spawns). Because this is a gate, a missing `pty` is a HARD FAILURE — never a silent
/// skip that reads as green — unless a dev explicitly opts out with `ST2_ALLOW_PTY_SKIP` on a box that
/// has no pty. Returns true to run, false to return early (only via the opt-out). CI/gating MUST
/// install `pty` so these actually run. (The pty-free exec spawn/kill/group-teardown coverage lives in
/// `tests/exec_backend.rs`, which always runs.)
fn pty_gate(test: &str) -> bool {
    if pty_available() {
        return true;
    }
    assert!(
        std::env::var_os("ST2_ALLOW_PTY_SKIP").is_some(),
        "{test}: `pty` is not on PATH, so st2's nomad-decoupling gate cannot run and is UNPROVEN. \
         Install `pty` (required in CI/gating), or set ST2_ALLOW_PTY_SKIP=1 to skip on a dev box \
         without pty."
    );
    eprintln!("SKIP {test}: `pty` not on PATH (ST2_ALLOW_PTY_SKIP set)");
    false
}

#[cfg(target_os = "linux")]
fn scope_gate(test: &str) -> bool {
    let runtime_dir_valid =
        std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|path| Path::new(&path).is_dir());
    if runtime_dir_valid && st2::isolate::mode() == st2::isolate::Isolation::Scope {
        return true;
    }
    assert!(
        std::env::var_os("ST2_ALLOW_ISOLATION_SKIP").is_some(),
        "{test}: a valid XDG_RUNTIME_DIR and systemd user scope are required. Set \
         ST2_ALLOW_ISOLATION_SKIP=1 to skip on a host without them."
    );
    eprintln!("SKIP {test}: systemd user scope unavailable (ST2_ALLOW_ISOLATION_SKIP set)");
    false
}

#[test]
fn pty_sessions_use_unique_agent_identity_display_names_and_preserve_lifecycle() {
    if !pty_gate("pty_sessions_use_unique_agent_identity_display_names_and_preserve_lifecycle") {
        return;
    }
    let fx = Fixture::new();
    fx.write_compact_agent("cos");
    fx.write_agent("st2", "pty", false);

    let launched = fx.up_once();
    assert!(
        launched.contains("nomadtest.cos") && launched.contains("nomadtest.st2.task"),
        "both agent sessions must launch; output:\n{launched}"
    );

    let listed = Command::new("pty")
        .env("PTY_ROOT", &fx.pty_root)
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "`pty list --json` failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let sessions: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let display_name = |id: &str| {
        sessions
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["name"] == id)
            .and_then(|session| session["displayName"].as_str())
            .map(str::to_owned)
    };
    assert_eq!(
        display_name("nomadtest.cos"),
        None,
        "when the lifecycle id is already the identity, pty must fall back to it without an equal alias"
    );
    assert_eq!(
        display_name("nomadtest.st2.task").as_deref(),
        Some("nomadtest.st2")
    );

    let adopted = fx.up_once();
    assert!(
        adopted.contains("adopted (2): cos, st2") && !adopted.contains("launched"),
        "a second runner pass must adopt both named sessions without duplicates; output:\n{adopted}"
    );

    let down = fx
        .st2()
        .arg("down")
        .arg(&fx.catalog)
        .args(["--host", HOST])
        .output()
        .unwrap();
    assert!(
        down.status.success(),
        "`st2 down` failed: {}",
        String::from_utf8_lossy(&down.stderr)
    );
    let down_out = String::from_utf8_lossy(&down.stdout);
    assert!(
        down_out.contains("torn down (2): nomadtest.cos, nomadtest.st2.task"),
        "down must tear down both sessions by lifecycle id; output:\n{down_out}"
    );
    for session_id in ["nomadtest.cos", "nomadtest.st2.task"] {
        let pidfile = fx.pty_root.join(format!("{session_id}.pid"));
        assert!(
            poll_until(DEATH_TIMEOUT, || !read_alive(&pidfile)),
            "down did not stop {session_id}"
        );
    }
}

#[test]
fn managed_agents_do_not_inherit_launcher_no_color_unless_declared() {
    if !pty_gate("managed_agents_do_not_inherit_launcher_no_color_unless_declared") {
        return;
    }
    assert_managed_agent_color_contract("portable");
}

#[cfg(target_os = "linux")]
#[test]
fn managed_agent_color_contract_crosses_systemd_scope() {
    let test = "managed_agent_color_contract_crosses_systemd_scope";
    if !pty_gate(test) || !scope_gate(test) {
        return;
    }
    assert_eq!(
        st2::isolate::mode(),
        st2::isolate::Isolation::Scope,
        "the NO_COLOR integration must exercise systemd-run, not degraded pass-through"
    );
    assert_managed_agent_color_contract("scope");
}

fn assert_managed_agent_color_contract(suffix: &str) {
    let fx = Fixture::new();
    let ambient_identity = format!("ambient-color-{suffix}");
    let explicit_identity = format!("explicit-color-{suffix}");
    let ambient_snapshot = fx.write_color_env_agent(&ambient_identity, false);
    let explicit_snapshot = fx.write_color_env_agent(&explicit_identity, true);

    let out = fx
        .st2()
        .env("NO_COLOR", "1")
        .arg("up")
        .arg(&fx.catalog)
        .args(["--host", HOST, "--once"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "st2 up --once failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(poll_until(SPAWN_TIMEOUT, || {
        ambient_snapshot.exists() && explicit_snapshot.exists()
    }));

    assert_eq!(
        std::fs::read_to_string(&ambient_snapshot).unwrap(),
        "unset\n",
        "the reconciler's capture-only NO_COLOR must not disable agent color"
    );
    assert_eq!(
        std::fs::read_to_string(&explicit_snapshot).unwrap(),
        "1\n",
        "an explicit Agent Spec NO_COLOR must still reach the agent"
    );

    for (identity, snapshot) in [
        (&ambient_identity, &ambient_snapshot),
        (&explicit_identity, &explicit_snapshot),
    ] {
        let mut restarted = Command::new("pty")
            .env("NO_COLOR", "1")
            .env("PTY_ROOT", &fx.pty_root)
            .args(["restart", "-y", "--force", &format!("{HOST}.{identity}")])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let observed = poll_until(SPAWN_TIMEOUT, || {
            std::fs::read_to_string(snapshot).is_ok_and(|contents| contents.lines().count() == 2)
        });
        let _ = restarted.kill();
        let output = restarted.wait_with_output().unwrap();
        assert!(
            observed,
            "`pty restart -y` did not produce a second snapshot for {identity}:\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        poll_until(SPAWN_TIMEOUT, || {
            [&ambient_snapshot, &explicit_snapshot]
                .iter()
                .all(|snapshot| {
                    std::fs::read_to_string(snapshot)
                        .is_ok_and(|contents| contents.lines().count() == 2)
                })
        }),
        "restarted agents did not record their second environment snapshot"
    );
    assert_eq!(
        std::fs::read_to_string(ambient_snapshot).unwrap(),
        "unset\nunset\n",
        "the persisted removal must win over a restart caller's ambient NO_COLOR"
    );
    assert_eq!(
        std::fs::read_to_string(explicit_snapshot).unwrap(),
        "1\n1\n",
        "the explicit Agent Spec assignment must win on restart"
    );
}

#[test]
fn presentation_changes_patch_the_exact_live_pty_without_restarting_it() {
    if !pty_gate("presentation_changes_patch_the_exact_live_pty_without_restarting_it") {
        return;
    }
    let fx = Fixture::new();
    let identity = "presented";
    let session_id = format!("{HOST}.{identity}");
    fx.write_presented_compact_agent(identity, "Build owner", "Owns build delivery");

    let launched = fx.up_once();
    assert!(launched.contains(&session_id), "output:\n{launched}");
    let pidfile = fx.pty_root.join(format!("{session_id}.pid"));
    let initial_pid = read_pid(&pidfile).unwrap();
    let list_session = || {
        let output = Command::new("pty")
            .env("PTY_ROOT", &fx.pty_root)
            .args(["list", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["name"] == session_id)
            .unwrap()
            .clone()
    };
    let event_count = || {
        let output = Command::new("pty")
            .env("PTY_ROOT", &fx.pty_root)
            .args(["events", "--recent", "--json", &session_id])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .matches("metadata_change")
            .count()
    };
    let initial = list_session();
    let created_at = initial["createdAt"].clone();
    assert_eq!(initial["displayName"], "Build owner");
    assert_eq!(initial["tags"]["agent.presentation.schema"], "1");
    assert_eq!(initial["tags"]["agent.actor.path"], session_id);
    assert_eq!(
        initial["tags"]["agent.presentation.description"],
        "Owns build delivery"
    );
    let initial_events = event_count();

    for (command, value) in [
        ("rename", "Release owner"),
        ("describe", "Owns release delivery"),
    ] {
        let output = fx
            .st2()
            .env_remove("ST_AGENT")
            .args([
                "--catalog",
                fx.catalog.to_str().unwrap(),
                command,
                &session_id,
                value,
            ])
            .args(["--host", HOST, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let adopted = fx.up_once();
    assert!(
        adopted.contains("adopted (1): presented"),
        "output:\n{adopted}"
    );
    let changed = list_session();
    assert_eq!(read_pid(&pidfile), Some(initial_pid));
    assert_eq!(changed["createdAt"], created_at);
    assert_eq!(changed["displayName"], "Release owner");
    assert_eq!(
        changed["tags"]["agent.presentation.description"],
        "Owns release delivery"
    );
    assert_eq!(event_count(), initial_events + 1);

    fx.up_once();
    assert_eq!(read_pid(&pidfile), Some(initial_pid));
    assert_eq!(
        event_count(),
        initial_events + 1,
        "unchanged projection emitted an event"
    );

    let output = fx
        .st2()
        .env_remove("ST_AGENT")
        .args([
            "--catalog",
            fx.catalog.to_str().unwrap(),
            "rename",
            &session_id,
            &session_id,
        ])
        .args(["--host", HOST, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fx.up_once();
    let lifecycle_equal = list_session();
    assert_eq!(read_pid(&pidfile), Some(initial_pid));
    assert_eq!(lifecycle_equal["createdAt"], created_at);
    assert!(lifecycle_equal.get("displayName").is_none());
    assert_eq!(event_count(), initial_events + 2);

    fx.up_once();
    assert_eq!(event_count(), initial_events + 2);

    for command in ["rename", "describe"] {
        let output = fx
            .st2()
            .env_remove("ST_AGENT")
            .args([
                "--catalog",
                fx.catalog.to_str().unwrap(),
                command,
                &session_id,
                "--clear",
            ])
            .args(["--host", HOST, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fx.up_once();
    let cleared = list_session();
    assert_eq!(read_pid(&pidfile), Some(initial_pid));
    assert_eq!(cleared["createdAt"], created_at);
    assert!(cleared.get("displayName").is_none());
    assert!(
        cleared["tags"]
            .get("agent.presentation.description")
            .is_none()
    );
    assert_eq!(event_count(), initial_events + 3);

    let declaration = fx.catalog.join(HOST).join(identity).join("agent.kdl");
    let source = std::fs::read_to_string(&declaration).unwrap();
    std::fs::write(
        &declaration,
        source.replace(
            "  type \"service\"\n",
            "  type \"service\"\n  retired #true\n",
        ),
    )
    .unwrap();
    let retired = fx.up_once();
    assert!(
        retired.contains(&format!("torn down (1): {session_id}")),
        "output:\n{retired}"
    );
    assert!(
        poll_until(DEATH_TIMEOUT, || !read_alive(&pidfile)),
        "the genuine retirement lifecycle change did not stop the PTY"
    );
}

#[test]
fn manual_pty_restart_preserves_every_st2_managed_environment_and_config_value() {
    if !pty_gate("manual_pty_restart_preserves_every_st2_managed_environment_and_config_value") {
        return;
    }
    let fx = Fixture::new();
    let identity = "envrestart";
    let session_id = format!("{HOST}.{identity}.task");
    let snapshot = fx.write_restart_env_agent(identity);

    let launched = fx.up_once();
    assert!(
        launched.contains(&format!("launched (1): {session_id}")),
        "st2 did not launch the restart fixture:\n{launched}"
    );
    assert!(
        poll_until(SPAWN_TIMEOUT, || {
            std::fs::read_to_string(&snapshot).is_ok_and(|contents| contents.lines().count() == 1)
        }),
        "initial PTY task never wrote its managed environment snapshot"
    );
    let initial_pid = read_pid(&fx.task_pidfile("pty", identity)).unwrap();

    let restarted = Command::new("pty")
        .env_remove("ST2_TEST_AMBIENT_ONLY")
        .env("PTY_ROOT", &fx.pty_root)
        .args(["restart", "-y", &session_id])
        .output()
        .unwrap();
    assert!(
        restarted.status.success(),
        "`pty restart -y` failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&restarted.stdout),
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert!(
        poll_until(SPAWN_TIMEOUT, || {
            std::fs::read_to_string(&snapshot).is_ok_and(|contents| contents.lines().count() >= 2)
        }),
        "manually restarted PTY task never wrote its second environment snapshot"
    );
    let restarted_pid = read_pid(&fx.task_pidfile("pty", identity)).unwrap();
    assert_ne!(
        restarted_pid, initial_pid,
        "manual restart did not replace the session process"
    );
    assert!(process_alive(restarted_pid));

    let snapshots = std::fs::read_to_string(&snapshot).unwrap();
    let lines = snapshots.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "unexpected task boot count:\n{snapshots}");
    let catalog = fx.catalog.canonicalize().unwrap();
    let expected_managed = [
        catalog.to_string_lossy().into_owned(),
        catalog.join("custom-bus").to_string_lossy().into_owned(),
        fx.pty_root.to_string_lossy().into_owned(),
        "screen-256color".to_string(),
        fx.xdg.join("st2/hooks").to_string_lossy().into_owned(),
        format!("{HOST}.{identity}"),
        format!("{HOST}.supervisor"),
        "task-value".to_string(),
        catalog.join("workspace").to_string_lossy().into_owned(),
    ];
    let initial_expected = expected_managed
        .iter()
        .cloned()
        .chain(["initial-launch-only".to_string()])
        .collect::<Vec<_>>()
        .join("|");
    let restarted_expected = expected_managed
        .iter()
        .cloned()
        .chain([String::new()])
        .collect::<Vec<_>>()
        .join("|");
    assert_eq!(
        lines[0], initial_expected,
        "initial launch must inherit ambient input in addition to the managed overlay"
    );
    assert_eq!(
        lines[1], restarted_expected,
        "manual PTY restart must restore the managed overlay without snapshotting ambient input"
    );

    let listed = Command::new("pty")
        .env("PTY_ROOT", &fx.pty_root)
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let sessions: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let session = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == session_id)
        .unwrap();
    let expected_display_name = format!("{HOST}.{identity}");
    let expected_cwd = catalog.join("workspace").to_string_lossy().into_owned();
    assert_eq!(
        session["displayName"].as_str(),
        Some(expected_display_name.as_str()),
        "manual restart lost the st2 agent display name"
    );
    assert_eq!(
        session["cwd"].as_str(),
        Some(expected_cwd.as_str()),
        "manual restart lost the resolved task cwd"
    );
    assert_eq!(
        session["tags"]["purpose"].as_str(),
        Some("restart-env"),
        "manual restart lost the task tag"
    );
}

// ── R11: replace the control plane while the unchanged task stays usable and is adopted ──────────

fn control_plane_replacement_preserves_agent(kind: &str, signal: &str) {
    // The whole scenario runs the real `st2` binary, whose SystemRunner lists `pty` sessions every
    // reconcile pass — so pty must be present even for an `exec` task, or every pass is skipped and
    // nothing spawns. Gate accordingly (fail-loud without pty; see `pty_gate`).
    if !pty_gate(&format!(
        "control_plane_replacement_preserves_agent({kind}/{signal})"
    )) {
        return;
    }
    let fx = Fixture::new();
    let (boots, heartbeat) = fx.write_replacement_agent("survivor", kind);
    let installed = fx.install_control_plane();

    // 1) The installed control plane spawns one usable task.
    let mut runner = Runner(fx.spawn_loop_from(&installed));
    let pidfile = fx.task_pidfile(kind, "survivor");
    assert!(
        poll_until(SPAWN_TIMEOUT, || {
            read_alive(&pidfile) && line_count(&boots) == 1 && file_len(&heartbeat) > 0
        }),
        "{kind}/{signal}: runner never brought up one usable task (pidfile {})",
        pidfile.display()
    );
    let task_pid = read_pid(&pidfile).unwrap();
    let task_creation = process_creation_identity(task_pid);

    // 2) Stop or force-kill the control plane and prove the original task keeps doing work.
    let runner_pid = runner.0.id() as i32;
    signal_pid(runner_pid, signal);
    runner.0.wait().unwrap();
    assert!(
        !process_alive(runner_pid),
        "{kind}/{signal}: runner did not actually die"
    );
    assert!(
        process_alive(task_pid),
        "{kind}/{signal}: task pid {task_pid} died with the control plane"
    );
    let heartbeat_without_control_plane = file_len(&heartbeat);
    assert!(
        poll_until(Duration::from_secs(2), || {
            file_len(&heartbeat) > heartbeat_without_control_plane
        }),
        "{kind}/{signal}: surviving task stopped being usable with no control plane"
    );

    // 3) Reinstall st2 atomically while the task is live, and declare one genuinely missing task.
    fx.replace_control_plane(&installed);
    fx.write_agent("newcomer", kind, false);

    // 4) The replacement adopts the survivor unchanged and starts only the genuinely missing work.
    let heartbeat_before_adoption = file_len(&heartbeat);
    let out = fx.up_once_from(&installed);
    let newcomer_id = format!("{HOST}.newcomer.task");
    assert!(
        out.contains("adopted (1): survivor"),
        "{kind}/{signal}: replacement did not adopt the surviving task; output:\n{out}"
    );
    assert!(
        out.contains(&format!("launched (1): {newcomer_id}")),
        "{kind}/{signal}: replacement did not start exactly the missing task; output:\n{out}"
    );
    assert_eq!(
        read_pid(&pidfile),
        Some(task_pid),
        "{kind}/{signal}: adoption replaced the survivor pid"
    );
    assert_eq!(
        process_creation_identity(task_pid),
        task_creation,
        "{kind}/{signal}: adoption replaced the survivor creation identity"
    );
    assert_eq!(
        line_count(&boots),
        1,
        "{kind}/{signal}: replacement control plane duplicated the survivor"
    );
    assert!(
        poll_until(Duration::from_secs(2), || {
            file_len(&heartbeat) > heartbeat_before_adoption
        }),
        "{kind}/{signal}: adopted task is no longer usable"
    );
    assert!(
        read_alive(&fx.task_pidfile(kind, "newcomer")),
        "{kind}/{signal}: the genuinely missing task was reported launched but is not alive"
    );
}

#[test]
fn normal_stop_and_binary_replacement_adopt_exec_unchanged_without_duplicate() {
    control_plane_replacement_preserves_agent("exec", "TERM");
}

#[test]
fn forced_kill_and_binary_replacement_adopt_exec_unchanged_without_duplicate() {
    control_plane_replacement_preserves_agent("exec", "KILL");
}

#[test]
fn normal_stop_and_binary_replacement_adopt_pty_unchanged_without_duplicate() {
    control_plane_replacement_preserves_agent("pty", "TERM");
}

#[test]
fn forced_kill_and_binary_replacement_adopt_pty_unchanged_without_duplicate() {
    control_plane_replacement_preserves_agent("pty", "KILL");
}

// ── the inverse: only an explicit teardown kills; a plain stop never does ──────────────────────────

fn explicit_teardown_kills_but_plain_stop_does_not(kind: &str) {
    if !pty_gate(&format!(
        "explicit_teardown_kills_but_plain_stop_does_not({kind})"
    )) {
        return;
    }
    let fx = Fixture::new();
    fx.write_agent("survivor", kind, false);

    let mut runner = Runner(fx.spawn_loop());
    let pidfile = fx.task_pidfile(kind, "survivor");
    assert!(
        poll_until(SPAWN_TIMEOUT, || read_alive(&pidfile)),
        "{kind}: runner never brought up a task"
    );
    let task_pid = read_pid(&pidfile).unwrap();

    // A plain stop (SIGTERM the supervisor) must NOT kill the task.
    signal_pid(runner.0.id() as i32, "TERM");
    runner.0.wait().unwrap();
    assert!(
        process_alive(task_pid),
        "{kind}: a plain runner stop killed the task — it must not"
    );

    // Retiring the spec is the one action that tears the task down.
    fx.write_agent("survivor", kind, true);
    let out = fx.up_once();
    assert!(
        out.contains("torn down"),
        "{kind}: retiring the spec did not tear the task down; output:\n{out}"
    );
    assert!(
        poll_until(DEATH_TIMEOUT, || !process_alive(task_pid)),
        "{kind}: explicit teardown did not actually kill task pid {task_pid}"
    );
}

#[test]
fn explicit_teardown_kills_exec_but_plain_stop_does_not() {
    explicit_teardown_kills_but_plain_stop_does_not("exec");
}

#[test]
fn explicit_teardown_kills_pty_but_plain_stop_does_not() {
    explicit_teardown_kills_but_plain_stop_does_not("pty");
}
