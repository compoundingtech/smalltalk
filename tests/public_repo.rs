use std::process::Command;

#[test]
fn repository_contains_only_public_examples() {
    for script in [
        "scripts/check-public-repo-test",
        "scripts/check-public-repo",
    ] {
        let output = Command::new("python3")
            .arg(script)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run public repository check");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
