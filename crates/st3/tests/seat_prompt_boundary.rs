//! A seat's terminal belongs to its harness and to the person attached to it.
//!
//! st3 hands a seat its graph messages and work wakes through the harness's own channel, never by
//! typing into the seat's terminal, so a delivery can neither answer a dialog the seat is showing
//! nor merge into text a person is composing. The Claude channel hands each inbox message to the
//! session once, as its own notification, and only after the session has loaded the channel.
//!
//! The runtime context-clear control is the one st3 action that does type into a seat: it writes
//! `/clear` and Return. The two ignored tests hold it to the same boundary.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use st3::api::AppState;
use st3::model::{ClaimInput, IntentInput, MemberSpec, MissionRunRequest};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

const PERMISSION_DIALOG: &str = "\
╭─ Claude Code ─╮
 Bash command

   rm -rf build

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for rm commands in this project
   3. No, and tell Claude what to do differently (esc)
";

fn apply_source(store: &Store, source: &str, key: &str) {
    let intent = st3::parse_intent(source, "node").unwrap();
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &mission.subject_tokens, key).unwrap();
}

fn member(store: &Store, subject: &str) -> MemberSpec {
    store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject == subject)
        .and_then(|desired| desired.member)
        .unwrap()
}

fn observe_harness(store: &Store, subject: &str, key: &str, fields: &[(&str, &str)]) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Value::String((*value).to_owned())))
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })
        .unwrap();
}

/// Records every runtime action, including any key sent to a terminal.
#[derive(Default)]
struct RecordingRuntime {
    ptys: Mutex<Vec<RuntimeObservation>>,
    screen: Mutex<String>,
    keys: Mutex<Vec<(String, String)>>,
}

impl RuntimeControl for RecordingRuntime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<RuntimeObservation>> {
        Ok(self.ptys.lock().unwrap().clone())
    }
    fn observe_exec(&self, _runtime_id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _member: &MemberSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove(&self, _: &str, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn attach(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(self.screen.lock().unwrap().clone())
    }
    fn send_key(&self, runtime_id: &str, key: &str) -> anyhow::Result<()> {
        self.keys
            .lock()
            .unwrap()
            .push((runtime_id.into(), key.into()));
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// A seat showing a permission dialog gets its work wake as a graph message, and the reconciler
/// never sends a key to its terminal: nothing st3 delivers can press the dialog's focused button.
#[test]
fn a_work_wake_for_a_seat_at_a_permission_dialog_is_a_graph_message_not_terminal_input() {
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().display().to_string();
    let store = Arc::new(Store::open_memory("node").unwrap());
    let source = format!(
        "version 2\n\
         agent \"seat\" {{ workspace {workspace:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n\
         mission \"dialog-wake\" state=\"ready\" {{\n\
           goal \"Do the work.\"\n\
           step \"work\" {{\n\
             assigned-to \"agent/node.seat\"\n\
             title \"Build the change\"\n\
             goal \"Build it.\"\n\
           }}\n\
         }}\n"
    );
    apply_source(&store, &source, "dialog-wake");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "dialog-wake".into(),
            revision: None,
            workspace: workspace.clone(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "dialog-wake-run".into(),
        })
        .unwrap();
    let seat = "agent/node.seat";
    let runtime_id = member(&store, seat).runtime_id;
    let runtime = Arc::new(RecordingRuntime::default());
    *runtime.screen.lock().unwrap() = "╭─ Claude Code ─╮\n> ".into();
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.reconcile_once().unwrap();
    runtime.ptys.lock().unwrap().push(RuntimeObservation {
        runtime_id: runtime_id.clone(),
        terminal: true,
        status: "running".into(),
        exit_code: None,
        incarnation_id: Some("seat-one".into()),
    });
    reconciler.reconcile_once().unwrap();

    // The seat stops at a permission dialog: the screen shows it and the hooks report it.
    *runtime.screen.lock().unwrap() = PERMISSION_DIALOG.into();
    observe_harness(
        &store,
        seat,
        "seat-one-permission",
        &[
            ("state", "working"),
            ("driver", "claude"),
            ("transport", "claude-channel"),
            ("blocked_on", "human"),
            ("ask", "permission"),
            ("input_buffer", "unknown"),
            ("incarnation_id", "seat-one"),
        ],
    );
    for _ in 0..3 {
        reconciler.reconcile_once().unwrap();
    }
    assert!(
        runtime.keys.lock().unwrap().is_empty(),
        "a seat at a dialog received terminal input: {:?}",
        runtime.keys.lock().unwrap()
    );

    // The person answers; the seat is ready and its wake arrives as a message.
    *runtime.screen.lock().unwrap() = "╭─ Claude Code ─╮\n> ".into();
    observe_harness(
        &store,
        seat,
        "seat-one-ready",
        &[
            ("state", "ready"),
            ("driver", "claude"),
            ("transport", "claude-channel"),
            ("blocked_on", "none"),
            ("ask", "none"),
            ("incarnation_id", "seat-one"),
        ],
    );
    for _ in 0..3 {
        reconciler.reconcile_once().unwrap();
    }
    let messages = store.messages(Some(seat), true).unwrap();
    assert!(
        !messages.is_empty(),
        "the ready seat was never woken for its step"
    );
    for message in &messages {
        assert_eq!(message.from, "daemon/runtime");
        assert_eq!(message.to, seat);
        assert!(
            message
                .content
                .contains(&format!("work claim {}", run.steps[0].subject)),
            "{}",
            message.content
        );
    }
    assert!(
        runtime.keys.lock().unwrap().is_empty(),
        "a work wake was typed into the seat's terminal: {:?}",
        runtime.keys.lock().unwrap()
    );
}

struct Channel {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Channel {
    fn start(state: &Path, home: &Path, subject: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_st3"))
            .args(["driver", "claude-mcp", "--subject", subject])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("ST3_DRIVER_STATE_DIR", state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, frame: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{frame}").unwrap();
        stdin.flush().unwrap();
    }

    /// Every frame the channel writes within `window`.
    fn frames_for(&self, window: Duration) -> Vec<Value> {
        let deadline = Instant::now() + window;
        let mut frames = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => frames.push(serde_json::from_str(&line).unwrap()),
                Err(RecvTimeoutError::Timeout) => return frames,
                Err(RecvTimeoutError::Disconnected) => return frames,
            }
        }
    }

    /// Frames up to and including the first channel notification, or all frames at the deadline.
    fn until_notification(&self, limit: Duration) -> Vec<Value> {
        let deadline = Instant::now() + limit;
        let mut frames = Vec::new();
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            let frame: Value = serde_json::from_str(&line).unwrap();
            let notification = is_channel_notification(&frame);
            frames.push(frame);
            if notification {
                break;
            }
        }
        frames
    }

    fn close(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!("the channel did not end when its session closed stdin");
    }
}

fn is_channel_notification(frame: &Value) -> bool {
    frame["method"] == "notifications/claude/channel"
}

fn notified_filename(frame: &Value) -> String {
    frame["params"]["meta"]["messageFilename"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn channel_inbox(state: &Path, subject: &str) -> PathBuf {
    let identity = subject.strip_prefix("agent/").unwrap();
    let root = state.join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24]);
    let agent_dir = root
        .join("catalog")
        .join("agents")
        .join(st2::run::detect_host())
        .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16]);
    st2::message::inbox_dir(&agent_dir)
}

/// The Claude channel is Claude's own MCP pipe, not the seat's terminal. A session that has not
/// loaded the channel yet, such as one still at a startup dialog, receives nothing; after that each
/// inbox message is handed over exactly once, as its own notification, carrying the exact inbox
/// filename that the delivery receipt is correlated with.
#[test]
fn the_claude_channel_hands_each_message_over_once_and_only_after_the_session_loads_it() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("driver-state");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let subject = "agent/seat";
    let inbox = channel_inbox(&state, subject);
    let body = "First line of the request.\n\nSecond line: run `cargo test` and report != results.";
    let first = st2::message::send_to_inbox(
        &inbox,
        "agent/peer",
        Some("Two-line request"),
        None,
        &["st3-message:message/0123456789abcdef".into()],
        body,
    )
    .unwrap();

    let mut channel = Channel::start(&state, &home, subject);
    channel.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2025-06-18"}
    }));
    let before_initialized = channel.frames_for(Duration::from_millis(1_500));
    assert_eq!(
        before_initialized.len(),
        1,
        "only the initialize response precedes initialization: {before_initialized:?}"
    );
    assert_eq!(before_initialized[0]["id"], 1);
    assert!(!before_initialized.iter().any(is_channel_notification));

    channel.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let frames = channel.until_notification(Duration::from_secs(10));
    let notification = frames
        .iter()
        .find(|frame| is_channel_notification(frame))
        .expect("the loaded session was handed its message");
    assert_eq!(notified_filename(notification), first);
    let content = notification["params"]["content"].as_str().unwrap();
    assert!(
        content.starts_with(&format!(
            "[st3-delivery:{first}]\n[PING from st3] message/0123456789abcdef from agent/peer: Two-line request\n\n"
        )),
        "{content}"
    );
    assert!(content.contains("run `cargo test` and report != results."));

    let second = st2::message::send_to_inbox(
        &inbox,
        "agent/peer",
        Some("Follow-up"),
        None,
        &["st3-message:message/fedcba9876543210".into()],
        "A second, separate request.",
    )
    .unwrap();
    let later = channel.frames_for(Duration::from_millis(2_500));
    let notified = later
        .iter()
        .filter(|frame| is_channel_notification(frame))
        .map(notified_filename)
        .collect::<Vec<_>>();
    assert_eq!(
        notified,
        [second.clone()],
        "the first message was handed over again, or the second was not handed over alone"
    );
    let follow_up = later
        .iter()
        .find(|frame| is_channel_notification(frame))
        .unwrap()["params"]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(follow_up.starts_with(&format!("[st3-delivery:{second}]\n")));
    assert!(!follow_up.contains("Two-line request"));
    channel.close();
}

fn fake_pty(root: &Path, runtime_id: &str, log: &Path) -> PathBuf {
    let path = root.join("fake-pty");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = list ]; then\n  printf '[{{\"name\":\"{runtime_id}\",\"status\":\"running\",\"pid\":42,\"createdAt\":\"now\"}}]'\nfi\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A running Claude seat owned by this node, with its terminal served by a recording `pty`.
fn live_claude_seat(root: &Path) -> (AppState, String, PathBuf) {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        &format!(
            "version 2\nagent \"seat\" {{ workspace {:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n",
            workspace.display().to_string()
        ),
        "context-clear-seat",
    );
    let seat = "agent/node.seat".to_owned();
    let runtime_id = member(&store, &seat).runtime_id;
    store
        .append_claim(&ClaimInput {
            subject: seat.clone(),
            kind: "runtime.observed".into(),
            actor: Some(seat.clone()),
            fields: BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String(runtime_id.clone())),
                ("incarnation_id".into(), Value::String("42:now".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("context-clear-seat-running".into()),
        })
        .unwrap();
    let log = root.join("pty.log");
    let binary = fake_pty(root, &runtime_id, &log);
    let state = AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "node".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: binary,
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    };
    (state, seat, log)
}

async fn clear_context(state: AppState, seat: &str) -> (StatusCode, Value) {
    let response = st3::api::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/sessions/{}/context/clear",
                    seat.replace('/', "%2F")
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"expected_incarnation": "42:now", "idempotency_key": "clear-1"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn terminal_writes(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("send "))
        .map(str::to_owned)
        .collect()
}

/// The recording `pty` really serves the control: context clear reaches the seat's terminal.
#[tokio::test]
async fn context_clear_writes_its_command_through_the_seat_terminal() {
    let root = tempfile::tempdir().unwrap();
    let (state, seat, log) = live_claude_seat(root.path());
    let (status, body) = clear_context(state, &seat).await;
    assert!(status.is_success(), "{status}: {body}");
    let writes = terminal_writes(&log);
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].ends_with("--seq /clear --seq key:return"), "{writes:?}");
}

/// A seat at a permission dialog must not receive `/clear` and Return: Return presses the dialog's
/// focused option, approving the tool call instead of clearing the context.
#[tokio::test]
#[ignore = "fails on main: context clear types /clear and Return into a seat that is waiting on a person"]
async fn context_clear_is_not_typed_into_a_seat_waiting_on_a_person() {
    let root = tempfile::tempdir().unwrap();
    let (state, seat, log) = live_claude_seat(root.path());
    observe_harness(
        &state.store,
        &seat,
        "seat-permission",
        &[
            ("state", "working"),
            ("driver", "claude"),
            ("transport", "claude-channel"),
            ("blocked_on", "human"),
            ("ask", "permission"),
            ("input_buffer", "unknown"),
            ("incarnation_id", "42:now"),
        ],
    );
    let (status, body) = clear_context(state, &seat).await;
    let writes = terminal_writes(&log);
    assert!(
        writes.is_empty(),
        "context clear typed into a seat waiting on a person ({status}: {body}): {writes:?}"
    );
}

/// A seat whose composer holds a person's unsent draft must not receive `/clear` and Return: the
/// command lands after the draft and Return submits both as one prompt.
#[tokio::test]
#[ignore = "fails on main: context clear types /clear and Return over a draft in the composer"]
async fn context_clear_is_not_typed_over_a_draft_in_the_composer() {
    let root = tempfile::tempdir().unwrap();
    let (state, seat, log) = live_claude_seat(root.path());
    observe_harness(
        &state.store,
        &seat,
        "seat-draft",
        &[
            ("state", "idle"),
            ("driver", "claude"),
            ("transport", "claude-channel"),
            ("blocked_on", "none"),
            ("ask", "none"),
            ("input_buffer", "nonempty"),
            ("incarnation_id", "42:now"),
        ],
    );
    let (status, body) = clear_context(state, &seat).await;
    let writes = terminal_writes(&log);
    assert!(
        writes.is_empty(),
        "context clear typed over a draft in the composer ({status}: {body}): {writes:?}"
    );
}
