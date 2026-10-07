//! The transport-decoupling gate — the permanent fix for the fleet-fragility incident.
//!
//! st2's `nomad_survival` gate proves a task survives its supervisor's *process* death (SIGTERM/
//! SIGKILL to st2). That is necessary but not sufficient: on a systemd host a supervisor and its
//! children can share one service cgroup, and `setsid` changes a
//! process's *session*, not its *cgroup*. systemd tears a unit down by **cgroup**, so a
//! `systemctl restart` of the transport/supervisor unit SIGTERM+SIGKILLs every task still sitting in
//! that cgroup — which is exactly how a fabric restart cascade-killed the whole example-linux fleet.
//!
//! This gate proves the fix: st2 spawns each task into its OWN transient systemd scope (own cgroup, a
//! SIBLING of the transport unit), so a cgroup-cascade kill of the transport unit cannot reach it. It
//! is proven with REAL processes — the real `st2` binary running *inside* a stand-in transport scope,
//! a real `systemctl --user stop` of that scope, and assertions on real pids — not a fake runner.
//!
//! The CONTROL makes the gate honest: the same cascade that must leave the isolated task ALIVE must
//! also leave the supervisor (which lives *in* the transport scope) DEAD. If the supervisor survived,
//! the "stop" did nothing and the survival assertion would be hollow.
//!
//! Isolation: the fixture points `$XDG_STATE_HOME` (exec pids) and `$PTY_ROOT` at its own temp dirs
//! and uses a throwaway host, and every scope it creates is uniquely named and torn down on `Drop`
//! (even on panic), so this can never touch the live fleet.
//!
//! Linux-only: the mechanism under test is a systemd `--user` scope. The macOS realization (setsid +
//! reparent, no cgroups) is proven in `tests/transport_isolation_macos.rs`.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use st2::host_lock::process_alive;

const HOST: &str = "isotest";
// `exec sleep` so `sh -c` replaces itself with the workload (the recorded pid IS the task, not a
// shell wrapper) — same rationale as nomad_survival.
const TASK_CMD: &str = "exec sleep 120";
const SPAWN_TIMEOUT: Duration = Duration::from_secs(20);
const DEATH_TIMEOUT: Duration = Duration::from_secs(10);

static NONCE: AtomicU32 = AtomicU32::new(0);

/// A process-unique scope unit name, so parallel tests never collide.
fn unique_unit(role: &str) -> String {
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    format!("st2-test-{role}-{}-{n}", std::process::id())
}

// ── fixture ───────────────────────────────────────────────────────────────────────────────────────

struct Fixture {
    catalog: PathBuf,
    xdg: PathBuf,
    pty_root: PathBuf,
    /// Transient scopes to tear down on drop (transport + any st2-spawned task scopes).
    scopes: std::cell::RefCell<Vec<String>>,
    /// pty session ids created (force-killed on drop).
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
            scopes: Default::default(),
            pty_sessions: Default::default(),
            _tmp: tmp,
        }
    }

    /// One service agent whose single task is `kind` = "exec" | "pty".
    fn write_agent(&self, identity: &str, kind: &str) {
        if kind == "pty" {
            self.pty_sessions
                .borrow_mut()
                .push(format!("{HOST}.{identity}.task"));
        }
        let kdl = format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{HOST}\"\n  \
             type \"service\"\n  {kind} \"task\" {{ command \"{TASK_CMD}\" }}\n}}\n"
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
    }

    /// The task's pidfile — whose liveness == "the task is running". The exec child's pid, or the pty
    /// daemon's pid (which owns the session). Both must survive the transport cascade.
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

    /// Occupy a transport scope (stand-in for a supervisor service cgroup) with:
    ///   1. a real `st2 up --once` that spawns the task into its OWN scope (a sibling of this one),
    ///      then exits — `--once` so there is no continuous reconcile that could, under concurrent
    ///      `pty list` contention, transiently misread the live session as dead and GC it (a
    ///      pre-existing reconcile race, unrelated to isolation, that a long-lived loop would expose);
    ///   2. a naive `sleep` that STAYS in this transport scope — the CONTROL: it must die in the
    ///      cascade, proving the kill actually reaches the transport cgroup (else survival is hollow).
    ///
    /// Returns the systemd-run handle; the scope is registered for teardown.
    fn spawn_transport(&self, transport_unit: &str) -> Child {
        self.scopes.borrow_mut().push(transport_unit.to_string());
        let st2 = test_env!("CARGO_BIN_EXE_st2");
        let script = self.root().join(format!("{transport_unit}.sh"));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n'{st2}' up '{}' --host '{HOST}' --once >/dev/null 2>&1\nexec sleep 300\n",
                self.catalog.display()
            ),
        )
        .unwrap();
        Command::new("systemd-run")
            .args(["--user", "--scope", "--collect", "--quiet"])
            .arg(format!("--unit={transport_unit}.scope"))
            .arg("--")
            .arg("sh")
            .arg(&script)
            .env("XDG_STATE_HOME", &self.xdg)
            .env("PTY_ROOT", &self.pty_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The fixture root (parent of the catalog) — a scratch dir NOT watched by `st2 up`.
    fn root(&self) -> &Path {
        self.catalog.parent().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let quiet = |args: &[&str]| {
            let _ = Command::new("systemctl")
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        };
        // Stop only the transport scopes WE registered — never a broad `st2-<host>` sweep, which would
        // stop a concurrently-running sibling test's task scope (same host). The task scopes this
        // fixture spawned are `--collect`ed automatically once the backstops below kill their tasks.
        for u in self.scopes.borrow().iter() {
            quiet(&["--user", "stop", &format!("{u}.scope")]);
            quiet(&["--user", "reset-failed", &format!("{u}.scope")]);
        }
        // Backstop: SIGKILL any leftover exec task pid.
        let exec_dir = self.xdg.join("st2").join(HOST).join("exec");
        if let Ok(entries) = std::fs::read_dir(&exec_dir) {
            for e in entries.flatten() {
                if e.path().extension().and_then(|x| x.to_str()) == Some("pid")
                    && let Some(pid) = read_pid(&e.path())
                {
                    for t in [format!("-{pid}"), pid.to_string()] {
                        let _ = Command::new("kill")
                            .arg("-KILL")
                            .arg(t)
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                    }
                }
            }
        }
        // Backstop: kill + remove any pty sessions we created.
        for sess in self.pty_sessions.borrow().iter() {
            for verb in ["kill", "rm"] {
                let _ = Command::new("pty")
                    .env("PTY_ROOT", &self.pty_root)
                    .args([verb, sess])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// Owns the spawned supervisor handle so a panicking assertion can never leak it.
struct Handle(Child);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ── helpers ────────────────────────────────────────────────────────────────────────────────────────

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

/// A pid's cgroup line (`0::/…`), or empty if unreadable.
fn cgroup_of(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The live pids inside a scope's cgroup (empty once the scope is drained/gone).
fn scope_pids(unit: &str) -> Vec<i32> {
    let out = match Command::new("systemctl")
        .args([
            "--user",
            "show",
            &format!("{unit}.scope"),
            "-p",
            "ControlGroup",
            "--value",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    let cg = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if cg.is_empty() {
        return vec![];
    }
    std::fs::read_to_string(format!("/sys/fs/cgroup{cg}/cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
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

/// Fire the cgroup cascade: SIGKILL every process in `unit`'s cgroup. This is the *lethal* blow of a
/// transport restart — st2's supervisor traps SIGTERM (a stray signal must not kill supervision), so
/// `systemctl stop`'s SIGTERM is survivable and only the SIGKILL escalation is fatal. Sending SIGKILL
/// directly is that escalation, deterministic and immune to signal handlers. A sibling scope's cgroup
/// is untouched — which is the whole point.
fn cascade_kill_scope(unit: &str) {
    let ok = Command::new("systemctl")
        .args([
            "--user",
            "kill",
            "--kill-whom=all",
            "--signal=SIGKILL",
            &format!("{unit}.scope"),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "failed to systemctl --user kill {unit}.scope");
}

fn have(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// This gate needs `systemd-run`/`systemctl --user` (the isolation mechanism) AND `pty` on PATH (the
/// real `st2 up` lists pty every reconcile pass). Missing either means the gate cannot run and is
/// UNPROVEN — a HARD FAILURE, never a silent green skip, unless a dev explicitly opts out with
/// `ST2_ALLOW_ISOLATION_SKIP` on a box without them. CI/gating MUST provide both.
fn isolation_gate(test: &str) -> bool {
    let systemd = have("systemd-run", &["--user", "--version"])
        && std::env::var_os("XDG_RUNTIME_DIR").is_some();
    let pty = have("pty", &["--help"]);
    if systemd && pty {
        return true;
    }
    assert!(
        std::env::var_os("ST2_ALLOW_ISOLATION_SKIP").is_some(),
        "{test}: needs systemd user scopes ({systemd}) and `pty` ({pty}) — the transport-decoupling \
         gate cannot run and is UNPROVEN. Provide both (required in CI/gating), or set \
         ST2_ALLOW_ISOLATION_SKIP=1 to skip on a box without them."
    );
    eprintln!("SKIP {test}: systemd/pty unavailable (ST2_ALLOW_ISOLATION_SKIP set)");
    false
}

// ── the guarantee ───────────────────────────────────────────────────────────────────────────────────

/// A running task survives a cgroup-cascade kill of the transport/supervisor unit — the failure mode
/// that killed the fleet — because st2 spawned it into its own scope, a sibling of that unit. Proven
/// for BOTH spawn paths: `exec` (st2's own child) and `pty` (the pty daemon + session).
fn task_survives_transport_cgroup_cascade(kind: &str) {
    if !isolation_gate(&format!("task_survives_transport_cgroup_cascade({kind})")) {
        return;
    }
    let fx = Fixture::new();
    // A per-kind identity so the two variants (which may run in parallel) never derive the same task
    // pidfile path. (Scope names carry a nonce, so those never collide; this keeps the pidfiles
    // distinct too.) Production task ids (`<host>.<identity>.<task>`) are already globally unique.
    let identity = format!("{kind}-survivor");
    fx.write_agent(&identity, kind);
    let transport = unique_unit("transport");

    // 1) Occupy the transport scope: `st2 up --once` spawns the task into its own scope and exits; a
    //    naive `sleep` stays behind in the transport scope as the control. Wait for both.
    let _tr = Handle(fx.spawn_transport(&transport));
    let task_pidfile = fx.task_pidfile(kind, &identity);
    assert!(
        poll_until(SPAWN_TIMEOUT, || read_alive(&task_pidfile)
            && !scope_pids(&transport).is_empty()),
        "st2 up --once never brought up a live task (task pidfile {})",
        task_pidfile.display()
    );
    let task_pid = read_pid(&task_pidfile).unwrap();

    // 2) PRECONDITION: the task settles into its OWN scope, NOT the transport cgroup. Without this the
    //    survival assertion could pass vacuously (e.g. if the cascade never reached the task's cgroup).
    //    We poll for the settle: for `exec`, st2 records the pid the instant it spawns `systemd-run`,
    //    which moves into its scope a moment later as systemd registers it — so an immediate read can
    //    catch it still in the transport cgroup mid-transition. A real isolation failure never settles
    //    and times out here with a clear message.
    let isolated = |pid: i32| {
        let cg = cgroup_of(pid);
        !cg.contains(&transport) && cg.contains(".scope")
    };
    assert!(
        poll_until(DEATH_TIMEOUT, || isolated(task_pid)),
        "task pid {task_pid} never left the transport cgroup into its own scope — NOT isolated. \
         cgroup: {}",
        cgroup_of(task_pid)
    );
    // The control (a naive `sleep`) is live in the transport cgroup right now.
    assert!(
        !scope_pids(&transport).is_empty(),
        "transport scope unexpectedly empty before the cascade"
    );

    // 3) Fire the cascade: SIGKILL the transport scope's cgroup — the supervisor-restart failure
    //    restart (its SIGTERM would be trapped by a supervisor; the SIGKILL escalation is what kills).
    cascade_kill_scope(&transport);

    // 4) CONTROL: everything in the transport cgroup — including the naive `sleep` — must die, draining
    //    the scope. If it did not, the SIGKILL missed the cgroup and step 5 would be hollow.
    assert!(
        poll_until(DEATH_TIMEOUT, || scope_pids(&transport).is_empty()),
        "the transport scope still has live processes after the cascade — the SIGKILL did not reach \
         its cgroup; the survival assertion would be hollow"
    );

    // 5) THE PROPERTY: the task outlived the transport cascade, still in its own scope.
    assert!(
        process_alive(task_pid),
        "task pid {task_pid} died with the transport unit — st2 is NOT transport-decoupled"
    );
    assert!(
        cgroup_of(task_pid).contains(".scope"),
        "task pid {task_pid} survived but left its scope — isolation regressed post-cascade"
    );
}

#[test]
fn exec_task_survives_transport_cgroup_cascade() {
    task_survives_transport_cgroup_cascade("exec");
}

#[test]
fn pty_task_survives_transport_cgroup_cascade() {
    task_survives_transport_cgroup_cascade("pty");
}
