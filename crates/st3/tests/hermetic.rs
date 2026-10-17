#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

#[test]
fn fixture_commands_clear_host_seat_settings_before_adding_their_own() {
    let mut command = Command::new(env!("ST3_FIXTURE_BASH"));
    command
        .env("ST_AGENT", "agent/host/seat")
        .env("ST3_ENDPOINT", "/host-daemon.sock")
        .env("ST3_INCARNATION", "host-incarnation")
        .env("ST3_PERSON", "person/pat");
    st3::test_support::clear_seat_environment(&mut command);
    command.env("ST3_ENDPOINT", "/fixture-daemon.sock")
        .args(["--noprofile", "--norc", "-c",
            "test -z \"${ST_AGENT+x}\" && test -z \"${ST3_INCARNATION+x}\" && test -z \"${ST3_PERSON+x}\" && test \"$ST3_ENDPOINT\" = /fixture-daemon.sock"]);
    assert!(command.status().unwrap().success());
}

#[test]
fn fixture_commits_ignore_local_hook_and_signing_policy() {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = st3::test_support::git()
            .current_dir(root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "--quiet"]);
    let hooks = root.path().join("rejecting-hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("commit-msg");
    std::fs::write(&hook, "#!/bin/sh\nexit 17\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    git(&["config", "commit.gpgsign", "true"]);
    git(&["config", "gpg.program", "/absent-fixture-signer"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", "Fixture commit"]);
    // Policy stays in the repository. Only this fixture command bypassed it.
    let output = st3::test_support::git()
        .current_dir(root.path())
        .args(["config", "--local", "--get", "core.hooksPath"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        hooks.to_str().unwrap()
    );
}

#[test]
fn renaming_the_production_cli_does_not_bypass_mutating_actor_checks() {
    let root = tempfile::tempdir().unwrap();
    let renamed = root.path().join("st3-fixture");
    std::os::unix::fs::symlink(test_env!("CARGO_BIN_EXE_st3"), &renamed).unwrap();
    let output = Command::new(&renamed)
        .env_clear()
        .env("HOME", root.path())
        .env("ST_AGENT", "agent/fixture/seat")
        .env("ST3_FIXTURE_MODE", "1")
        .env("ST3_TEST_DISABLE_ANCESTRY", "1")
        .args([
            "--endpoint",
            "/absent-fixture.sock",
            "--daemon-wait",
            "0",
            "missions",
            "start",
            "mission/fixture",
            "--as",
            "person/pat",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("cannot act as `person/pat`"), "{error}");
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read — DELETE at contraction — https://github.com/compoundingtech/smalltalk/issues/3032
#[test]
fn conflicting_agent_aliases_cannot_fall_back_to_native_driver_identity() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(test_env!("CARGO_BIN_EXE_st3"))
        .env_clear()
        .env("HOME", root.path())
        .env("ST_AGENT", "agent/legacy")
        .env("FRACTAL_AGENT", "agent/preferred")
        .env("ST_DRIVER_IDENTITY", "agent/fallback")
        .env("ST_DRIVER_ROOT", root.path())
        .env("ST_DRIVER_AGENT_DIR", root.path().join("agent"))
        .env("ST_DRIVER_SESSION_DIR", root.path().join("session"))
        .args(["driver-hook", "claude-observe", "SessionStart"])
        .stdin(std::process::Stdio::null())
        .output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no agent identity"), "{stderr}");
    assert!(stderr.contains("conflicting environment variables"), "{stderr}");
    assert!(!stderr.contains("agent/legacy") && !stderr.contains("agent/preferred"), "{stderr}");
}

#[test]
fn legacy_daemon_wait_preserves_invalid_empty_and_non_utf8_values() {
    use std::os::unix::ffi::OsStringExt as _;
    for value in [
        std::ffi::OsString::new(),
        std::ffi::OsString::from_vec(vec![0xff]),
    ] {
        let output = Command::new(test_env!("CARGO_BIN_EXE_st3"))
            .env_clear()
            .env("ST3_DAEMON_WAIT", value)
            .args(["work", "start", "task", "--as", "person/explicit"])
            .output().unwrap();
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--daemon-wait"), "{stderr}");
    }
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read
