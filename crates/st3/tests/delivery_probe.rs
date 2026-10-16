//! The live probe's own consumer must prove a native handoff, not a mailbox lookup.
//!
//! `scripts/st3-delivery-probe-test` holds the fake-API cases and four native scenarios that each boot
//! their own daemons and probe seats. One Rust test per unit lets them run side by side.

/// Run the named unittest selectors of the probe script in an isolated, process-supervised child.
#[cfg(target_os = "linux")]
fn probe(selectors: &[&str]) {
    if st3::test_support::supervise_test() {
        return;
    }
    use std::path::PathBuf;

    let repo = PathBuf::from(test_env!("CARGO_MANIFEST_DIR")).join("../..");
    let scratch = tempfile::Builder::new()
        .prefix("delivery-probe-scratch-")
        .tempdir_in(test_env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let result = st3::test_support::command("setsid")
        .args(["-f", "env", "-u", "ST_AGENT", "python3"])
        .arg(repo.join("scripts/st3-delivery-probe-test"))
        .arg("--binary")
        .arg(test_env!("CARGO_BIN_EXE_st3-fixture"))
        .arg("--scratch")
        .arg(scratch.path())
        .args(selectors)
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
    // A selector that matches nothing runs zero cases and still prints OK.
    assert!(
        !stderr.contains("Ran 0 tests"),
        "the selector matched no test: {stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_probe_against_a_fake_api_alerts_and_recovers() {
    probe(&["ProbeTests"]);
}

#[cfg(target_os = "linux")]
#[test]
fn native_delivery_probe_waits_for_runtime_publication() {
    probe(&["NativeProbeTests.test_native_read_waits_for_runtime_publication"]);
}

#[cfg(target_os = "linux")]
#[test]
fn native_delivery_probe_alerts_attention_and_recovers_without_model_turns() {
    probe(&["NativeProbeTests.test_native_read_outage_attention_and_recovery"]);
}

#[cfg(target_os = "linux")]
#[test]
fn native_delivery_probe_alerts_the_agent_and_recovers_without_model_turns() {
    probe(&["NativeProbeTests.test_native_read_outage_agent_message_and_recovery"]);
}

#[cfg(target_os = "linux")]
#[test]
fn native_delivery_probe_falls_back_to_the_reviewer_when_the_agent_is_unavailable() {
    probe(&["NativeProbeTests.test_native_unavailable_alert_agent_falls_back_to_reviewer"]);
}
