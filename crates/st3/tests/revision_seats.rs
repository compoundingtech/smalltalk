//! Actual mission controls on a private daemon with real terminal runtimes.
#[cfg(target_os = "linux")]
#[test]
fn isolated_revision_preserves_completed_builder_declarations_and_cleanup() {
    let output = tempfile::tempdir().unwrap();
    let evidence = output.path().join("evidence");
    let repo = std::path::Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../..");
    let result = st3::test_support::command("env")
        .args(["-u", "ST_AGENT", "-u", "ST3_SUBJECT", "python3"])
        .arg(repo.join("scripts/st3-revision-seat-eval/run"))
        .arg(test_env!("CARGO_BIN_EXE_st3-fixture"))
        .arg(&evidence)
        .output()
        .unwrap();
    let verdict = std::fs::read_to_string(evidence.join("result.json")).unwrap_or_default();
    if !result.status.success() {
        let _ = output.keep();
    }
    assert!(
        result.status.success(),
        "{verdict}\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
