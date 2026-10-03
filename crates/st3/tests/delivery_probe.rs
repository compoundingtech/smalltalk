//! The live probe's own consumer must prove a native handoff, not a mailbox lookup.
#[cfg(target_os = "linux")]
#[test]
fn native_delivery_probe_alerts_and_recovers_without_model_turns() {
    use std::path::PathBuf;
    use std::process::Command;

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let result = Command::new("setsid")
        .args(["-f", "env", "-u", "ST_AGENT", "python3"])
        .arg(repo.join("scripts/st3-delivery-probe-test"))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .output()
        .expect("run the isolated native delivery probe");
    let stderr = String::from_utf8_lossy(&result.stderr);
    // setsid double-forks past a CI seat's actor guard. The descendant retains
    // the output pipes; unittest's completed result is the success authority.
    assert!(
        result.status.success() && stderr.contains("\nOK\n"),
        "{stderr}\n{}",
        String::from_utf8_lossy(&result.stdout)
    );
}
