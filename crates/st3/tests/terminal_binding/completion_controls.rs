//! Forced schedules use a real provider, Unix API, Store and borrowed shell.
use super::*;

async fn wait_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing control barrier {}", path.display()));
}

fn title_owner(root: &std::path::Path) -> st3::mailbox::Fence {
    let db = rusqlite::Connection::open(root.join("graph.db")).unwrap();
    db.query_row("SELECT b.incarnation,b.epoch,b.token FROM local_mailbox_owners o
        JOIN local_mailbox_bindings b ON b.subject=o.subject AND b.component=o.component
        AND b.incarnation=o.incarnation AND b.epoch=o.epoch WHERE o.subject=?1 AND o.component='title'",
        [SEAT], |row| Ok(st3::mailbox::Fence {
            subject: SEAT.into(), component: "title".into(), incarnation: row.get(0)?, epoch: row.get(1)?, token: row.get(2)?,
        })).unwrap()
}

#[tokio::test]
async fn provider_success_before_join_finish() {
    completion_control(Some("before"), 0, None).await;
}
#[tokio::test]
async fn provider_success_after_join_finish() {
    completion_control(Some("after"), 0, None).await;
}
#[tokio::test]
async fn provider_failure_before_join_finish() {
    completion_control(Some("before"), 7, None).await;
}
#[tokio::test]
async fn provider_failure_after_join_finish() {
    completion_control(Some("after"), 7, None).await;
}
#[tokio::test]
async fn real_runtime_exit_remains_fenced() {
    completion_control(None, 0, Some("runtime")).await;
}
#[tokio::test]
async fn real_token_replacement_remains_fenced() {
    completion_control(None, 0, Some("token")).await;
}
#[tokio::test]
async fn real_invocation_replacement_remains_fenced() {
    completion_control(None, 0, Some("replacement")).await;
}

#[tokio::test]
async fn foreign_runtime_after_terminal_receipt_remains_fenced() {
    completion_control(Some("before"), 0, Some("runtime")).await;
}
#[tokio::test]
async fn foreign_token_after_terminal_receipt_remains_fenced() {
    completion_control(Some("before"), 0, Some("token")).await;
}
#[tokio::test]
async fn foreign_invocation_after_terminal_receipt_remains_fenced() {
    completion_control(Some("before"), 0, Some("replacement")).await;
}

fn reject_binding(
    store: &Arc<Store>,
    runtime: &Arc<Runtime>,
    root: &std::path::Path,
    physical: &str,
    incarnation: &str,
    rejection: &str,
) -> st3::mailbox::Fence {
    let owner = title_owner(root);
    assert!(st3::test_support::check_fixture_mailbox(store, &owner).is_ok());
    if rejection == "token" {
        let request = st3::mailbox::Fence::new(SEAT, incarnation, "title");
        let successor = st3::test_support::bind_fixture_mailbox(store, &request).unwrap();
        assert_eq!(
            st3::test_support::check_fixture_mailbox(store, &owner)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
        // Forced raw owner displacement is foreign to the durable native lease.
        // It fences the old wrapper, but does not authenticate this replacement token.
        assert!(st3::test_support::check_fixture_mailbox(store, &successor).is_err());
        successor
    } else {
        if rejection == "runtime" {
            claim(
                store,
                "runtime.observed",
                json!({"status":"exited", "incarnation_id":incarnation}),
            );
        } else {
            apply(
                store,
                &source(NEXT).replace("shell:created", physical),
                Some("person/avery"),
                "replace",
            )
            .unwrap();
            reconcile(store, runtime);
        }
        assert_eq!(
            st3::test_support::check_fixture_mailbox(store, &owner)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
        owner
    }
}

async fn completion_control(order: Option<&str>, exit: u8, rejection: Option<&str>) {
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
import json, os, subprocess, sys, time
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
if os.environ.get('FIXTURE_EXIT_GATE'):
    while not Path(os.environ['FIXTURE_EXIT_GATE']).exists():
        time.sleep(0.005)
sys.exit(int(os.environ.get('FIXTURE_PROVIDER_EXIT', '0')))
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
    let barrier = root.path().join("completion-control");
    std::fs::create_dir(&barrier).unwrap();
    if let Some(order) = order {
        std::fs::write(barrier.join("order"), order).unwrap();
        command.env("ST3_FIXTURE_TERMINAL_COMPLETION", &barrier);
    }
    command.env("FIXTURE_PROVIDER_EXIT", exit.to_string());
    if rejection.is_some() && order.is_none() {
        command.env("FIXTURE_EXIT_GATE", barrier.join("exit-provider"));
        command.env("ST3_FIXTURE_TERMINAL_COMPLETION", &barrier);
    }
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
    let mut rejection_fence = if let Some(rejection) = rejection.filter(|_| order.is_none()) {
        wait_file(&received).await;
        // Reject an established stream, rather than racing its initial HTTP admission.
        wait_file(&barrier.join("title-stream-admitted")).await;
        Some(reject_binding(
            &store,
            &runtime,
            root.path(),
            &physical,
            &agent_incarnation,
            rejection,
        ))
    } else {
        None
    };
    let mut completion_fence = None;
    if let Some(order) = order {
        wait_file(&barrier.join("provider-return.json")).await;
        let outcome: Value =
            serde_json::from_slice(&std::fs::read(barrier.join("provider-return.json")).unwrap())
                .unwrap();
        assert_eq!(
            outcome["ok"],
            exit == 0,
            "actual provider result: {outcome}"
        );
        if exit != 0 {
            assert!(
                outcome["error"]
                    .as_str()
                    .unwrap()
                    .contains("exit status: 7"),
                "{outcome}"
            );
        }
        wait_file(&barrier.join("observation-drained")).await;
        if order == "after" {
            std::fs::write(barrier.join("release-provider"), b"go").unwrap();
        }
        wait_file(&barrier.join("join-phase")).await;
        assert_eq!(
            std::fs::read_to_string(barrier.join("join-phase")).unwrap(),
            if order == "after" {
                "finished"
            } else {
                "pending"
            }
        );
        // Before the fix, ended was already admitted and Store must deny the binding.
        // With the publication hold, the current binding remains valid until the actual
        // task result is disposed. Both paths must deny it after final exit publication.
        let fence = title_owner(root.path());
        let deferred =
            std::fs::read_to_string(barrier.join("observation-drained")).unwrap() == "deferred";
        if deferred {
            assert!(st3::test_support::check_fixture_mailbox(&store, &fence).is_ok());
        } else {
            assert_eq!(
                st3::test_support::check_fixture_mailbox(&store, &fence)
                    .unwrap_err()
                    .code,
                "stale-mailbox-session"
            );
        }
        completion_fence = Some(fence);
        if let Some(rejection) = rejection {
            assert!(
                deferred,
                "foreign fence control requires the terminal publication hold"
            );
            wait_file(&barrier.join("title-stream-admitted")).await;
            rejection_fence = Some(reject_binding(
                &store,
                &runtime,
                root.path(),
                &physical,
                &agent_incarnation,
                rejection,
            ));
        }
        std::fs::write(barrier.join("poll-driver"), b"go").unwrap();
        if deferred && rejection.is_none() {
            wait_file(&barrier.join("awaiting-completion")).await;
        } else {
            wait_file(&barrier.join("fence-received")).await;
            assert_eq!(
                std::fs::read_to_string(barrier.join("fence-received")).unwrap(),
                if order == "after" {
                    "finished"
                } else {
                    "pending"
                }
            );
        }
        std::fs::write(barrier.join("release-provider"), b"go").unwrap();
    }
    if let Some(owner) = rejection_fence.as_ref().filter(|_| order.is_none()) {
        // The real rejection is accepted while the provider is still alive. Tokio waits
        // for a blocking provider worker on shutdown, so release it only after that proof.
        wait_file(&barrier.join("fence-received")).await;
        assert_eq!(
            std::fs::read_to_string(barrier.join("fence-received")).unwrap(),
            "pending"
        );
        if rejection == Some("token") {
            assert!(st3::test_support::check_fixture_mailbox(&store, owner).is_err());
        }
        assert!(!barrier.join("provider-return.json").exists());
        std::fs::write(barrier.join("exit-provider"), b"go").unwrap();
    }
    let mut stdout = tokio::io::BufReader::new(shell.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let expected = if exit == 0 && rejection.is_none() {
        0
    } else {
        2
    };
    if line != format!("driver-exit={expected} shell-still-alive\n") {
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
    if let Some(fence) = completion_fence {
        assert_eq!(
            st3::test_support::check_fixture_mailbox(&store, &fence)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
    }
    if let Some(owner) = rejection_fence {
        assert!(shell.try_wait().unwrap().is_none());
        if rejection == Some("token") {
            assert!(st3::test_support::check_fixture_mailbox(&store, &owner).is_err());
        } else {
            assert_eq!(
                st3::test_support::check_fixture_mailbox(&store, &owner)
                    .unwrap_err()
                    .code,
                "stale-mailbox-session"
            );
        }
        assert!(runtime.actions.lock().unwrap().is_empty());
        std::fs::write(barrier.join("exit-provider"), b"go").unwrap();
        server.abort();
        shell.kill().await.unwrap();
        return;
    }
    // The wrapper reports its own exit under the bound invocation fence.
    assert_eq!(status(&store)["status"], "exited");
    assert_eq!(status(&store)["exit_code"], if exit == 0 { 0 } else { 1 });
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
