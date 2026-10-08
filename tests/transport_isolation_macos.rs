//! The transport-decoupling gate — macOS realization.
//!
//! macOS has no cgroups, and launchd does NOT cascade-kill detached children (proven in the incident:
//! the Mac fleet survived the Mac fabric restart while the systemd fleet died). So on macOS the whole
//! defense is `setsid` + reparent to launchd/init — st2's [`isolate`](st2::isolate) primitive runs in
//! `Detached` mode (a pass-through; the exec backend's `setsid` provides the detachment).
//!
//! The failure mode this rules out on macOS is a **process-group** signal: if a task stayed in the
//! spawner's process group, a group-directed kill of the spawner (or its group) would take the task
//! too. `setsid` puts the task in its own session and group, so it does not. This gate proves it with
//! real processes — a real `st2 up` in its own process group, a real `kill -KILL -<pgid>` of that
//! group, and assertions on real pids.
//!
//! The CONTROL keeps it honest: the same group kill that must leave the task ALIVE must leave the
//! spawner (which IS in the killed group) DEAD — otherwise the kill did nothing and survival is hollow.
//!
//! Linux's systemd-scope realization lives in `tests/transport_isolation.rs`.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use st2::host_lock::process_alive;

const HOST: &str = "isotestmac";
const TASK_CMD: &str = "exec sleep 120";
const SPAWN_TIMEOUT: Duration = Duration::from_secs(20);
const DEATH_TIMEOUT: Duration = Duration::from_secs(10);

static NONCE: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    catalog: PathBuf,
    xdg: PathBuf,
    pty_root: PathBuf,
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
            _tmp: tmp,
        }
    }

    fn write_exec_agent(&self, identity: &str) {
        let kdl = format!(
            "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{HOST}\"\n  \
             type \"service\"\n  exec \"task\" {{ command \"{TASK_CMD}\" }}\n}}\n"
        );
        let path = self.catalog.join(HOST).join(identity).join("agent.kdl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, kdl).unwrap();
    }

    fn task_pidfile(&self, identity: &str) -> PathBuf {
        self.xdg
            .join("st2")
            .join(HOST)
            .join("exec")
            .join(format!("{HOST}.{identity}.task.pid"))
    }

    fn supervisor_pidfile(&self) -> PathBuf {
        self.catalog.join(format!(".st2.{HOST}.lock"))
    }

    /// Spawn the real `st2 up` in its OWN process group (`setsid`), so a group-directed kill targets
    /// exactly the spawner (and anything still in its group) — the task, having `setsid`'d itself, is
    /// no longer in it.
    fn spawn_supervisor_in_own_group(&self) -> Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(test_env!("CARGO_BIN_EXE_st2"));
        cmd.arg("up")
            .arg(&self.catalog)
            .args(["--host", HOST, "--interval", "60"])
            .env("XDG_STATE_HOME", &self.xdg)
            .env("PTY_ROOT", &self.pty_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    }
}

struct Handle(Child);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
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

/// A pid's parent pid via `ps` (macOS has no `/proc`). `Some(1)` == reparented to launchd/init.
fn ppid_of(pid: i32) -> Option<i32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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

/// This gate needs `pty` on PATH (the real `st2 up` lists pty every reconcile pass). Missing it means
/// the gate cannot run and is UNPROVEN — a HARD FAILURE, never a silent skip, unless a dev opts out.
fn isolation_gate(test: &str) -> bool {
    let pty = Command::new("pty")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if pty {
        return true;
    }
    assert!(
        std::env::var_os("ST2_ALLOW_ISOLATION_SKIP").is_some(),
        "{test}: `pty` not on PATH — the transport-decoupling gate cannot run and is UNPROVEN. \
         Install `pty`, or set ST2_ALLOW_ISOLATION_SKIP=1 to skip on a box without it."
    );
    eprintln!("SKIP {test}: pty unavailable (ST2_ALLOW_ISOLATION_SKIP set)");
    false
}

/// Kill a process group (leader `pgid`) with SIGKILL: `kill -KILL -<pgid>`.
fn kill_group(pgid: i32) {
    let ok = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pgid}"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "failed to kill process group {pgid}");
}

/// A running task survives a SIGKILL of the spawner's process group and reparents to launchd — the
/// macOS realization of transport-decoupling (no cgroups; `setsid` is the whole defense).
#[test]
fn task_survives_spawner_group_kill() {
    let _ = NONCE.fetch_add(1, Ordering::Relaxed); // reserved for future parallel variants
    if !isolation_gate("task_survives_spawner_group_kill") {
        return;
    }
    let fx = Fixture::new();
    fx.write_exec_agent("survivor");

    // 1) A real `st2 up`, in its own process group, brings the task up.
    let mut sup = Handle(fx.spawn_supervisor_in_own_group());
    let task_pidfile = fx.task_pidfile("survivor");
    assert!(
        poll_until(SPAWN_TIMEOUT, || read_alive(&task_pidfile)
            && read_alive(&fx.supervisor_pidfile())),
        "supervisor never brought up a live task (task pidfile {})",
        task_pidfile.display()
    );
    let task_pid = read_pid(&task_pidfile).unwrap();
    // The spawner `setsid`'d, so its pgid == its pid — the kill target.
    let sup_pgid = read_pid(&fx.supervisor_pidfile()).unwrap();

    // 2) PRECONDITION: the task is in its OWN process group, not the spawner's (else the group kill
    //    would take it and survival would be vacuous). Its own group => its pgid == its own pid.
    let task_pgid = unsafe { libc::getpgid(task_pid) };
    assert!(
        task_pgid == task_pid,
        "task pid {task_pid} shares the spawner's process group (pgid {task_pgid}) — not detached"
    );

    // 3) Fire the group kill.
    kill_group(sup_pgid);

    // 4) CONTROL: the spawner IS in the killed group — it must die. Reap via the child handle so a
    //    zombie (kill(pid,0) reports zombies as alive) is not mistaken for a survivor.
    assert!(
        poll_until(DEATH_TIMEOUT, || matches!(sup.0.try_wait(), Ok(Some(_)))),
        "supervisor (pgid {sup_pgid}) survived the group kill — the kill did nothing; survival hollow"
    );

    // 5) THE PROPERTY: the task outlived the group kill and reparented to launchd/init (ppid == 1).
    assert!(
        process_alive(task_pid),
        "task pid {task_pid} died with the spawner's group — NOT detached"
    );
    assert!(
        poll_until(DEATH_TIMEOUT, || ppid_of(task_pid) == Some(1)),
        "task pid {task_pid} survived but did not reparent to launchd/init (ppid {:?})",
        ppid_of(task_pid)
    );
}
