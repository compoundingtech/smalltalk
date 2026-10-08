#![cfg(unix)]
//! Borrowed PTYs are tested through the runtime backend: every physical action is recorded.
use serde_json::{Value, json};
use st3::model::{ClaimInput, IntentInput, MemberSpec, MessageSendRequest, MessageView};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, watch};

const TERMINAL: &str = "pty/person/avery/019a0000-0000-7000-8000-000000000001";
const SEAT: &str = "agent/example/terminal-claude";
const INVOCATION: &str = "019a0000-0000-7000-8000-000000000002";
const NEXT: &str = "019a0000-0000-7000-8000-000000000003";

#[derive(Default)]
struct Runtime {
    ptys: Mutex<Vec<RuntimeObservation>>,
    actions: Mutex<Vec<String>>,
}
impl RuntimeControl for Runtime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<RuntimeObservation>> {
        Ok(self.ptys.lock().unwrap().clone())
    }
    fn observe_exec(&self, _: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, m: &MemberSpec) -> anyhow::Result<()> {
        self.actions
            .lock()
            .unwrap()
            .push(format!("start:{}", m.runtime_id));
        Ok(())
    }
    fn stop(&self, id: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        self.actions.lock().unwrap().push(format!("stop:{id}"));
        Ok(())
    }
    fn kill(&self, id: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        self.actions.lock().unwrap().push(format!("kill:{id}"));
        Ok(())
    }
    fn remove(&self, id: &str, _: bool) -> anyhow::Result<()> {
        self.actions.lock().unwrap().push(format!("remove:{id}"));
        Ok(())
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        self.actions.lock().unwrap().push("key".into());
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn end_leftovers(&self, _: &str, _: bool) {
        self.actions.lock().unwrap().push("leftovers".into());
    }
}
fn apply(store: &Store, source: &str, actor: Option<&str>, key: &str) -> Result<(), st3::St3Error> {
    let intent = st3::parse_intent(source, "orchid")?;
    let preview = store.mission(
        &intent,
        IntentInput {
            kdl: source.into(),
            source_name: None,
        },
    )?;
    store.apply_as(&intent, &preview.subject_tokens, key, actor)?;
    Ok(())
}
fn source(id: &str) -> String {
    format!(
        "version 2\nagent \"example/terminal-claude\" {{ name \"Terminal/claude\"; harness \"claude\" {{}}; bind-terminal {TERMINAL:?} incarnation=\"shell:created\" id={id:?}; }}"
    )
}
fn declare_terminal(store: &Store) {
    apply(
        store,
        &format!(
            "version 2\nterminal {:?} {{ command \"shell\"; restart \"never\"; }}",
            TERMINAL.strip_prefix("pty/").unwrap()
        ),
        Some("person/avery"),
        "terminal",
    )
    .unwrap();
}
fn runtime() -> Arc<Runtime> {
    Arc::new(Runtime {
        ptys: Mutex::new(vec![RuntimeObservation {
            runtime_id: TERMINAL.replace('/', "."),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("shell:created".into()),
        }]),
        ..Default::default()
    })
}
fn reconcile(store: &Arc<Store>, runtime: &Arc<Runtime>) {
    Reconciler::new(
        store.clone(),
        runtime.clone(),
        "orchid".into(),
        Arc::new(Notify::new()),
    )
    .reconcile_once()
    .unwrap();
}
fn incarnation(id: &str) -> String {
    format!("shell:created:bound:{id}")
}
fn claim(store: &Store, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: kind.into(),
            actor: Some(SEAT.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}
fn status(store: &Store) -> Value {
    {
        let value = store.latest_actual_value(SEAT).unwrap().unwrap();
        value.get("fields").unwrap_or(&value).clone()
    }
}

#[test]
fn only_the_terminal_owner_can_bind_and_the_target_must_exist_on_the_same_host() {
    let store = Store::open_memory("orchid").unwrap();
    declare_terminal(&store);
    for (i, actor) in [
        None,
        Some("person/intruder"),
        Some("agent/example/worker"),
        Some("daemon/runtime"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            apply(
                &store,
                &source(INVOCATION),
                actor,
                &format!("forbidden-{i}")
            )
            .unwrap_err()
            .code,
            "terminal-owner-forbidden"
        );
    }
    assert_eq!(
        apply(
            &store,
            &source(INVOCATION).replace("name \"Terminal/claude\"", "host \"elsewhere\""),
            Some("person/avery"),
            "wrong-host"
        )
        .unwrap_err()
        .code,
        "invalid-terminal-binding"
    );
    apply(&store, &source(INVOCATION), Some("person/avery"), "owner").unwrap();
    let absent = Store::open_memory("orchid").unwrap();
    assert_eq!(
        apply(&absent, &source(INVOCATION), Some("person/avery"), "absent")
            .unwrap_err()
            .code,
        "invalid-terminal-binding"
    );
}

#[test]
fn binding_forbids_managed_side_effects_and_requires_an_invocation_fence() {
    for additional in [
        "command \"true\";",
        "render { git-exclude \"scratch\"; }",
        "fresh-context;",
        "one-shot;",
        "exec \"helper\" { command \"true\"; }",
    ] {
        let source = source(INVOCATION).replacen("name \"Terminal/claude\";", additional, 1);
        assert!(st3::parse_intent(&source, "orchid").is_err(), "{source}");
    }
    assert!(st3::parse_intent(&source("not-a-uuid"), "orchid").is_err());
    let intent = st3::parse_intent(&source(INVOCATION), "orchid").unwrap();
    let member = intent.subjects[SEAT].member.as_ref().unwrap();
    assert_eq!(member.lifecycle, st3::model::MemberLifecycle::TerminalBound);
    assert_eq!(member.restart, st3::model::RestartType::Never);
}

#[test]
fn exits_stops_and_replaced_shells_never_act_on_the_borrowed_pty() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("graph.db");
    let store = Arc::new(Store::open(&path, "orchid").unwrap());
    declare_terminal(&store);
    apply(&store, &source(INVOCATION), Some("person/avery"), "bound").unwrap();
    let runtime = runtime();
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["adopted"], true);
    assert_eq!(status(&store)["incarnation_id"], incarnation(INVOCATION));
    claim(
        &store,
        "runtime.observed",
        json!({"status":"exited","incarnation_id":incarnation(INVOCATION),"exit_code":7}),
    );
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["status"], "exited");
    drop(store);
    let store = Arc::new(Store::open(&path, "orchid").unwrap());
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["status"], "exited");
    // The next invocation shares the shell but cannot inherit its predecessor's exit.
    apply(&store, &source(NEXT), Some("person/avery"), "next").unwrap();
    reconcile(&store, &runtime);
    claim(
        &store,
        "runtime.observed",
        json!({"status":"exited","incarnation_id":incarnation(INVOCATION),"exit_code":1}),
    );
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["status"], "running");
    assert_eq!(status(&store)["incarnation_id"], incarnation(NEXT));
    apply(
        &store,
        &format!("version 2\nstop {SEAT:?}"),
        Some("person/avery"),
        "stop",
    )
    .unwrap();
    Reconciler::new(
        store.clone(),
        runtime.clone(),
        "iris".into(),
        Arc::new(Notify::new()),
    )
    .reconcile_once()
    .unwrap();
    assert_eq!(status(&store)["status"], "running");
    assert_eq!(
        store.selected_actual_origin(SEAT).unwrap().as_deref(),
        Some("orchid")
    );
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["status"], "stopped");
    // A terminal with the same name but another process cannot satisfy the old binding.
    apply(
        &store,
        &source("019a0000-0000-7000-8000-000000000004"),
        Some("person/avery"),
        "again",
    )
    .unwrap();
    runtime.ptys.lock().unwrap()[0].incarnation_id = Some("replacement:created".into());
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["status"], "vanished");
    assert!(
        runtime.actions.lock().unwrap().is_empty(),
        "{:?}",
        runtime.actions.lock().unwrap()
    );
    assert_eq!(runtime.ptys.lock().unwrap()[0].status, "running");
}

#[tokio::test]
async fn fake_harness_receives_mail_and_exits_while_the_shell_survives_daemon_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("graph.db"), "orchid").unwrap());
    declare_terminal(&store);
    let runtime = runtime();
    let socket = root.path().join("api.sock");
    let state = st3::api::AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "orchid".into(),
        state_dir: root.path().into(),
        pty_root: root.path().join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix_bound(&server_socket, &server_socket, st3::api::router(state))
            .await
            .unwrap();
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let client = st3::client::Client::new(st3::client::Endpoint::Unix(socket));
    // The real native wrapper launches a model-free provider, which consumes one real
    // Claude channel notification and exits. All files live under this fixture's HOME.
    let provider = root.path().join("fake-claude.py");
    let received = root.path().join("received.json");
    std::fs::write(&provider, r#"
import json, os, subprocess
from pathlib import Path
channel = subprocess.Popen([os.environ['ST3_BIN'], 'driver', 'claude-mcp', '--subject', os.environ['ST_AGENT']],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
try:
    channel.stdin.write('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n')
    channel.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
    channel.stdin.flush()
    for line in channel.stdout:
        event = json.loads(line)
        if event.get('method') == 'notifications/claude/channel':
            Path(os.environ['FIXTURE_RECEIVED']).write_text(json.dumps(event))
            break
    else:
        raise RuntimeError('channel ended without mail')
finally:
    channel.stdin.close()
    channel.wait(timeout=10)
"#).unwrap();
    // Archives move between runners; choose this consumer's interpreter rather than
    // resolve a bare name through the producer's captured fixture PATH.
    let python = std::env::split_paths(&std::env::var_os("PATH").expect("fixture tool PATH"))
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("terminal provider fixture needs python3 on the executing runner");
    let mut command = st3::test_support::async_command(env!("ST3_FIXTURE_BASH"));
    command
        .env_clear()
        .env("PATH", env!("ST3_FIXTURE_PATH"))
        .env("HOME", root.path())
        .env("PTY_ROOT", root.path().join("pty"))
        .env("FIXTURE_SEAT", SEAT)
        .env("ST3_SUBJECT", TERMINAL)
        .env("ST3_ENDPOINT", root.path().join("api.sock"))
        .env("ST3_BIN", test_env!("CARGO_BIN_EXE_st3-fixture"))
        .env("ST3_DRIVER_STATE_DIR", root.path().join("drivers"))
        .env("ST3_MAILBOX_TRANSPORT", "push")
        .env("FIXTURE_RECEIVED", &received)
        .env("FIXTURE_PROVIDER", provider)
        .env("FIXTURE_PYTHON", python)
        .current_dir(root.path())
        .args(["--noprofile", "--norc", "-c", r#"
read -r _
ST_AGENT="$FIXTURE_SEAT" "$ST3_BIN" driver claude --subject "$FIXTURE_SEAT" -- "$FIXTURE_PYTHON" "$FIXTURE_PROVIDER"
driver_exit=$?
printf 'driver-exit=%s shell-still-alive\n' "$driver_exit"
read -r _
"#])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut shell = command.spawn().unwrap();
    let physical = format!("{}:created", shell.id().unwrap());
    let agent_incarnation = format!("{physical}:bound:{INVOCATION}");
    let pty_root = root.path().join("pty");
    std::fs::create_dir_all(&pty_root).unwrap();
    let runtime_id = TERMINAL.replace('/', ".");
    std::fs::write(
        pty_root.join(format!("{runtime_id}.pid")),
        shell.id().unwrap().to_string(),
    )
    .unwrap();
    std::fs::write(
        pty_root.join(format!("{runtime_id}.json")),
        json!({"createdAt":"created","tags":{"st3.subject":TERMINAL}}).to_string(),
    )
    .unwrap();
    runtime.ptys.lock().unwrap()[0].incarnation_id = Some(physical.clone());
    apply(
        &store,
        &source(INVOCATION).replace("shell:created", &physical),
        Some("person/avery"),
        "bind",
    )
    .unwrap();
    reconcile(&store, &runtime);
    claim(
        &store,
        "harness.observed",
        json!({"state":"ready","driver":"claude","incarnation_id":agent_incarnation}),
    );
    // A fresh reconciler and reopened graph adopt an active invocation without launching it.
    let reopened = Arc::new(Store::open(&root.path().join("graph.db"), "orchid").unwrap());
    reconcile(&reopened, &runtime);
    assert_eq!(status(&reopened)["incarnation_id"], agent_incarnation);
    drop(reopened);
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
    let mut stderr = shell.stderr.take().unwrap();
    let diagnostics = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    shell
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"launch\n")
        .await
        .unwrap();
    let sent: MessageView = client
        .post(
            "/v1/messages",
            &MessageSendRequest {
                idempotency_key: "mail".into(),
                from: "person/avery".into(),
                to: SEAT.into(),
                content: "hello from the terminal owner".into(),
                title: None,
                in_reply_to: None,
                tags: vec![],
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    let mut stdout = tokio::io::BufReader::new(shell.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    if line != "driver-exit=0 shell-still-alive\n" {
        shell.kill().await.unwrap();
        let logs = diagnostics.await.unwrap();
        panic!("{line}: {}", String::from_utf8_lossy(&logs));
    }
    assert!(
        shell.try_wait().unwrap().is_none(),
        "the harness must return to its shell"
    );
    let notification: Value = serde_json::from_slice(&std::fs::read(received).unwrap()).unwrap();
    assert!(
        notification["params"]["content"]
            .as_str()
            .unwrap()
            .contains(&sent.content)
    );
    assert_eq!(notification["params"]["meta"]["messageId"], sent.subject);
    // The wrapper reports its own exit under the bound invocation fence.
    assert_eq!(status(&store)["status"], "exited");
    assert_eq!(status(&store)["incarnation_id"], agent_incarnation);
    server.abort();
    let _ = server.await;
    drop(client);
    drop(store);
    let store = Arc::new(Store::open(&root.path().join("graph.db"), "orchid").unwrap());
    reconcile(&store, &runtime);
    assert_eq!(status(&store)["status"], "exited");
    assert_eq!(runtime.ptys.lock().unwrap()[0].status, "running");
    assert!(runtime.actions.lock().unwrap().is_empty());
    assert!(
        shell.try_wait().unwrap().is_none(),
        "the shell survives daemon restart"
    );
    shell.kill().await.unwrap();
    shell.wait().await.unwrap();
    let _ = diagnostics.await.unwrap();
}

#[path = "terminal_binding/completion_controls.rs"]
mod completion_controls;
