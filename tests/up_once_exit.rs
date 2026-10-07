use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn failing_pty_path() -> tempfile::TempDir {
    let bin = tempfile::tempdir().unwrap();
    executable(&bin.path().join("pty"), "#!/bin/sh\nexit 42\n");
    bin
}

fn failing_pty_spawn_path() -> tempfile::TempDir {
    let bin = tempfile::tempdir().unwrap();
    executable(
        &bin.path().join("pty"),
        "#!/bin/sh\nif [ \"$1\" = list ]; then\n  printf '[]\\n'\n  exit 0\nfi\nprintf 'forced pty failure\\n' >&2\nexit 42\n",
    );
    bin
}

fn assert_errors_once_exits_nonzero(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        !output.status.success(),
        "an errored --once pass reported success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error: spawn")
            && stderr.contains("one-shot reconcile pass reported errors"),
        "missing errored-pass report:\n{stderr}"
    );
    assert!(
        !stderr.contains("pass skipped"),
        "spawn failure incorrectly skipped the pass:\n{stderr}"
    );
}

fn assert_skipped_once_exits_nonzero(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        !output.status.success(),
        "a skipped --once pass reported success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("list sessions (pass skipped)")
            && stderr.contains("one-shot reconcile pass was skipped"),
        "missing skipped-pass receipt:\n{stderr}"
    );
}

#[test]
fn catalog_up_once_exits_nonzero_when_the_pass_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = failing_pty_path();
    let agent = tmp.path().join("catalog/agents/h/worker/agent.kdl");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    fs::write(
        &agent,
        "agent \"worker\" { host \"h\"; command \"true\" }\n",
    )
    .unwrap();

    assert_skipped_once_exits_nonzero(
        Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .arg("up")
            .arg("--catalog")
            .arg(tmp.path().join("catalog"))
            .args(["--host", "h", "--once"])
            .env("PATH", bin.path())
            .env("XDG_STATE_HOME", tmp.path().join("state")),
    );
}

#[test]
fn catalog_up_once_exits_nonzero_when_the_report_has_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = failing_pty_spawn_path();
    let agent = tmp.path().join("catalog/agents/h/worker/agent.kdl");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    fs::write(
        &agent,
        "agent \"worker\" { host \"h\"; command \"true\" }\n",
    )
    .unwrap();

    assert_errors_once_exits_nonzero(
        Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .arg("up")
            .arg("--catalog")
            .arg(tmp.path().join("catalog"))
            .args(["--host", "h", "--once"])
            .env("PATH", bin.path())
            .env("XDG_STATE_HOME", tmp.path().join("state")),
    );
}

#[test]
fn spec_up_once_exits_nonzero_when_the_pass_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = failing_pty_path();
    let spec = tmp.path().join("fleet.kdl");
    fs::write(
        &spec,
        "host \"h\"\nteam \"fleet\" { agent \"worker\" { command \"true\" } }\n",
    )
    .unwrap();

    assert_skipped_once_exits_nonzero(
        Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .arg("up")
            .arg(&spec)
            .args(["--host", "h", "--once"])
            .env("PATH", bin.path())
            .env("XDG_STATE_HOME", tmp.path().join("state")),
    );
}

#[test]
fn spec_up_once_exits_nonzero_when_the_report_has_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = failing_pty_spawn_path();
    let spec = tmp.path().join("fleet.kdl");
    fs::write(
        &spec,
        "host \"h\"\nteam \"fleet\" { agent \"worker\" { command \"true\" } }\n",
    )
    .unwrap();

    assert_errors_once_exits_nonzero(
        Command::new(test_env!("CARGO_BIN_EXE_st2"))
            .arg("up")
            .arg(&spec)
            .args(["--host", "h", "--once"])
            .env("PATH", bin.path())
            .env("XDG_STATE_HOME", tmp.path().join("state")),
    );
}
