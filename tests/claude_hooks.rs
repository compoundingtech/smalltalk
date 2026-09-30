#![cfg(unix)]

//! The Claude hooks, exercised as the harness runs them. These assert the channel the payload
//! arrives on, not merely that a payload was produced: the defect these cover shipped a hook that
//! emitted a byte-perfect payload on a channel the model never sees, so "the hook printed it" and
//! "the model received it" are exactly the two things that must not be conflated.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use st2::context;

fn bash() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("bash"))
        .find(|path| path.is_file())
        .expect("bash is available on PATH")
}

fn jq_available() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

struct Fixture {
    _tmp: tempfile::TempDir,
    catalog: PathBuf,
    context: PathBuf,
    bin: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let agent = catalog.join("agents/ExampleMac/cos");
        let context = context::context_dir(&agent);
        fs::create_dir_all(&context).unwrap();
        fs::write(
            agent.join("agent.kdl"),
            r#"agent "cos" {
  host "ExampleMac"
  workspace "/tmp"
  env { ST_AGENT "ExampleMac.cos" }
  command "claude"
}"#,
        )
        .unwrap();

        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(env!("CARGO_BIN_EXE_st2"), bin.join("st2")).unwrap();
        let state = tmp.path().join("state");

        Self {
            _tmp: tmp,
            catalog,
            context,
            bin,
            state,
        }
    }

    fn run(&self, script: &str) -> Output {
        self.run_with(script, &[])
    }

    /// `overrides` are applied last, so a test can drop `PATH` entries or retune staleness.
    fn run_with(&self, script: &str, overrides: &[(&str, &str)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("hooks")
            .join(script);
        let current_path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new(bash());
        command
            .arg(script)
            .env("PATH", format!("{}:{current_path}", self.bin.display()))
            .env("ST_ROOT", &self.catalog)
            .env("CATALOG", &self.catalog)
            .env("ST_AGENT", "ExampleMac.cos")
            .env("XDG_STATE_HOME", &self.state);
        for (key, value) in overrides {
            command.env(key, value);
        }
        command.output().unwrap()
    }
}

/// The payload must arrive as `additionalContext` on stdout. An earlier revision of this hook wrote
/// the identical bytes to stderr and exited 2, which the harness renders as a hook error and never
/// hands to the model — so `stderr.is_empty()` and `status == 0` are the load-bearing assertions
/// here, not the presence of the text.
#[test]
fn session_start_delivers_context_on_the_supported_channel_and_never_on_stderr() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Claude hook");
        return;
    }
    let fixture = Fixture::new();
    context::write_now(&fixture.context, "rehydration canary PELICAN-7742\n").unwrap();

    let output = fixture.run("claude-session-start.sh");

    assert!(
        output.status.success(),
        "the hook must exit 0; exit 2 is rendered as a hook error and discarded. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "nothing may go to stderr — it is the channel the model cannot see. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let additional = json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional.contains("rehydration canary PELICAN-7742"));
    assert!(additional.contains(r#"<context source="st2/context/now.md" agent="ExampleMac.cos">"#));
    assert!(additional.contains("Run the st2 boot ritual"));
    assert!(additional.contains("set your status to busy"));
}

#[test]
fn session_start_delivers_context_larger_than_the_platform_argument_limit() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Claude hook");
        return;
    }
    let fixture = Fixture::new();
    let large_context = "x".repeat(256 * 1024);
    context::write_now(&fixture.context, &large_context).unwrap();

    let output = fixture.run("claude-session-start.sh");

    assert!(
        output.status.success(),
        "large durable context must not cross an argv boundary. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let additional = json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        additional.contains(&large_context),
        "the complete durable context must reach Claude without truncation"
    );
}

/// Missing durable state is an ordinary cold start: the ritual still has to reach the model, and the
/// context envelope must be absent rather than empty.
#[test]
fn session_start_still_delivers_the_ritual_when_no_context_exists() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Claude hook");
        return;
    }
    let fixture = Fixture::new();

    let output = fixture.run("claude-session-start.sh");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let additional = json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional.contains("Run the st2 boot ritual"));
    assert!(
        !additional.contains("<context"),
        "no state means no envelope, not an empty one: {additional}"
    );
}

/// `--fresh-within` is a staleness gate, so context older than the window must not be restored —
/// otherwise a replacement session resumes an objective its predecessor already abandoned.
#[test]
fn session_start_omits_context_that_fails_the_staleness_gate() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Claude hook");
        return;
    }
    let fixture = Fixture::new();
    context::write_now(&fixture.context, "stale objective\n").unwrap();

    let output = fixture.run_with("claude-session-start.sh", &[("ST_REHYDRATE_STALE_S", "0")]);

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let additional = json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!additional.contains("stale objective"), "{additional}");
    assert!(additional.contains("Run the st2 boot ritual"));
}

/// Fail-open: lifecycle enrichment must never prevent Claude from starting.
#[test]
fn session_start_fails_open_without_required_commands() {
    let fixture = Fixture::new();

    let output = fixture.run_with("claude-session-start.sh", &[("PATH", "")]);

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}
