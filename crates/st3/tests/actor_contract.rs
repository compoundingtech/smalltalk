use std::process::Command;

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(test_env!("CARGO_BIN_EXE_st3"));
    command
        .env_remove("ST_INSTANCE")
        .env_remove("ST_AGENT")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"));
    command
}

#[test]
fn actor_contract_prints_without_daemon_or_config() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args(["actor-contract", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["actor_semantics"], "actor: none means no --as syntax");
    assert!(value["contract_sha256"].as_str().is_some());
    assert_eq!(value["contract"]["kind"], "st3.actor-contract.v1");
    assert_eq!(
        value["contract"]["commands"]["st actor-contract"]["actor"],
        "none"
    );
    assert!(value.get("effective").is_none());
    assert!(!home.path().join("config").exists());
    let refused = command(home.path())
        .args(["actor-contract", "--as", "person/operator"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--as"));
}

#[test]
fn actor_contract_effective_uses_real_config_resolution() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config/st3");
    std::fs::create_dir_all(&config_dir).unwrap();
    let state_dir = home.path().join("custom-state");
    std::fs::write(
        config_dir.join("config.toml"),
        format!("state_dir = {:?}\n", state_dir),
    )
    .unwrap();
    let output = command(home.path())
        .args(["actor-contract", "--json", "--effective"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["effective"]["state_dir"], state_dir.to_str().unwrap());
    assert_eq!(
        value["effective"]["config_path"],
        config_dir.join("config.toml").to_str().unwrap()
    );
    assert!(value["effective"]["instance"].is_null());
}

#[test]
fn actor_contract_effective_uses_selected_instance() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args([
            "--instance",
            "contract-test",
            "actor-contract",
            "--json",
            "--effective",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["effective"]["instance"], "contract-test");
    assert_eq!(
        value["effective"]["state_dir"],
        home.path()
            .join(".st-instance/contract-test/state/st3")
            .to_str()
            .unwrap()
    );
}
