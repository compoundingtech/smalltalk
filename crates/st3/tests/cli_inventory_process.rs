//! The machine CLI inventory is a real process contract, independent of a daemon.
use std::path::Path;
use std::process::{Command, Output};

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_st3"));
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("ST_AGENT", "agent/test/cli-inventory")
        .env("ST3_ENDPOINT", root.join("absent-daemon.sock"))
        .env("ST3_DAEMON_WAIT", "0");
    command
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    command(root).args(arguments).output().unwrap()
}

#[test]
fn standalone_inventory_exits_with_live_json_without_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let output = run(root.path(), &["--read-contract-inventory-json"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "st3.cli-inventory.v1");
    assert_eq!(value["modes"], serde_json::json!(["human", "json"]));
    assert!(value["aliases"].is_object());
    let commands = value["commands"].as_array().unwrap();
    assert!(commands.len() > 100, "{commands:?}");
    assert!(commands.iter().any(|row| row.as_str() == Some("st claude-channel install-policy")));
    assert!(value.get("source_head").is_none());
    assert!(value.get("source_tree").is_none());
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[test]
fn mixed_inventory_requests_exit_before_an_action() {
    let root = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--read-contract-inventory-json", "documents", "put"],
        vec!["documents", "put", "--read-contract-inventory-json"],
        vec!["--read-contract-inventory-json=true"],
    ] {
        let output = run(root.path(), &arguments);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(
            output.stderr,
            b"st: --read-contract-inventory-json must be used alone\n"
        );
    }
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[test]
fn literal_inventory_names_after_terminator_reach_ordinary_dispatch() {
    let root = tempfile::tempdir().unwrap();
    for literal in [
        "--read-contract-inventory-json",
        "--read-contract-inventory-json=true",
    ] {
        std::fs::write(root.path().join(literal), b"invented document\n").unwrap();
        let output = run(
            root.path(),
            &["documents", "put", "--as", "doc/test/inventory", "--", literal],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("--read-contract-inventory-json must be used alone"),
            "{output:?}"
        );
        assert!(!output.status.success(), "the fixture has no daemon: {output:?}");
        assert_eq!(std::fs::read(root.path().join(literal)).unwrap(), b"invented document\n");
    }
    assert!(!root.path().join("absent-daemon.sock").exists());
}

#[cfg(unix)]
#[test]
fn literal_inventory_argument_after_terminator_reaches_a_benign_provider() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("fake-provider");
    let marker = root.path().join("received-argv");
    std::fs::write(&script, b"#!/bin/sh\nmarker=$1\nshift\nprintf '%s\\n' \"$@\" > \"$marker\"\n")
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let literal = "--read-contract-inventory-json=true";
    let mut child = command(root.path())
        .args([
            "driver",
            "exec",
            "--subject",
            "test/cli-inventory",
            "--",
            script.to_str().unwrap(),
            marker.to_str().unwrap(),
            literal,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the fake provider never received its argv");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The missing daemon cannot acknowledge runtime.observed; stop only this isolated process.
    let _ = child.kill();
    child.wait().unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), format!("{literal}\n"));
    assert!(!root.path().join("absent-daemon.sock").exists());
}
