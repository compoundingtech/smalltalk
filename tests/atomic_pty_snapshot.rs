#![cfg(unix)]

//! Hermetic producer-consumer boundary: a truncated `pty list --json` response is never consumed
//! as a valid prefix. This is a dedicated single-test target so CI cannot pass on a vacuous libtest
//! name filter.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn doctor_rejects_a_partial_pty_snapshot_atomically() {
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
        "#!/bin/sh\nprintf '[{\"name\":\"h.gone.agent\",\"status\":\"running\"},'\n",
    );

    let output = Command::new(test_env!("CARGO_BIN_EXE_st2"))
        .arg("doctor")
        .arg("--catalog")
        .arg(&catalog)
        .args(["--host", "h"])
        .env("PATH", &bin)
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("PTY_ROOT", tmp.path().join("pty"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("✗ task runtime readable"), "{stdout}");
    assert!(stdout.contains("parsing `pty list --json`"), "{stdout}");
    assert!(
        !stdout.contains("retirement complete"),
        "a valid prefix must never be consumed as a partial snapshot:\n{stdout}"
    );
}
