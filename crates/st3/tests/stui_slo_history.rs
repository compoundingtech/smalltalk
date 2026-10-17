//! `st ui --slo` reads local summaries before client configuration or terminal setup.
use std::{path::Path, process::Command};

fn history(root: &Path) -> String {
    let output = Command::new(test_env!("CARGO_BIN_EXE_st3"))
        .args(["ui", "--slo"]).env_remove("ST_AGENT").env_remove("FRACTAL_AGENT")
        .env("HOME", root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("missing-config"))
        .env("ST3_PERSON", "invalid")
        .env("ST3_ENDPOINT", root.join("missing-endpoint"))
        .env("STUI_TIMING_LOG", root.join("unexpected-timing"))
        .env("STUI_OPEN_TRACE_FILE", root.join("unexpected-open-trace"))
        .env("STUI_OPEN_TRACE_CONVERSATION", "agent/example/observer")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(!root.join("state").exists());
    assert!(!root.join("unexpected-timing").exists());
    assert!(!root.join("unexpected-open-trace").exists());
    assert!(!root.join("cache/st3/stui/runs").exists());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn empty_history_needs_no_person_daemon_or_terminal_and_creates_nothing() {
    let root = tempfile::tempdir().unwrap();
    assert!(history(root.path()).contains("Local stui quality"));
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn history_prints_every_fixed_measurement_and_its_historical_targets() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("cache/st3/stui/slo");
    std::fs::create_dir_all(&directory).unwrap();
    let fixture = include_str!("../../stui/tests/fixtures/quality-hour.json");
    let file = directory.join("0.jsonl");
    std::fs::write(&file, fixture.replace('\n', "") + "\n").unwrap();
    let output = history(root.path());
    let line: serde_json::Value = serde_json::from_str(fixture).unwrap();
    for group in ["counters", "histograms", "targets"] {
        for key in line[group].as_object().unwrap().keys() {
            assert!(output.contains(key), "missing {key}");
        }
    }
    assert!(output.contains("host-test") && output.contains("build-test"));
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        fixture.replace('\n', "") + "\n"
    );
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
}
