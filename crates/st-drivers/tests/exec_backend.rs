//! M1b correctness net: the terminal-free `exec` backend (R09) against real short-lived processes.

use std::collections::BTreeMap;
use std::fs;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use st_drivers::exec_backend::ExecBackend;
use st_drivers::host_lock::process_alive;
use st_drivers::reconcile::{TaskLaunch, TaskTarget};
use st_drivers::spec::TaskKind;

fn exec_target(id: &str, command: &str) -> TaskTarget {
    TaskTarget {
        kind: TaskKind::Exec,
        pty_id: id.to_string(),
        bus_id: "example-linux.demo".to_string(),
        name: "ding".to_string(),
        derived: false,
        launch: TaskLaunch::Shell(command.to_string()),
        cwd: None,
        workspace: None,
        tags: BTreeMap::new(),
        env: BTreeMap::new(),
        keep: false,
        presentation: None,
    }
}

fn argv_target(id: &str, argv: &[&str]) -> TaskTarget {
    let mut target = exec_target(id, "unused");
    target.launch = TaskLaunch::Argv(argv.iter().map(|arg| (*arg).to_string()).collect());
    target
}

/// tty_nr from /proc/<pid>/stat (0 == no controlling terminal).
#[cfg(target_os = "linux")]
fn tty_nr(pid: i32) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesized and may contain spaces — split after the last ')'.
    let after = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // after ')': state(0) ppid(1) pgrp(2) session(3) tty_nr(4)
    fields.get(4).and_then(|s| s.parse::<i64>().ok())
}

fn wait_until<F: Fn() -> bool>(cond: F) -> bool {
    for _ in 0..50 {
        if cond() {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    cond()
}

fn read_exec_pid(path: &std::path::Path) -> i32 {
    let raw = fs::read_to_string(path).unwrap();
    raw.trim().parse().unwrap_or_else(|_| {
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()["pid"]
            .as_i64()
            .unwrap() as i32
    })
}

#[test]
fn exec_spawns_terminal_free_process_tracks_liveness_kills_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let catalog = tmp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let backend = ExecBackend::new(state.clone(), catalog.clone());

    let id = "example-linux.demo.ding";
    let target = exec_target(id, "sleep 30");
    backend.spawn(&target, tmp.path()).unwrap();

    // pid file recorded
    let pid_path = state.join(format!("{id}.pid"));
    assert!(pid_path.exists(), "pid file written");
    let pid = read_exec_pid(&pid_path);

    // liveness: alive
    let sessions = backend.list().unwrap();
    assert!(
        sessions.iter().any(|s| s.pty_id == id && s.alive),
        "reported alive"
    );

    // R09: the process has NO controlling terminal.
    assert_eq!(tty_nr(pid), Some(0), "exec process must be terminal-free");

    // kill → becomes dead
    backend.kill(id).unwrap();
    assert!(
        wait_until(|| backend
            .list()
            .unwrap()
            .iter()
            .any(|s| s.pty_id == id && !s.alive)),
        "reported dead after kill"
    );

    // remove clears runner-state
    backend.remove(id).unwrap();
    assert!(!pid_path.exists(), "pid file removed");
    assert!(backend.list().unwrap().iter().all(|s| s.pty_id != id));
}

#[test]
fn exec_expands_catalog_in_env_and_command() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let catalog = tmp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let backend = ExecBackend::new(state, catalog.clone());

    let out = tmp.path().join("out.txt");
    // $CATALOG in the command (sh -c expands it via the injected CATALOG env) and an env value.
    let mut target = exec_target(
        "example-linux.demo.probe",
        &format!("printf '%s|%s' \"$CATALOG\" \"$DATA\" > {}", out.display()),
    );
    target.env.insert("DATA".into(), "$CATALOG/x".into());
    backend.spawn(&target, tmp.path()).unwrap();

    assert!(wait_until(
        || out.exists() && !fs::read_to_string(&out).unwrap().is_empty()
    ));
    let got = fs::read_to_string(&out).unwrap();
    let cat = catalog.display().to_string();
    assert_eq!(got, format!("{cat}|{cat}/x"));

    backend.kill("example-linux.demo.probe").ok();
    backend.remove("example-linux.demo.probe").ok();
}

#[test]
fn exec_launches_direct_argv_with_literal_boundaries_and_catalog_expansion() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let catalog = tmp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let backend = ExecBackend::new(state, catalog.clone());

    let id = "example-linux.demo.direct";
    backend
        .spawn(
            &argv_target(
                id,
                &[
                    "printf",
                    "%s|%s|%s\n",
                    "space arg",
                    "$CATALOG/result",
                    "; echo not-shell-syntax",
                ],
            ),
            tmp.path(),
        )
        .unwrap();

    let log = catalog.join("logs").join(format!("{id}.log"));
    let expected = format!(
        "space arg|{}/result|; echo not-shell-syntax\n",
        catalog.display()
    );
    assert!(
        wait_until(|| fs::read_to_string(&log).unwrap_or_default() == expected),
        "direct argv was not preserved: {:?}",
        fs::read_to_string(&log).ok()
    );
    backend.remove(id).unwrap();
}

/// Auto-log observability: a detached exec's stdout AND stderr must be captured to a discoverable
/// `<catalog>/logs/<label>.log`, so a wedged/crashed sidecar (like a stuck ding) is inspectable after
/// the fact instead of vanishing. Proven through the real isolate/scope wrapper.
#[test]
fn exec_auto_logs_stdout_and_stderr_to_catalog_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let catalog = tmp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let backend = ExecBackend::new(state, catalog.clone());

    let id = "example-linux.demo.ding";
    backend
        .spawn(
            &exec_target(id, "echo OUT_LINE; echo ERR_LINE 1>&2"),
            tmp.path(),
        )
        .unwrap();

    let log = catalog.join("logs").join(format!("{id}.log"));
    assert!(
        wait_until(|| {
            let s = fs::read_to_string(&log).unwrap_or_default();
            s.contains("OUT_LINE") && s.contains("ERR_LINE")
        }),
        "exec stdout+stderr must be captured to {}; got: {:?}",
        log.display(),
        fs::read_to_string(&log).ok()
    );
    backend.remove(id).ok();
}

#[test]
fn exec_restart_reap_keeps_bounded_diagnostics_and_final_remove_cleans_them() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let catalog = tmp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let backend = ExecBackend::new(state.clone(), catalog.clone());
    let id = "example-linux.demo.crasher";
    let current = catalog.join("logs").join(format!("{id}.log"));
    let previous = catalog.join("logs").join(format!("{id}.log.1"));

    for (generation, expected_previous) in [
        ("GENERATION_ONE", None),
        ("GENERATION_TWO", Some("GENERATION_ONE")),
        ("GENERATION_THREE", Some("GENERATION_TWO")),
    ] {
        backend
            .spawn(
                &exec_target(id, &format!("printf '%s\\n' {generation}; exit 1")),
                tmp.path(),
            )
            .unwrap();
        assert!(
            wait_until(|| fs::read_to_string(&current)
                .unwrap_or_default()
                .contains(generation)),
            "current generation never reached {}",
            current.display()
        );
        if let Some(expected) = expected_previous {
            assert_eq!(
                fs::read_to_string(&previous).unwrap(),
                format!("{expected}\n")
            );
        } else {
            assert!(!previous.exists());
        }
        assert!(
            wait_until(|| backend
                .list()
                .unwrap()
                .iter()
                .any(|session| session.pty_id == id && !session.alive)),
            "{generation} never exited"
        );
        backend.reap_for_restart(id).unwrap();
        assert!(!state.join(format!("{id}.pid")).exists());
        assert!(!current.exists());
        assert_eq!(
            fs::read_to_string(&previous).unwrap(),
            format!("{generation}\n")
        );
    }

    backend.remove(id).unwrap();
    assert!(!state.join(format!("{id}.pid")).exists());
    assert!(!current.exists());
    assert!(!previous.exists());
}

/// macOS has no `/proc`; `ps` prints `??` when a process has no controlling terminal.
#[cfg(target_os = "macos")]
fn tty_nr(pid: i32) -> Option<i64> {
    let output = Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let tty = String::from_utf8_lossy(&output.stdout);
    let tty = tty.trim();
    Some(i64::from(!matches!(tty, "" | "?" | "??")))
}

/// The process-group field (2) of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn pgrp(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(") ")?.1;
    after.split_whitespace().nth(2)?.parse().ok()
}

/// Every live pid whose process group is `pgid` (== the setsid leader's pid for an exec task).
#[cfg(target_os = "linux")]
fn group_members(pgid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    for e in fs::read_dir("/proc").unwrap().flatten() {
        if let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok())
            && pgrp(pid) == Some(pgid)
        {
            out.push(pid);
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn group_members(pgid: i32) -> Vec<i32> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,pgid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<i32>().ok()?;
            let candidate = fields.next()?.parse::<i32>().ok()?;
            (candidate == pgid).then_some(pid)
        })
        .collect()
}

/// Teardown must reap the WHOLE task, not just the recorded leader: a `sh -c` wrapper (dash forks a
/// bare command) or a compound command leaves children that would otherwise be orphaned. The exec
/// backend puts each task in its own `setsid` group and kills the group — proven here with a task that
/// forks extra children.
#[test]
fn exec_kill_reaps_the_whole_process_group_not_just_the_leader() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let catalog = tmp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let backend = ExecBackend::new(state.clone(), catalog.clone());

    let id = "example-linux.demo.forky";
    // dash backgrounds one sleep and foregrounds another → leader (dash) + 2 child sleeps, all in the
    // one setsid group, distinct pids. A kill-the-leader-only teardown would orphan the sleeps.
    backend
        .spawn(&exec_target(id, "sleep 45 & sleep 45"), tmp.path())
        .unwrap();
    let leader = read_exec_pid(&state.join(format!("{id}.pid")));

    assert!(
        wait_until(|| group_members(leader).len() >= 2),
        "expected a multi-process group (leader + forked child); members: {:?}",
        group_members(leader)
    );
    let members = group_members(leader);

    backend.kill(id).unwrap();

    // Every member dies. `backend.list()` reaps the leader zombie (our own child) each poll so its pid
    // stops reading as alive; the forked sleeps reparent to init, which reaps them.
    assert!(
        wait_until(|| {
            let _ = backend.list();
            members.iter().all(|&p| !process_alive(p))
        }),
        "explicit teardown left group members alive: {members:?}"
    );
}
