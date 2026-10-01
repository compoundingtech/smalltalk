#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn bash() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("bash"))
        .find(|path| path.is_file())
        .expect("bash is available on PATH")
}

use st2::{context, message};

struct Fixture {
    _tmp: tempfile::TempDir,
    catalog: PathBuf,
    context: PathBuf,
    inbox: PathBuf,
    bin: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let agent = catalog.join("agents/ExampleMac/cos");
        let context = context::context_dir(&agent);
        let inbox = message::inbox_dir(&agent);
        fs::create_dir_all(&context).unwrap();
        fs::create_dir_all(&inbox).unwrap();
        fs::write(
            agent.join("agent.kdl"),
            r#"agent "cos" {
  host "ExampleMac"
  workspace "/tmp"
  env { ST_AGENT "ExampleMac.cos" }
  command "codex"
  ding
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
            inbox,
            bin,
            state,
        }
    }

    fn run(&self, script: &str) -> Output {
        self.run_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("crates/st-drivers/hooks")
                .join(script),
        )
    }

    fn run_path(&self, script: &Path) -> Output {
        let current_path = std::env::var("PATH").unwrap_or_default();
        Command::new(bash())
            .arg(script)
            .env("PATH", format!("{}:{current_path}", self.bin.display()))
            .env("ST_ROOT", &self.catalog)
            .env("CATALOG", &self.catalog)
            .env("ST_AGENT", "ExampleMac.cos")
            .env("ST_HOOKS", self.state.join("st2/hooks"))
            .env("XDG_STATE_HOME", &self.state)
            .output()
            .unwrap()
    }

    fn install_hooks(&self) -> PathBuf {
        let output = Command::new(env!("CARGO_BIN_EXE_st2"))
            .args(["hooks", "install"])
            .env("ST_HOOKS", self.state.join("st2/hooks"))
            .env("XDG_STATE_HOME", &self.state)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        st2::hooks::versioned_hooks_dir_at(&self.state.join("st2/hooks"))
    }
}

fn jq_available() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn stop_reason(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let object = json.as_object().unwrap();
    assert_eq!(object.len(), 2);
    assert_eq!(json["decision"], "block");
    assert!(object.contains_key("reason"));
    assert!(!object.contains_key("continue"));
    assert!(!object.contains_key("hookSpecificOutput"));
    json["reason"].as_str().unwrap().to_string()
}

#[test]
fn session_start_emits_current_codex_context_envelope() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Codex hook");
        return;
    }
    let fixture = Fixture::new();
    context::write_now(&fixture.context, "working on the materializer\n").unwrap();
    message::send_to_inbox(
        &fixture.inbox,
        "ExampleMac.worker",
        Some("status"),
        None,
        &[],
        "done",
    )
    .unwrap();

    let output = fixture.run("codex-session-start.sh");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let additional = json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional.contains("working on the materializer"));
    assert!(additional.contains("## st2 inbox (1 unread)"));
    assert!(additional.contains("Run the st2 boot ritual"));
    assert!(additional.contains("set your status to busy"));
    assert!(additional.contains("set available only when yielding"));
}

#[test]
fn pre_compact_stubs_only_empty_context_and_never_clobbers_real_state() {
    let fixture = Fixture::new();
    let output = fixture.run("codex-pre-compact.sh");
    assert!(output.status.success());
    let stub = context::read(&fixture.context, context::View::Now);
    assert!(stub.contains("pre-compact stub"));

    context::write_now(&fixture.context, "real checkpoint\n").unwrap();
    let output = fixture.run("codex-pre-compact.sh");
    assert!(output.status.success());
    assert_eq!(
        context::read(&fixture.context, context::View::Now),
        "real checkpoint\n"
    );
}

#[test]
fn stop_uses_since_cursor_and_emits_only_new_messages() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Codex hook");
        return;
    }
    let fixture = Fixture::new();
    message::send_to_inbox(
        &fixture.inbox,
        "ExampleMac.worker",
        Some("first"),
        None,
        &[],
        "one",
    )
    .unwrap();

    let first = fixture.run("codex-stop.sh");
    assert!(stop_reason(&first).contains("first"));

    let quiet = fixture.run("codex-stop.sh");
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());
    assert!(quiet.stderr.is_empty());

    std::thread::sleep(Duration::from_millis(3));
    message::send_to_inbox(
        &fixture.inbox,
        "ExampleMac.worker",
        Some("second"),
        None,
        &[],
        "two",
    )
    .unwrap();
    let second = fixture.run("codex-stop.sh");
    let reason = stop_reason(&second);
    assert!(reason.contains("second"));
    assert!(!reason.contains("first"));
}

#[test]
fn installed_versioned_stop_hook_preserves_the_scope_a_envelope_and_cursor() {
    if !jq_available() {
        eprintln!("SKIP: jq is required by the shipped Codex hook");
        return;
    }
    let fixture = Fixture::new();
    let hooks = fixture.install_hooks();
    assert!(hooks.to_string_lossy().contains("/sets/sha256-"));
    message::send_to_inbox(
        &fixture.inbox,
        "ExampleMac.worker",
        Some("installed"),
        None,
        &[],
        "one",
    )
    .unwrap();

    let first = fixture.run_path(&hooks.join("codex-stop.sh"));
    assert!(stop_reason(&first).contains("installed"));
    let quiet = fixture.run_path(&hooks.join("codex-stop.sh"));
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());
    assert!(quiet.stderr.is_empty());
}

#[test]
fn stop_fails_open_without_required_commands() {
    let fixture = Fixture::new();
    let output = Command::new(bash())
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("crates/st-drivers/hooks")
                .join("codex-stop.sh"),
        )
        .env("PATH", fixture._tmp.path().join("missing-bin"))
        .env("ST_ROOT", &fixture.catalog)
        .env("CATALOG", &fixture.catalog)
        .env("ST_AGENT", "ExampleMac.cos")
        .env("ST_HOOKS", fixture.state.join("st2/hooks"))
        .env("XDG_STATE_HOME", &fixture.state)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
