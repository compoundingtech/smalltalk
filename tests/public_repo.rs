use std::process::Command;

#[test]
fn repository_contains_only_public_examples() {
    let output = Command::new("python3")
        .arg("scripts/check-public-repo")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run public repository check");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
