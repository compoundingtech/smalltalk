//! Subagent claims end to end. `scripts/st3-subagents-eval/run` starts an isolated daemon with
//! Claude seats whose `claude` is a stand-in that reports subagents through the real hooks (no
//! model, no login). It requires one appearance and one end per subagent on its parent seat, with
//! type, description, session, held step and tokens; renewals only while one runs; and an end for
//! every subagent whose lease ran out, whose harness was killed or restarted, whose session ended,
//! or whose seat was stopped or removed, including across a daemon restart.
#[cfg(target_os = "linux")]
#[test]
fn a_seats_subagents_appear_renew_and_end() {
    use std::path::PathBuf;
    use std::process::Command;

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let in_ci = std::env::var_os("CI_RUN_ID").is_some();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path =
        |program: &str| std::env::split_paths(&path).any(|dir| dir.join(program).is_file());
    if !in_ci && !(on_path("pty") && on_path("python3") && on_path("curl")) {
        eprintln!("skipped: the subagent seat eval needs pty, python3 and curl on PATH");
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
        .arg(repo.join("scripts/st3-subagents-eval/run"))
        .arg(env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(&evidence)
        .output()
        .expect("run the subagent seat eval");
    let result = std::fs::read_to_string(evidence.join("result.json"));
    let passed = result
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|result| result["verdict"] == "pass");
    if !passed {
        eprintln!("subagent evidence: {}", evidence.display());
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
