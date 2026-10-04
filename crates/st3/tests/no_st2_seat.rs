//! A Claude seat on a machine with no st2 at all: no `st2` on the daemon's PATH and no st2 state
//! directory under its private HOME. `scripts/st3-no-st2-eval/run` starts an isolated daemon and
//! one Claude seat whose `claude` is a stand-in that drives the real hook, status-line and
//! channel contracts (no model, no login). It requires observation, the native transcript in the
//! client API timeline, the status-line record, message delivery and read, a passing
//! `claude-hooks` doctor check, and no seat process whose argv or environment names an `st2`
//! program or an `/st2/` path.
#[cfg(target_os = "linux")]
#[test]
fn a_claude_seat_runs_with_no_st2_on_the_machine() {
    use std::path::PathBuf;
    use std::process::Command;

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let in_ci = std::env::var_os("CI_RUN_ID").is_some();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path =
        |program: &str| std::env::split_paths(&path).any(|dir| dir.join(program).is_file());
    if !in_ci && !(on_path("pty") && on_path("python3") && on_path("curl")) {
        eprintln!("skipped: the no-st2 seat eval needs pty, python3 and curl on PATH");
        return;
    }
    let output_root = tempfile::tempdir().unwrap();
    let evidence = output_root.path().join("evidence");
    // Double-fork out of the calling seat's ancestry: the isolated daemon binds a caller to the
    // nearest ST_AGENT above it, and the eval acts as person/eval.
    let output = Command::new("setsid")
        .args([
            "-f",
            "env",
            "-u",
            "ST_AGENT",
            "-u",
            "ST3_SUBJECT",
            "python3",
        ])
        .arg(repo.join("scripts/st3-no-st2-eval/run"))
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(&evidence)
        .output()
        .expect("run the no-st2 seat eval");
    let result = std::fs::read_to_string(evidence.join("result.json"));
    let passed = result
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|result| result["verdict"] == "pass");
    if !passed {
        eprintln!("no-st2 evidence: {}", evidence.display());
        let _ = output_root.keep();
    }
    assert!(
        passed,
        "{}\n{}\n{}",
        result.unwrap_or_default(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
