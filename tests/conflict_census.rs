use std::process::Command;

// Needs python3 and git on PATH and no network; the script test builds its own repository.
#[test]
fn conflict_census_finds_conflicts_on_a_throwaway_repository() {
    let output = Command::new("python3")
        .arg("scripts/conflict-census-test")
        .current_dir(test_env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run the conflict census test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
