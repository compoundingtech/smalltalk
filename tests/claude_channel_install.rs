//! CLI wiring for the embedded Claude channel installer.
//!
//! These tests do not install user or machine state. The pure file and policy behavior lives in
//! `src/claude_channel.rs` tests.

fn st2() -> std::process::Command {
    std::process::Command::new(test_env!("CARGO_BIN_EXE_st2"))
}

#[test]
fn claude_channel_exposes_a_service_style_lifecycle() {
    let output = st2().args(["claude-channel", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for command in ["install", "status", "uninstall"] {
        assert!(help.contains(command), "{help}");
    }
    assert!(!help.contains("install-policy"), "{help}");
    assert!(!help.contains("uninstall-policy"), "{help}");
}

#[test]
fn install_allows_an_external_machine_policy_manager() {
    let output = st2()
        .args(["claude-channel", "install", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--no-policy"), "{help}");
}
