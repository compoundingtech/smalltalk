#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn retired_catalog(root: &Path) -> Child {
    let declaration = root.join("agents/h/gone/agent.kdl");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::write(
        declaration,
        "agent \"gone\" { host \"h\"; retired #true; command \"true\" }\n",
    )
    .unwrap();
    let owner = Command::new("sleep").arg("30").spawn().unwrap();
    fs::write(root.join(".st2.h.lock"), format!("{}\n", owner.id())).unwrap();
    owner
}

fn doctor(catalog: &Path, bin: &Path, state: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("doctor")
        .arg("--catalog")
        .arg(catalog)
        .args(["--host", "h"])
        .env("PATH", bin)
        .env("XDG_STATE_HOME", state)
        .env("PTY_ROOT", state.join("pty"))
        .output()
        .unwrap()
}

#[test]
fn manual_mode_is_healthy_without_a_host_lock_but_can_require_one() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/gone/agent.kdl");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        declaration,
        "agent \"gone\" { host \"h\"; retired #true; command \"true\" }\n",
    )
    .unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[]\\n'; fi\n",
    );

    let manual = doctor(&catalog, &bin, &tmp.path().join("state"));
    assert!(
        manual.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&manual.stdout),
        String::from_utf8_lossy(&manual.stderr)
    );
    assert!(
        String::from_utf8_lossy(&manual.stdout)
            .contains("✓ supervision mode manual/--once (no live host-lock)")
    );

    let required = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("doctor")
        .arg("--catalog")
        .arg(&catalog)
        .args(["--host", "h", "--require-supervisor"])
        .env("PATH", &bin)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("PTY_ROOT", tmp.path().join("pty"))
        .output()
        .unwrap();
    assert!(!required.status.success());
    assert!(
        String::from_utf8_lossy(&required.stdout)
            .contains("required but no live host-lock — run `st2 up`")
    );

    fs::write(catalog.join(".st2.h.lock"), "2000000000").unwrap();
    let stale = doctor(&catalog, &bin, &tmp.path().join("state"));
    assert!(!stale.status.success());
    assert!(
        String::from_utf8_lossy(&stale.stdout).contains("stale host-lock from a dead supervisor")
    );
}

#[test]
fn doctor_closes_stdin_for_the_noninteractive_pty_probe() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/gone/agent.kdl");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        declaration,
        "agent \"gone\" { host \"h\"; retired #true; command \"true\" }\n",
    )
    .unwrap();
    // Blocks forever when it inherits an open stdin; reaches the JSON only when stdin is /dev/null.
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nIFS= read -r _line\nprintf '[]\\n'\n",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("doctor")
        .arg("--catalog")
        .arg(&catalog)
        .args(["--host", "h"])
        .env("PATH", &bin)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("PTY_ROOT", tmp.path().join("pty"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let held_stdin = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(held_stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        !timed_out,
        "doctor inherited its caller's open stdin and hung in `pty list --json`"
    );
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_bounds_a_hung_pty_probe_and_reports_the_runtime_error() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/gone/agent.kdl");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        declaration,
        "agent \"gone\" { host \"h\"; retired #true; command \"true\" }\n",
    )
    .unwrap();
    let escaped_pid = tmp.path().join("escaped.pid");
    let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("sleep"))
        .find(|candidate| candidate.is_file())
        .expect("sleep on the test runner's PATH");
    executable(
        &bin.join("pty"),
        &format!(
            "#!/bin/sh\nset -m\n'{}' 30 &\nprintf '%s\\n' \"$!\" > '{}'\nwait\n",
            sleep.display(),
            escaped_pid.display()
        ),
    );

    let started = Instant::now();
    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    if let Ok(pid) = fs::read_to_string(&escaped_pid) {
        unsafe {
            libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL);
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "doctor did not bound a hung `pty list --json`"
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("✗ task runtime readable"), "{stdout}");
    assert!(stdout.contains("timed out after 2.0s"), "{stdout}");
}

#[test]
fn retired_declaration_is_healthy_when_tasks_and_presence_are_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[]\\n'; fi\n",
    );
    let mut owner = retired_catalog(&catalog);

    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    let _ = owner.kill();
    let _ = owner.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("✓ h.gone retirement complete (all declared tasks absent)"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("h.gone presence"),
        "retired declarations must not require presence:\n{stdout}"
    );
}

#[test]
fn retired_declaration_is_unhealthy_while_a_declared_task_is_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.gone\",\"status\":\"running\"}]\\n'; fi\n",
    );
    let mut owner = retired_catalog(&catalog);

    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    let _ = owner.kill();
    let _ = owner.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "retirement leak passed doctor:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "✗ h.gone retirement complete (all declared tasks absent) — still present: h.gone (alive)"
        ),
        "{stdout}"
    );
    assert!(
        !stdout.contains("h.gone presence"),
        "retired declarations must not require presence:\n{stdout}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("1 problem(s) found"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn retired_declaration_is_unhealthy_while_a_dead_task_record_remains() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.gone\",\"status\":\"exited\"}]\\n'; fi\n",
    );
    let mut owner = retired_catalog(&catalog);

    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    let _ = owner.kill();
    let _ = owner.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "retirement residue passed doctor:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "✗ h.gone retirement complete (all declared tasks absent) — still present: h.gone (dead)"
        ),
        "{stdout}"
    );
    assert!(
        !stdout.contains("h.gone presence"),
        "retired declarations must not require presence:\n{stdout}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("1 problem(s) found"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
