//! The machine CLI inventory is a real process contract, independent of a daemon.
use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_st3"))
        .args(arguments)
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("ST_AGENT", "agent/test/cli-inventory")
        .env("ST3_ENDPOINT", root.join("absent-daemon.sock"))
        .env("ST3_DAEMON_WAIT", "0")
        .output()
        .unwrap()
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
