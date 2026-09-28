#![cfg(unix)]

//! The observed state of one Claude seat across a turn, driven through `hooks/claude-observe.sh`
//! exactly as Claude runs it: one process per hook event, the event name as the argument, the
//! payload on stdin, and the wrapper's exported ownership in the environment. Each test replays a
//! hook sequence and reads the resulting `harness-state` record the st3 driver publishes.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;
use st2::harness_state::{self, Activity, BlockedOn};

const IDENTITY: &str = "Silber.cos";
const NATIVE_SESSION: &str = "5b0c7f0e-0000-4000-8000-000000000001";

struct Seat {
    _tmp: tempfile::TempDir,
    catalog: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    token: String,
    seq: u64,
}

impl Seat {
    /// A declared seat whose wrapper has claimed the record, as `run_controlled_paths` does before
    /// Claude starts: the hooks then write under the wrapper's exported token and sequence.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let agent_dir = catalog.join("agents/Silber/cos");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.kdl"),
            r#"agent "cos" {
  host "Silber"
  workspace "/tmp"
  env { ST_AGENT "Silber.cos" }
  command "claude"
}"#,
        )
        .unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(env!("CARGO_BIN_EXE_st2"), bin.join("st2")).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let token = harness_state::session_token();
        let seq = harness_state::claim(&agent_dir, IDENTITY, "claude", &token).unwrap();
        Self {
            _tmp: tmp,
            catalog,
            bin,
            home,
            agent_dir,
            token,
            seq,
        }
    }

    /// Run one top-level hook event.
    fn hook(&self, event: &str, fields: serde_json::Value) {
        let mut payload = json!({
            "session_id": NATIVE_SESSION,
            "hook_event_name": event,
            "cwd": "/tmp",
        });
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks/claude-observe.sh");
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new("bash")
            .arg(script)
            .arg(event)
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("CATALOG", &self.catalog)
            .env("ST_AGENT", IDENTITY)
            .env("ST2_CLAUDE_IDENTITY", IDENTITY)
            .env("ST2_CLAUDE_RUNTIME_ID", IDENTITY)
            .env("ST2_CLAUDE_SESSION", &self.token)
            .env("ST2_CLAUDE_SESSION_SEQ", self.seq.to_string())
            .env_remove("ST2_CLAUDE_RESUME_GENERATION")
            .env_remove("ST2_CLAUDE_EXPECTED_NATIVE_SESSION")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "{event} hook failed");
    }

    /// The same wrapper's periodic heartbeat.
    fn heartbeat(&self) {
        harness_state::Writer::new(&self.agent_dir, IDENTITY, "claude", Some(IDENTITY.into()))
            .with_ownership(self.token.clone(), self.seq)
            .heartbeat()
            .unwrap();
    }

    fn observed(&self) -> (Activity, BlockedOn, Option<String>) {
        let observed =
            harness_state::read(&harness_state::harness_state_path(&self.agent_dir), None)
                .expect("the hooks wrote a harness-state record");
        (observed.state, observed.blocked_on, observed.reason)
    }

    fn start_turn(&self) {
        self.hook("SessionStart", json!({"source": "startup"}));
        assert_eq!(self.observed().0, Activity::Idle, "a fresh session is idle");
        self.hook("UserPromptSubmit", json!({"prompt": "run the suite"}));
    }
}

fn bash_call(id: &str, command: &str, background: bool) -> serde_json::Value {
    json!({
        "tool_name": "Bash",
        "tool_use_id": id,
        "tool_input": {"command": command, "run_in_background": background},
    })
}

/// A long foreground tool call and a subagent's whole lifecycle happen inside one parent turn. The
/// subagent's own hook events carry its `agent_id` and must never move the parent's state; the
/// parent stays working until its own `Stop`, and only then reads idle.
#[test]
fn subagents_and_long_tool_calls_keep_the_parent_turn_working_until_its_stop() {
    let seat = Seat::new();
    seat.start_turn();
    assert_eq!(seat.observed().0, Activity::Active);

    seat.hook("PreToolUse", bash_call("toolu_long", "cargo test", false));
    assert_eq!(
        seat.observed().0,
        Activity::Active,
        "a running tool call is work"
    );
    seat.hook("PostToolUse", bash_call("toolu_long", "cargo test", false));

    seat.hook(
        "PreToolUse",
        json!({"tool_name": "Task", "tool_use_id": "toolu_task", "tool_input": {"prompt": "x"}}),
    );
    let subagent = |fields: serde_json::Value| {
        let mut fields = fields;
        fields["agent_id"] = json!("a1b2c3");
        fields["agent_type"] = json!("general-purpose");
        fields
    };
    seat.hook("PreToolUse", subagent(bash_call("toolu_sub", "ls", false)));
    seat.hook("PostToolUse", subagent(bash_call("toolu_sub", "ls", false)));
    seat.hook("Stop", subagent(json!({})));
    assert_eq!(
        seat.observed(),
        (Activity::Active, BlockedOn::None, None),
        "a subagent finishing is not the parent finishing"
    );
    seat.hook(
        "PostToolUse",
        json!({"tool_name": "Task", "tool_use_id": "toolu_task", "tool_input": {"prompt": "x"}}),
    );
    assert_eq!(seat.observed().0, Activity::Active);

    seat.hook("Stop", json!({"stop_hook_active": false}));
    assert_eq!(
        seat.observed().0,
        Activity::Idle,
        "the parent's own Stop ends the turn"
    );
}

/// A permission prompt that is granted: blocked while the prompt is up, cleared by the granted
/// call's own completion, idle at the turn's end.
#[test]
fn a_granted_permission_prompt_is_blocked_then_clears_and_the_turn_ends_idle() {
    let seat = Seat::new();
    seat.start_turn();
    seat.hook("PreToolUse", bash_call("toolu_rm", "rm -rf build", false));
    seat.hook(
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}),
    );
    assert_eq!(
        seat.observed().1,
        BlockedOn::Human,
        "a prompt up is a human wait"
    );
    seat.hook("PostToolUse", bash_call("toolu_rm", "rm -rf build", false));
    assert_eq!(seat.observed(), (Activity::Active, BlockedOn::None, None));
    seat.hook("Stop", json!({"stop_hook_active": false}));
    assert_eq!(seat.observed().0, Activity::Idle);
    assert_eq!(seat.observed().1, BlockedOn::None);
}

/// Claude compacts automatically inside a running turn and then carries on with it. It reports
/// that compaction on three edges: `PreCompact`, `PostCompact`, and a `SessionStart` whose
/// `source` is `compact`. The turn has not ended, so the seat must still read working.
#[test]
#[ignore = "fails on main: SessionStart(source=compact) inside a running turn records the seat idle"]
fn an_automatic_compaction_inside_a_running_turn_keeps_the_seat_working() {
    let seat = Seat::new();
    seat.start_turn();
    seat.hook("PreToolUse", bash_call("toolu_a", "cargo build", false));
    seat.hook("PostToolUse", bash_call("toolu_a", "cargo build", false));
    assert_eq!(seat.observed().0, Activity::Active);

    seat.hook(
        "PreCompact",
        json!({"trigger": "auto", "custom_instructions": ""}),
    );
    seat.hook("PostCompact", json!({"trigger": "auto"}));
    seat.hook("SessionStart", json!({"source": "compact"}));

    let (state, _, reason) = seat.observed();
    assert_eq!(
        state,
        Activity::Active,
        "the turn is still running after its automatic compaction, but the seat reads {state:?} \
         ({reason:?})"
    );
}

/// A denied permission prompt ends the turn with no further hook event: no `Stop`, and no
/// `PermissionDenied` even when one is registered (the sequence the Claude driver documents
/// beside its hook mapping). Once the turn is over the seat is no longer waiting on a person and
/// is not working; it must not stay reported as blocked while the wrapper keeps the record fresh.
#[test]
#[ignore = "fails on main: after a denied permission prompt the seat stays working and blocked on a human"]
fn a_denied_permission_prompt_does_not_leave_the_seat_blocked_after_its_turn_ends() {
    let seat = Seat::new();
    seat.start_turn();
    seat.hook("PreToolUse", bash_call("toolu_rm", "rm -rf build", false));
    seat.hook(
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}),
    );
    assert_eq!(seat.observed().1, BlockedOn::Human);

    // The person answers "No". The turn ends; Claude fires no hook. The wrapper heartbeats.
    seat.heartbeat();

    let (state, blocked_on, reason) = seat.observed();
    assert!(
        blocked_on != BlockedOn::Human && state != Activity::Active,
        "the denied turn is over, but the seat still reads {state:?}/{blocked_on:?} ({reason:?})"
    );
}

/// A turn that ends while a shell it started in the background is still running. The agent's
/// own work is still in flight, so the seat must not read idle until that work is done.
#[test]
#[ignore = "fails on main: Stop records the seat idle while a background shell it started still runs"]
fn a_turn_that_leaves_its_own_background_shell_running_is_not_reported_idle() {
    let seat = Seat::new();
    seat.start_turn();
    seat.hook(
        "PreToolUse",
        bash_call("toolu_bg", "cargo test --all", true),
    );
    seat.hook(
        "PostToolUse",
        json!({
            "tool_name": "Bash",
            "tool_use_id": "toolu_bg",
            "tool_input": {"command": "cargo test --all", "run_in_background": true},
            "tool_response": {"backgroundTaskId": "bash_1", "stdout": "", "stderr": ""},
        }),
    );
    seat.hook("Stop", json!({"stop_hook_active": false}));

    let (state, _, _) = seat.observed();
    assert_ne!(
        state,
        Activity::Idle,
        "the background shell the turn started is still running"
    );
}
