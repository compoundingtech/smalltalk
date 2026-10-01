//! The sender-to-reader fault matrix runs in the normal Linux suite, including st/ci.
//! The provider API stand-in consumes native handoffs without making model calls.
#[cfg(target_os = "linux")]
#[test]
fn messages_recover_across_transport_and_process_faults() {
    use std::path::PathBuf;
    use std::process::Command;

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let old = match std::env::var_os("ST3_MESSAGING_COMPAT_BIN") {
        Some(path) => PathBuf::from(path),
        None => {
            // Pin the real channel before reexec/reporting, rather than making a current
            // process pretend it is old. Nix caches this immutable package across CI runs.
            let baseline: serde_json::Value = serde_json::from_str(include_str!(
                "../../../.github/messaging-compat-baseline.json"
            ))
            .unwrap();
            let commit = baseline["commit"].as_str().unwrap();
            assert_eq!(commit.len(), 40);
            assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
            let expression = format!(
                "((builtins.getFlake \"github:compoundingtech/smalltalk/{commit}\").packages.${{builtins.currentSystem}}.st3).overrideAttrs (_: {{ doCheck = false; nativeCheckInputs = []; postInstall = \"\"; cargoBuildFlags = [\"-p\" \"st3\"]; }})"
            );
            let output = Command::new("timeout")
                .args([
                    "10m",
                    "nix",
                    "build",
                    "--impure",
                    "--no-link",
                    "--print-out-paths",
                    "--expr",
                    &expression,
                ])
                .output()
                .expect(
                    "Nix builds the pinned historical channel (or set ST3_MESSAGING_COMPAT_BIN)",
                );
            assert!(
                output.status.success(),
                "historical channel build: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()).join("bin/st3")
        }
    };
    assert!(
        old.is_file(),
        "historical st3 is missing: {}",
        old.display()
    );
    let in_ci = std::env::var_os("CI_RUN_ID").is_some();
    let mut output_root = Some(if in_ci {
        let artifacts = repo.join("target/messaging-faults");
        std::fs::create_dir_all(&artifacts).unwrap();
        tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(artifacts)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    });
    let evidence = std::env::var_os("ST3_MESSAGING_FAULTS_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| output_root.as_ref().unwrap().path().join("evidence"));
    // Double-fork out of the CI seat's ancestry. A sender is person/eval; an st harness
    // must never impersonate that sender. Captured pipes stay open until the eval exits.
    let output = Command::new("setsid")
        .args(["-f", "env", "-u", "ST_AGENT", "python3"])
        .arg(repo.join("scripts/st3-messaging-faults-eval/run"))
        .arg(env!("CARGO_BIN_EXE_st3"))
        .arg(&evidence)
        .arg("--old-binary")
        .arg(old)
        .output()
        .expect("run the isolated messaging fault eval");
    let result = std::fs::read_to_string(evidence.join("result.json"));
    let passed = result
        .as_ref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|result| result["verdict"] == "pass" && result.get("ended").is_some());
    if !passed || in_ci {
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
    assert_eq!(result["cases"].as_array().unwrap().len(), 11, "{result}");
    assert_eq!(
        result["verdict"],
        "pass",
        "{result}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
