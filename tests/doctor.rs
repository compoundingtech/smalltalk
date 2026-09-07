#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

mod support;

use support::RETIRED_RESOURCES;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn retired_catalog(root: &Path, retirement: &str, extra: &str) -> Child {
    let declaration = root.join("agents/h/gone/agent.kdl");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::write(
        declaration,
        format!("agent \"gone\" {{\n  host \"h\"\n  {retirement}\n{extra}  command \"true\"\n}}\n"),
    )
    .unwrap();
    let owner = Command::new("sleep").arg("30").spawn().unwrap();
    fs::write(root.join(".st2.h.lock"), format!("{}\n", owner.id())).unwrap();
    owner
}

fn suspended_catalog(root: &Path, keep: bool) {
    let declaration = root.join("agents/h/idle/agent.kdl");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::write(
        declaration,
        format!(
            "agent \"idle\" {{ host \"h\"; desired-state \"suspended\" reason=\"Waiting for capacity\"; keep #{keep}; command \"true\" }}\n"
        ),
    )
    .unwrap();
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

fn running_agent(catalog: &Path, identity: &str) {
    let directory = catalog.join("agents/h").join(identity);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("agent.kdl"),
        format!("agent \"{identity}\" {{ host \"h\"; command \"true\" }}\n"),
    )
    .unwrap();
    fs::write(directory.join("status"), "available\n").unwrap();
}

fn send_message(catalog: &Path, from: &str, to: &str, body: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["message", "send", to, "--root"])
        .arg(catalog)
        .args(["--host", "h", "--as", from, "-m", body])
        .output()
        .unwrap()
}

fn bytes_digest(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
fn doctor_reports_a_sender_ledger_that_blocks_outbound_messages_without_mutating_it() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    running_agent(&catalog, "sender");
    running_agent(&catalog, "recipient");
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.sender\",\"status\":\"running\"},{\"name\":\"h.recipient\",\"status\":\"running\"}]\\n'; fi\n",
    );
    assert!(
        send_message(&catalog, "sender", "recipient", "older")
            .status
            .success()
    );
    assert!(
        send_message(&catalog, "sender", "recipient", "newer")
            .status
            .success()
    );

    let sender_root = catalog.join("agents/h/sender/resources/sent");
    let mut rows = fs::read_dir(sender_root.join("messages"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    rows.sort();
    let older = fs::read(&rows[0]).unwrap();
    let pending_path = sender_root
        .join("pending")
        .join(format!("{}.json", bytes_digest(&older)));
    fs::write(&pending_path, &older).unwrap();

    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "blocked sender passed doctor:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "✗ h.sender outbound message ledger — cannot send: committed pending intent is missing its active marker"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("✓ h.recipient outbound message ledger"),
        "{stdout}"
    );
    assert_eq!(fs::read(&pending_path).unwrap(), older);
    assert!(
        !catalog.join("agents/h/recipient/resources/sent").exists(),
        "doctor must not create sender state while it inspects an unused ledger"
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
    let mut owner = retired_catalog(&catalog, "retired #true", "");

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

/// dotfiles#1535: `st2 doctor` for a retired agent requires runtime-record absence only. Declared
/// `resource` bindings (including a `work://` URI) are declaration metadata that survive retirement
/// byte-identical, so they must NOT make retirement incomplete or add any resource-specific check.
#[test]
fn retired_declaration_with_resources_is_healthy_when_tasks_are_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[]\\n'; fi\n",
    );
    let declaration = catalog.join("agents/h/gone/agent.kdl");
    let mut owner = retired_catalog(
        &catalog,
        "desired-state \"retired\" reason=\"Mission complete\"",
        RETIRED_RESOURCES,
    );
    let before = fs::read(&declaration).unwrap();

    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    let _ = owner.kill();
    let _ = owner.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "resources must not fail retirement health:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("✓ h.gone retirement complete (all declared tasks absent)"),
        "{stdout}"
    );
    // Doctor is read-only and adds no resource-specific line for a retired agent.
    assert!(
        !stdout.contains("resource") && !stdout.contains("work://"),
        "retired doctor must not check resources:\n{stdout}"
    );
    // The declaration — including its resources — is untouched by the read-only check.
    assert_eq!(fs::read(&declaration).unwrap(), before);
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
    let mut owner = retired_catalog(&catalog, "retired #true", "");

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
    let mut owner = retired_catalog(&catalog, "retired #true", "");

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

#[test]
fn suspended_declaration_is_healthy_when_tasks_are_absent_without_presence() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    suspended_catalog(&catalog, false);
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[]\\n'; fi\n",
    );

    let output = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("✓ h.idle suspension effective (no live tasks)"),
        "{stdout}"
    );
    assert!(!stdout.contains("h.idle presence"), "{stdout}");
}

#[test]
fn suspended_declaration_distinguishes_live_dead_keep_and_dead_nonkeep() {
    for (status, keep, healthy, detail) in [
        ("running", false, false, "h.idle (alive)"),
        ("exited", false, false, "h.idle (dead non-keep)"),
        ("exited", true, true, ""),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        suspended_catalog(&catalog, keep);
        executable(
            &bin.join("pty"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{{\"name\":\"h.idle\",\"status\":\"{status}\"}}]\\n'; fi\n"
            ),
        );

        let output = doctor(&catalog, &bin, &tmp.path().join("state"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            output.status.success(),
            healthy,
            "status={status} keep={keep}\n{stdout}"
        );
        assert!(
            stdout.contains("h.idle suspension effective (no live tasks)"),
            "{stdout}"
        );
        if !detail.is_empty() {
            assert!(stdout.contains(detail), "{stdout}");
        }
        assert!(!stdout.contains("h.idle presence"), "{stdout}");
    }
}

#[test]
fn missing_delivery_is_advisory_while_an_invalid_delivery_is_a_catalog_problem() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; command "true" }"#,
    )
    .unwrap();
    fs::write(declaration.parent().unwrap().join("status"), "available\n").unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.worker\",\"status\":\"running\"}]\\n'; fi\n",
    );

    let missing = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&missing.stdout);
    assert!(
        missing.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(
        stdout.contains(
            "⚠ h.worker delivery transport missing — declare `ding`, `deliver`, or a driver block; agent receives no DING"
        ),
        "{stdout}"
    );

    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; command "true"; deliver "mcp" }"#,
    )
    .unwrap();
    let declared = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&declared.stdout);
    assert!(declared.status.success(), "{stdout}");
    assert!(!stdout.contains("delivery transport missing"), "{stdout}");

    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; claude { prompt "Start work." } }"#,
    )
    .unwrap();
    let driver = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&driver.stdout);
    assert!(driver.status.success(), "{stdout}");
    assert!(!stdout.contains("delivery transport missing"), "{stdout}");

    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; command "true"; deliver "mpc" }"#,
    )
    .unwrap();
    let invalid = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&invalid.stdout);
    assert!(!invalid.status.success(), "{stdout}");
    assert!(
        stdout.contains("unsupported `deliver` value 'mpc'"),
        "{stdout}"
    );
}

#[test]
fn observed_harness_state_arms_are_advisory_except_a_fresh_live_record() {
    use st2::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer};

    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; command "true"; deliver "mcp" }"#,
    )
    .unwrap();
    let agent_dir = declaration.parent().unwrap().to_path_buf();
    fs::write(agent_dir.join("status"), "available\n").unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.worker\",\"status\":\"running\"}]\\n'; fi\n",
    );

    // No record: a driver gap is an advisory, never a failing check.
    let absent = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&absent.stdout);
    assert!(absent.status.success(), "{stdout}");
    assert!(
        stdout.contains("⚠ h.worker observed harness state absent"),
        "{stdout}"
    );

    // A fresh live record is a passing check.
    let mut writer = Writer::new(
        &agent_dir,
        "h.worker",
        "codex",
        Some("h.worker".to_string()),
    );
    writer
        .observe(Observation::new(
            Activity::Active,
            BlockedOn::None,
            InputBuffer::Unknown,
        ))
        .unwrap();
    let live = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&live.stdout);
    assert!(live.status.success(), "{stdout}");
    assert!(
        stdout.contains("h.worker observed harness state fresh (is `active`)"),
        "{stdout}"
    );

    // A terminal record while the declaration wants the seat running is the crashed-seat signal.
    writer.ended("signal 9").unwrap();
    let ended = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&ended.stdout);
    assert!(ended.status.success(), "{stdout}");
    assert!(
        stdout.contains("⚠ h.worker observed harness state ended"),
        "{stdout}"
    );
    assert!(stdout.contains("session ended (signal 9)"), "{stdout}");

    // A record that derives `unknown` names its reason, still advisory.
    fs::write(agent_dir.join("harness-state"), "garbage").unwrap();
    let indeterminate = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&indeterminate.stdout);
    assert!(indeterminate.status.success(), "{stdout}");
    assert!(
        stdout.contains("⚠ h.worker observed harness state indeterminate"),
        "{stdout}"
    );
    assert!(stdout.contains("(malformed-record)"), "{stdout}");
}

#[test]
fn native_driver_diagnostic_roster_and_doctor_agree_and_recovery_clears() {
    use st2::driver_diagnostic::{Driver, Publisher, Reason, Source, Stage, Support};

    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; opencode { prompt "go" } }"#,
    )
    .unwrap();
    let agent_dir = declaration.parent().unwrap();
    fs::write(agent_dir.join("status"), "available\n").unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.worker\",\"status\":\"running\"}]\\n'; fi\n",
    );

    let mut publisher = Publisher::new(
        agent_dir,
        Driver::OpenCode,
        Some("1.18.19".to_string()),
        Support::Supported,
    );
    publisher.publish(Stage::Seed, Reason::UnknownStatus, Source::StatusSnapshot);

    let roster = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(&catalog)
        .args(["--host", "h", "--identity", "h.worker", "--json"])
        .output()
        .unwrap();
    assert!(roster.status.success());
    let wire: serde_json::Value = serde_json::from_slice(&roster.stdout).unwrap();
    assert_eq!(wire[0]["driverDiagnostic"]["status"], "failure");
    assert_eq!(wire[0]["driverDiagnostic"]["stage"], "seed");
    assert_eq!(wire[0]["driverDiagnostic"]["reason"], "unknownStatus");

    let diagnosed = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&diagnosed.stdout);
    assert!(diagnosed.status.success(), "{stdout}");
    assert!(
        stdout.contains("native driver diagnostic: seed/unknownStatus"),
        "{stdout}"
    );
    assert!(
        stdout.contains(st2::driver_diagnostic::repair_text(
            &st2::driver_diagnostic::read(&st2::driver_diagnostic::path(agent_dir))
        )),
        "{stdout}"
    );

    publisher.clear(Stage::Seed);
    let recovered = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    assert!(recovered.status.success(), "{stdout}");
    assert!(stdout.contains("native driver diagnostic absent"), "{stdout}");
    assert!(!stdout.contains("seed/unknownStatus"), "{stdout}");

    fs::write(st2::driver_diagnostic::path(agent_dir), b"{bad").unwrap();
    let malformed = doctor(&catalog, &bin, &tmp.path().join("state"));
    let stdout = String::from_utf8_lossy(&malformed.stdout);
    assert!(malformed.status.success(), "{stdout}");
    assert!(
        stdout.contains("native driver diagnostic indeterminate (malformedRecord)"),
        "{stdout}"
    );
}

/// A Claude, Codex, or omp seat publishes this record only when its own typed turn result names a
/// rejected credential, so its two Doctor lines are the failure advisory and NOTHING at all:
/// absence is the healthy steady state for every such seat in a fleet — and omp is the fleet's
/// default managed harness — so warning under each of them would bury the one seat that actually
/// needs a human.
#[test]
fn a_rejected_provider_credential_advises_while_absence_stays_silent() {
    use st2::driver_diagnostic::{Driver, Publisher, Reason, Source, Stage, Support};

    for (block, driver, word) in [
        ("claude", Driver::Claude, "claude"),
        ("codex", Driver::Codex, "codex"),
        ("omp", Driver::Omp, "omp"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let declaration = catalog.join("agents/h/worker/agent.kdl");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(declaration.parent().unwrap()).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            &declaration,
            format!(r#"agent "worker" {{ host "h"; {block} {{ prompt "go" }} }}"#),
        )
        .unwrap();
        let agent_dir = declaration.parent().unwrap();
        fs::write(agent_dir.join("status"), "available\n").unwrap();
        executable(
            &bin.join("pty"),
            "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.worker\",\"status\":\"running\"}]\\n'; fi\n",
        );

        let healthy = doctor(&catalog, &bin, &tmp.path().join("state"));
        let stdout = String::from_utf8_lossy(&healthy.stdout);
        assert!(healthy.status.success(), "{block}: {stdout}");
        assert!(
            !stdout.contains("native driver diagnostic"),
            "a {block} seat that never had a credential refused says nothing: {stdout}"
        );

        let mut publisher = Publisher::new(agent_dir, driver, None, Support::Unknown);
        publisher.publish(
            Stage::ProviderAuth,
            Reason::ProviderAuthRejected,
            Source::TurnResult,
        );

        let roster = Command::new(env!("CARGO_BIN_EXE_st2"))
            .arg("agents")
            .arg(&catalog)
            .args(["--host", "h", "--identity", "h.worker", "--json"])
            .output()
            .unwrap();
        assert!(roster.status.success());
        let wire: serde_json::Value = serde_json::from_slice(&roster.stdout).unwrap();
        assert_eq!(wire[0]["driverDiagnostic"]["status"], "failure");
        assert_eq!(wire[0]["driverDiagnostic"]["driver"], word);
        assert_eq!(wire[0]["driverDiagnostic"]["stage"], "providerAuth");
        assert_eq!(
            wire[0]["driverDiagnostic"]["reason"],
            "providerAuthRejected"
        );
        assert_eq!(wire[0]["driverDiagnostic"]["source"], "turnResult");

        let rejected = doctor(&catalog, &bin, &tmp.path().join("state"));
        let stdout = String::from_utf8_lossy(&rejected.stdout);
        assert!(rejected.status.success(), "{block}: {stdout}");
        assert!(
            stdout.contains("native driver diagnostic: providerAuth/providerAuthRejected"),
            "{block}: {stdout}"
        );
        assert!(
            stdout.contains("re-login with the account's own client"),
            "{block}: {stdout}"
        );

        publisher.clear(Stage::ProviderAuth);
        let recovered = doctor(&catalog, &bin, &tmp.path().join("state"));
        let stdout = String::from_utf8_lossy(&recovered.stdout);
        assert!(recovered.status.success(), "{block}: {stdout}");
        assert!(
            !stdout.contains("native driver diagnostic"),
            "{block}: {stdout}"
        );
    }
}

/// HC-R17: Doctor's harness-context lines are advisory in both directions — a reading at or above
/// st2's attention threshold and a stale record beside a `running` desired state each print a
/// warning and leave the exit status alone, while a fresh reading below the threshold prints
/// nothing at all. Nothing here can fail a health check on an unfenced, advisory number.
#[test]
fn harness_context_doctor_lines_are_advisory_and_never_change_the_exit_status() {
    use st2::harness_context::{Harness, Reading, Writer, harness_context_path};

    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let declaration = catalog.join("agents/h/worker/agent.kdl");
    let bin = tmp.path().join("bin");
    let state = tmp.path().join("state");
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &declaration,
        r#"agent "worker" { host "h"; opencode { prompt "go" } }"#,
    )
    .unwrap();
    let agent_dir = declaration.parent().unwrap().to_path_buf();
    fs::write(agent_dir.join("status"), "available\n").unwrap();
    executable(
        &bin.join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then printf '[{\"name\":\"h.worker\",\"status\":\"running\"}]\\n'; fi\n",
    );

    // No record at all: Doctor says nothing about context and stays green.
    let quiet = doctor(&catalog, &bin, &state);
    let stdout = String::from_utf8_lossy(&quiet.stdout);
    assert!(quiet.status.success(), "{stdout}");
    assert!(!stdout.contains("harness context"), "{stdout}");

    // A fresh reading below the threshold is likewise silent.
    let mut writer = Writer::new(&agent_dir, "h.worker", Harness::Claude).unwrap();
    let fill = |percent: f64| Reading {
        used_tokens: Some((percent * 2_000.0) as u64),
        window_tokens: Some(200_000),
        used_percent: Some(percent),
        ..Reading::default()
    };
    writer.observe(fill(41.0)).unwrap();
    let below = doctor(&catalog, &bin, &state);
    let stdout = String::from_utf8_lossy(&below.stdout);
    assert!(below.status.success(), "{stdout}");
    assert!(!stdout.contains("harness context"), "{stdout}");

    // At the threshold: an advisory, and the exit status is unchanged.
    writer.observe(fill(80.0)).unwrap();
    let warned = doctor(&catalog, &bin, &state);
    let stdout = String::from_utf8_lossy(&warned.stdout);
    assert!(warned.status.success(), "{stdout}");
    assert!(stdout.contains("h.worker harness context at 80%"), "{stdout}");
    assert!(stdout.starts_with("  ⚠") || stdout.contains("⚠ h.worker harness context"), "{stdout}");
    assert!(stdout.contains("all checks passed"), "{stdout}");

    // Above the window is carried raw into the advisory rather than clamped away.
    writer.observe(fill(104.0)).unwrap();
    let overrun = doctor(&catalog, &bin, &state);
    let stdout = String::from_utf8_lossy(&overrun.stdout);
    assert!(overrun.status.success(), "{stdout}");
    assert!(stdout.contains("h.worker harness context at 104%"), "{stdout}");

    // A stale record beside a running desired state warns on its own axis, still advisory. Backdate
    // the reading past the horizon exactly as the passage of time would.
    let path = harness_context_path(&agent_dir);
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let aged = st2::message::now_ms()
        - u64::try_from(st2::harness_context::HARNESS_CONTEXT_STALE.as_millis()).unwrap()
        - 60_000;
    record["observedAtMs"] = serde_json::json!(aged);
    record["usedPercent"] = serde_json::json!(12.0);
    fs::write(&path, format!("{record}\n")).unwrap();

    let stale = doctor(&catalog, &bin, &state);
    let stdout = String::from_utf8_lossy(&stale.stdout);
    assert!(stale.status.success(), "{stdout}");
    assert!(stdout.contains("h.worker harness context stale"), "{stdout}");
    assert!(
        !stdout.contains("harness context at"),
        "a low stale reading warns about its age, not its level: {stdout}"
    );
    assert!(stdout.contains("all checks passed"), "{stdout}");
}
