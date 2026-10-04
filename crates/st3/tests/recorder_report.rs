use std::fs;

#[test]
fn the_cli_combines_host_logs_without_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let mut logs = Vec::new();
    for (host, program, args, code) in [
        ("host-a", "git", vec!["status"], 0),
        ("host-b", "gh", vec!["api", "repos/example"], 3),
    ] {
        let path = root.path().join(format!("{host}.jsonl"));
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({
                    "schema": "st3.recorder.command.v1",
                    "time": now,
                    "host": host,
                    "actor": "agent/example/builder",
                    "program": program,
                    "args": args,
                    "exit_code": code,
                    "signal": null,
                    "duration_ms": 10.0
                })
            ),
        )
        .unwrap();
        logs.push(path);
    }
    let output = st3::test_support::command(env!("CARGO_BIN_EXE_st3-fixture"))
        .args(["--json", "recorder", "report", "--hours", "24"])
        .arg("--log")
        .arg(&logs[0])
        .arg("--log")
        .arg(&logs[1])
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["calls"], 2);
    assert_eq!(report["by_host_actor_command"].as_array().unwrap().len(), 2);
    assert_eq!(report["gh_api_candidates_by_agent"][0]["calls"], 1);
    assert_eq!(report["failures"][0]["command"], "gh api");
}

#[test]
fn an_out_of_range_period_is_a_cli_error() {
    let root = tempfile::tempdir().unwrap();
    let output = st3::test_support::command(env!("CARGO_BIN_EXE_st3-fixture"))
        .args(["recorder", "report", "--hours", "10000000000"])
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--hours is too large"));
}
