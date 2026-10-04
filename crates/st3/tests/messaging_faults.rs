//! Each sender-to-reader fault case is a separate Linux test so nextest can run them in parallel.
//! The provider API stand-in consumes native handoffs without making model calls.
#![cfg(target_os = "linux")]

fn run_case(case: &str) {
    use std::path::PathBuf;

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let old = match std::env::var_os("ST3_MESSAGING_COMPAT_BIN") {
        Some(path) => PathBuf::from(path),
        None => {
            // Pin the real channel before reexec/reporting, rather than making a current
            // process pretend it is old. Nix caches this immutable package across CI runs.
            let output = st3::test_support::command("timeout")
                .args(["10m", "bash"])
                .arg(repo.join("scripts/messaging-compat-binary"))
                .output()
                .expect(
                    "Nix builds the pinned historical channel (or set ST3_MESSAGING_COMPAT_BIN)",
                );
            assert!(
                output.status.success(),
                "historical channel build: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
        }
    };
    assert!(
        old.is_file(),
        "historical st3 is missing: {}",
        old.display()
    );
    let in_ci = std::env::var_os("CI_RUN_ID").is_some();
    let evidence_parent = std::env::var_os("ST3_MESSAGING_FAULTS_EVIDENCE").map(PathBuf::from);
    let mut output_root = Some(if let Some(parent) = &evidence_parent {
        std::fs::create_dir_all(parent).unwrap();
        tempfile::Builder::new()
            .prefix(&format!("{case}-"))
            .tempdir_in(parent)
            .unwrap()
    } else if in_ci {
        let artifacts = repo.join("target/messaging-faults");
        std::fs::create_dir_all(&artifacts).unwrap();
        tempfile::Builder::new()
            .prefix(&format!("{case}-"))
            .tempdir_in(artifacts)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    });
    let evidence = output_root.as_ref().unwrap().path().join("evidence");
    // Debug executables can be hundreds of megabytes. Keep their per-case copies on Cargo's
    // artifact filesystem while the Python fixture keeps its Unix sockets in a short /tmp root.
    let scratch = tempfile::Builder::new()
        .prefix("messaging-fault-scratch-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    // Double-fork out of the CI seat's ancestry. A sender is person/eval; an st harness
    // must never impersonate that sender. Captured pipes stay open until the eval exits.
    let output = st3::test_support::command("setsid")
        .args(["-f", "env", "-u", "ST_AGENT", "python3"])
        .arg(repo.join("scripts/st3-messaging-faults-eval/run"))
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(&evidence)
        .arg("--old-binary")
        .arg(old)
        .arg("--scratch")
        .arg(scratch.path())
        .args(["--cases", case])
        .output()
        .expect("run the isolated messaging fault eval");
    let result = std::fs::read_to_string(evidence.join("result.json"));
    let passed = result
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|result| result["verdict"] == "pass" && result.get("ended").is_some());
    if !passed || in_ci || evidence_parent.is_some() {
        eprintln!("messaging fault evidence: {}", evidence.display());
        let _ = output_root.take().unwrap().keep();
    }
    assert!(
        output.status.success() && result.is_ok(),
        "eval did not finish: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    // A detached process's exit status belongs to setsid; the completed matrix is authoritative.
    assert!(
        result.get("ended").is_some(),
        "eval stopped early: {result}"
    );
    let cases = result["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 1, "{result}");
    assert_eq!(cases[0]["case"], case, "{result}");
    assert_eq!(
        result["verdict"],
        "pass",
        "{result}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn baseline() {
    run_case("baseline");
}

#[test]
fn daemon_restart() {
    run_case("daemon-restart");
}

#[test]
fn binary_swap() {
    run_case("binary-swap");
}

#[test]
fn path_deploy() {
    run_case("path-deploy");
}

#[test]
fn link_seconds() {
    run_case("link-seconds");
}

#[test]
fn link_minutes() {
    run_case("link-minutes");
}

#[test]
fn receiver_down() {
    run_case("receiver-down");
}

#[test]
fn harness_restart() {
    run_case("harness-restart");
}

#[test]
fn channel_killed() {
    run_case("channel-killed");
}

#[test]
fn old_channel() {
    run_case("old-channel");
}

#[test]
fn handoff_failed() {
    run_case("handoff-failed");
}
