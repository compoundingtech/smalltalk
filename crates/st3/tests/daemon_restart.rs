#![cfg(unix)]
//! Restart an isolated daemon under running and starting seats.
//!
//! A deploy restarts the daemon while seats run. Their drivers and channels share the seat's
//! terminal, so they must wait the outage out silently, never exit for it, and resume from the
//! graph once the daemon is back. Each test here owns its daemon, socket, and state directories.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::ClaimInput;
use st3::store::Store;
use tokio::sync::{Notify, watch};

/// One isolated daemon whose API can stop and start again over the same durable graph.
struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    store: Arc<Store>,
    event_notify: watch::Sender<u64>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Daemon {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            socket: root.join("st3.sock"),
            store: Arc::new(Store::open(&root.join("daemon.sqlite3"), "restart-node").unwrap()),
            event_notify: watch::channel(0_u64).0,
            server: None,
        }
    }

    async fn start(&mut self) {
        self.start_with_binding(false).await;
    }

    async fn start_with_binding(&mut self, native: bool) {
        let state = AppState {
            store: self.store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: self.event_notify.clone(),
            node: "restart-node".into(),
            state_dir: self.root.join("daemon"),
            pty_root: self.root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let socket = self.socket.clone();
        self.server = Some(tokio::spawn(async move {
            let app = st3::api::router(state);
            if native {
                let _ = st3::api::serve_unix_bound(&socket, &socket, app).await;
            } else {
                let _ = st3::api::serve_unix(&socket, app).await;
            }
        }));
        wait_until(
            "the daemon accepts connections",
            Duration::from_secs(5),
            || std::os::unix::net::UnixStream::connect(&self.socket).is_ok(),
        )
        .await;
    }

    /// Stop the API the way an exiting daemon does: the socket file stays and refuses.
    async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
        wait_until(
            "the stopped daemon refuses new connections",
            Duration::from_secs(5),
            || std::os::unix::net::UnixStream::connect(&self.socket).is_err(),
        )
        .await;
    }

    fn append(&self, subject: &str, kind: &str, fields: Value) {
        self.store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: serde_json::from_value::<BTreeMap<String, Value>>(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        self.event_notify.send_modify(|value| *value = value.wrapping_add(1));
    }

    /// The runtime observation the reconciler recorded before the restart.
    fn observe_running(&self, subject: &str, incarnation: &str) {
        self.append(
            subject,
            "runtime.observed",
            json!({
                "runtime_id": subject.trim_start_matches("agent/"),
                "incarnation_id": incarnation,
                "status": "running",
                "reachability": "local",
            }),
        );
    }

    fn send(&self, message: &str, to: &str, content: &str) {
        self.append(
            message,
            "message.sent",
            json!({
                "from": "person/operator",
                "to": to,
                "content": content,
                "status": "sent",
            }),
        );
    }

    /// The in-memory store shares one cache between its connections, so a read that meets the
    /// API's write lock fails with `SQLITE_LOCKED`. A waiting read treats that as "not yet".
    fn harness_states(&self, subject: &str, incarnation: &str) -> Vec<String> {
        self.store
            .claims_for(subject, Some("harness.observed"))
            .unwrap_or_default()
            .into_iter()
            .filter(|claim| {
                claim
                    .body
                    .pointer("/fields/incarnation_id")
                    .and_then(Value::as_str)
                    == Some(incarnation)
            })
            .filter_map(|claim| {
                claim
                    .body
                    .pointer("/fields/state")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    fn has_diagnostic(&self, subject: &str, incarnation: &str, code: &str) -> bool {
        self.store
            .claims_for(subject, Some("harness.diagnostic"))
            .unwrap_or_default()
            .into_iter()
            .any(|claim| {
                claim.body.pointer("/fields/code").and_then(Value::as_str) == Some(code)
                    && claim
                        .body
                        .pointer("/fields/incarnation_id")
                        .and_then(Value::as_str)
                        == Some(incarnation)
            })
    }
}

async fn wait_until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A seat process launched with the environment the reconciler gives it, but with every home
/// and state directory inside the test root.
fn seat_command(root: &Path, socket: &Path) -> Command {
    let mut command = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"));
    // Its own process group, so stopping the seat also stops the stand-in provider.
    command
        .process_group(0)
        .current_dir(root.join("workspace"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("ST3_DRIVER_STATE_DIR", root.join("drivers"))
        .env("ST3_DAEMON_WAIT", "0")
        .env("ST3_ENDPOINT", socket);
    for directory in ["workspace", "home", "state", "config", "runtime", "drivers"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    command
}

fn driver_log(root: &Path) -> String {
    std::fs::read_to_string(root.join("state/st3/driver-api-warnings.log")).unwrap_or_default()
}

fn files_containing(directory: &Path, needle: &str) -> Vec<PathBuf> {
    // Driver directories contain IPC endpoints as well as evidence. Reading a FIFO as a
    // text file waits for its writer to close and can block this restart proof indefinitely.
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if path.is_file()
            && std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
        {
            found.push(path);
        }
    }
    found
}

fn assert_alive(child: &mut Child, what: &str) {
    assert!(
        child.try_wait().unwrap().is_none(),
        "{what} exited during the daemon outage"
    );
}

fn stop(mut child: Child) -> String {
    let group = i32::try_from(child.id()).unwrap();
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = child.wait();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_seat_starts_through_a_daemon_restart_and_then_keeps_its_mail() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let seat = "agent/restart-claude";
    let incarnation = "4242:2026-09-27T12:00:00.000Z";
    let mut daemon = Daemon::new(root);
    daemon.observe_running(seat, incarnation);
    // The daemon was already stopped for a deploy when the reconciler's PTY started the driver.
    daemon.start().await;
    daemon.stop().await;

    let mut driver = seat_command(root, &daemon.socket)
        .args(["driver", "claude", "--subject", seat, "--", "sleep", "300"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until(
        "the starting driver observes the daemon outage",
        Duration::from_secs(20),
        || {
            assert_alive(&mut driver, "a starting Claude driver");
            driver_log(root)
                .contains("waiting for the runtime incarnation while the daemon restarts")
        },
    )
    .await;
    assert_alive(&mut driver, "a starting Claude driver");
    assert!(daemon.harness_states(seat, incarnation).is_empty());

    daemon.start().await;
    wait_until(
        "the driver records the seat as starting",
        Duration::from_secs(10),
        || daemon.harness_states(seat, incarnation) == ["starting"],
    )
    .await;

    // Now the seat is running. A message accepted just before the next restart is delivered
    // once the daemon is back, without the driver exiting or writing to the seat's terminal.
    daemon.stop().await;
    daemon.send("message/restart-claude-mail", seat, "AMBER LANTERN");
    wait_until(
        "native delivery observes the daemon outage",
        Duration::from_secs(20),
        || {
            assert_alive(&mut driver, "a running Claude driver");
            driver_log(root).contains("native conversation delivery paused")
        },
    )
    .await;
    assert_alive(&mut driver, "a running Claude driver");
    assert!(files_containing(&root.join("drivers"), "AMBER LANTERN").is_empty());
    daemon.start().await;
    wait_until(
        "native delivery records recovery after the daemon restart",
        Duration::from_secs(20),
        || daemon.has_diagnostic(seat, incarnation, "native-delivery-recovered"),
    )
    .await;
    assert!(
        !files_containing(&root.join("drivers"), "AMBER LANTERN").is_empty(),
        "the recovered driver did not project the message to the Claude channel"
    );
    wait_until(
        "the driver records that delivery resumed",
        Duration::from_secs(10),
        || driver_log(root).contains("native conversation delivery resumed"),
    )
    .await;
    assert_alive(&mut driver, "a running Claude driver");
    // The graph records the delivery outage and its recovery for this incarnation.
    let diagnostics = daemon
        .store
        .claims_for(seat, Some("harness.diagnostic"))
        .unwrap()
        .into_iter()
        .map(|claim| {
            (
                claim.body["fields"]["code"].as_str().unwrap().to_owned(),
                claim.body["fields"]["status"].as_str().unwrap().to_owned(),
                claim.body["fields"]["incarnation_id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics,
        [
            (
                "native-delivery-degraded".into(),
                "waiting".into(),
                incarnation.into()
            ),
            (
                "native-delivery-recovered".into(),
                "recovered".into(),
                incarnation.into()
            ),
        ]
    );

    let terminal = stop(driver);
    assert!(
        terminal.is_empty(),
        "the driver wrote into the seat's terminal: {terminal}"
    );
    let log = driver_log(root);
    assert!(log.contains("is not reachable"), "{log}");
    assert!(log.contains("may be restarting"), "{log}");
    assert!(
        log.contains("retrying every 1s until the daemon is back"),
        "{log}"
    );
    assert!(
        log.contains("native conversation delivery resumed"),
        "{log}"
    );
    assert!(!log.contains("st3 up"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pi_family_channel_keeps_state_and_mail_through_a_daemon_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let seat = "agent/restart-omp";
    let incarnation = "4343:2026-09-27T12:00:00.000Z";
    let mut daemon = Daemon::new(root);
    daemon.observe_running(seat, incarnation);
    daemon.start().await;

    let mut channel = seat_command(root, &daemon.socket)
        .arg("--catalog")
        .arg(root.join("catalog"))
        .args(["driver", "omp-channel", "--identity", "restart-omp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = channel.stdin.take().unwrap();
    let (frames, received) = std::sync::mpsc::channel::<Value>();
    let output = channel.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if let Ok(frame) = serde_json::from_str(&line) {
                let _ = frames.send(frame);
            }
        }
    });
    let hello = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(hello["type"], "hello");

    daemon.stop().await;
    // The harness goes idle and a message arrives while the daemon is down.
    writeln!(input, "{}", json!({"type": "state", "state": "idle"})).unwrap();
    input.flush().unwrap();
    daemon.send("message/restart-omp-mail", seat, "COPPER KITE");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_alive(&mut channel, "the omp channel");
    assert!(daemon.harness_states(seat, incarnation).is_empty());

    daemon.start().await;
    wait_until(
        "the channel publishes the state it kept",
        Duration::from_secs(10),
        || daemon.harness_states(seat, incarnation) == ["idle"],
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let message = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = received
            .recv_timeout(remaining)
            .expect("the channel delivers the message once the daemon is back");
        if frame["type"] == "message" {
            break frame;
        }
    };
    assert!(message.to_string().contains("COPPER KITE"), "{message}");

    // The harness takes the message; the acknowledgement reaches the graph.
    writeln!(
        input,
        "{}",
        json!({"type": "delivered", "meta": {"messageId": "message/restart-omp-mail"}})
    )
    .unwrap();
    input.flush().unwrap();
    wait_until(
        "the delivery acknowledgement is recorded",
        Duration::from_secs(10),
        || {
            daemon
                .store
                .message("message/restart-omp-mail")
                .ok()
                .flatten()
                .is_some_and(|message| message.status == "delivered")
        },
    )
    .await;
    assert_alive(&mut channel, "the omp channel");

    let stderr = stop(channel);
    assert!(stderr.is_empty(), "the channel wrote to stderr: {stderr}");
    let log = driver_log(root);
    assert!(log.contains("may be restarting"), "{log}");
    assert!(!log.contains("st3 up"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn todo_graph_lag_on_first_open_keeps_delivery_and_publishes_hydration_after_catchup() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let seat = "agent/restart-todo";
    let mut daemon = Daemon::new(root);
    daemon.store = Arc::new(Store::open(&root.join("graph.sqlite"), "restart-node").unwrap());
    daemon.observe_running(seat, "previous-runtime");
    daemon.start_with_binding(true).await;
    let registry = root.join("local-pty");
    std::fs::create_dir_all(&registry).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(registry.join("seat.sock")).unwrap();
    std::fs::write(registry.join("seat.pid"), std::process::id().to_string()).unwrap();
    let started = "2026-10-03T20:00:00.000Z";
    std::fs::write(registry.join("seat.json"), json!({
        "createdAt": started, "tags": {"st3.subject": seat},
    }).to_string()).unwrap();
    let incarnation = format!("{}:{started}", std::process::id());
    use sha2::Digest as _;
    let dir = root.join("catalog/.st3-channel-outbox")
        .join(hex::encode(sha2::Sha256::digest(seat.as_bytes())))
        .join(hex::encode(sha2::Sha256::digest(incarnation.as_bytes())));
    let todo = json!({"type":"todo", "session":"native", "observed_at":"2026-10-03T20:00:01Z",
        "source_op":"hydrate", "phases":[{"name":"Restart","tasks":[
            {"content":"Preserved through graph lag","status":"in_progress"}
        ]}], "totals":{"pending":0,"in_progress":1,"completed":0,"blocked":0},
        "truncated":false});
    st_drivers::harness_events::enable(&dir, &incarnation).unwrap();
    let mut fields = st_drivers::pi_channel::todo_observation(&todo, "omp", Some("native"), &incarnation).unwrap();
    fields.get_mut("phases").unwrap()[0]["tasks"][0]["content"] = "Older rejected hydration".into();
    st_drivers::harness_events::write_channel_todo(&dir, &incarnation, &json!(fields)).unwrap();
    let event = st_drivers::harness_events::pending(&dir, 10).unwrap().remove(0);
    let mut bad_fields: BTreeMap<String, Value> = serde_json::from_value(event.payload).unwrap();
    bad_fields.remove("incarnation");
    let rejected = ClaimInput {
        subject: seat.into(), kind: "harness.todo.observed".into(), actor: Some(seat.into()),
        fields: bad_fields, evidence: vec![], expected_subject: None,
        idempotency_key: Some(format!("harness-todo:{seat}:{incarnation}:{}", event.sequence)),
    };
    st_drivers::harness_events::prepare_publication(&dir, event.sequence,
        "harness.todo.observed:", &serde_json::to_value(rejected).unwrap()).unwrap();
    let mut channel = seat_command(root, &daemon.socket)
        .env("PTY_ROOT", &registry)
        .env("ST_AGENT", seat)
        .env("ST3_ACCOUNT", std::env::var("ST3_ACCOUNT").unwrap_or_default())
        .arg("--catalog").arg(root.join("catalog"))
        .args(["driver", "omp-channel", "--identity", "restart-todo"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let mut input = channel.stdin.take().unwrap();
    let output = channel.stdout.take().unwrap();
    let (frames, received) = std::sync::mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if let Ok(frame) = serde_json::from_str(&line) { let _ = frames.send(frame); }
        }
    });
    assert_eq!(received.recv_timeout(Duration::from_secs(10)).unwrap()["type"], "hello");
    for frame in [
        json!({"type":"ready", "sessionId":"native"}),
        json!({"type":"state", "state":"idle"}),
        todo,
    ] { writeln!(input, "{frame}").unwrap(); }
    input.flush().unwrap();
    daemon.send("message/restart-todo-mail", seat, "GRAPH LAG MAIL");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frame = match received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(frame) => frame,
            Err(error) => panic!("waiting for graph-lag delivery: {error}; stderr: {}", stop(channel)),
        };
        assert_ne!(frame["type"], "hello", "delivery must not reconnect");
        if frame["type"] == "message" {
            assert!(frame.to_string().contains("GRAPH LAG MAIL"));
            break;
        }
    }
    assert_alive(&mut channel, "the graph-lag channel");
    assert!(daemon.store.claims_for(seat, Some("harness.todo.observed")).unwrap().is_empty());
    daemon.observe_running(seat, &incarnation);
    let deadline = Instant::now() + Duration::from_secs(65);
    while !daemon.store.claims_for(seat, Some("harness.todo.observed")).unwrap_or_default().iter()
            .any(|claim| claim.body.pointer("/fields/incarnation_id").and_then(Value::as_str)
                    == Some(incarnation.as_str())
                && claim.body.pointer("/fields/phases/0/tasks/0/content").and_then(Value::as_str)
                    == Some("Preserved through graph lag"))
    {
        assert!(Instant::now() < deadline, "hydration after graph catch-up: {}", driver_log(root));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_alive(&mut channel, "the caught-up channel");
    wait_until("the repaired retry is acknowledged", Duration::from_secs(5), || {
        st_drivers::harness_events::pending(&dir, 10).unwrap().is_empty()
    }).await;
    let connection = rusqlite::Connection::open(st_drivers::harness_events::database_path(&dir)).unwrap();
    let old_slots: u64 = connection.query_row(
        "SELECT COUNT(*) FROM prepared WHERE slot='harness.todo.observed:'", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(old_slots, 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!driver_log(root).contains("unknown-claim-field"), "{}", driver_log(root));
    assert_alive(&mut channel, "the repaired channel after later retry ticks");
    let _ = stop(channel);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_outbox_drain_preserves_captured_limits_and_usage_account_attribution() {
    if st3::test_support::supervise_test() {
        return;
    }
    use sha2::Digest as _;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let seat = "agent/drain-accounts";
    let incarnation = "account-runtime";
    let mut daemon = Daemon::new(root);
    daemon.store = Arc::new(Store::open(&root.join("graph.sqlite"), "restart-node").unwrap());
    daemon.observe_running(seat, incarnation);
    daemon.start_with_binding(true).await;
    let mut channel = seat_command(root, &daemon.socket)
        .env("ST_AGENT", seat).env("ST3_ACCOUNT", "successor/different-account")
        .arg("--catalog").arg(root.join("catalog"))
        .args(["driver", "omp-channel", "--identity", "drain-accounts"])
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().unwrap();
    let dir = root.join("catalog/.st3-channel-outbox")
        .join(hex::encode(sha2::Sha256::digest(seat.as_bytes())))
        .join(hex::encode(sha2::Sha256::digest(incarnation.as_bytes())));
    wait_until("the native drain binds its spool", Duration::from_secs(10), || {
        st_drivers::harness_events::enabled(&dir) && st_drivers::harness_events::pending(&dir, 1).is_ok()
    }).await;
    st_drivers::harness_state::claim(&dir, "drain-accounts", "omp", "account-producer").unwrap();
    let now = st_drivers::message::now_ms();
    st_drivers::harness_events::write_snapshot(&dir, "harness-context", &serde_json::to_vec(&json!({
        "schema":"st.harness-context.v1", "agent":"drain-accounts", "harness":"omp",
        "incarnation":"account-producer", "observedAtMs":now, "writtenAtMs":now,
        "sessionTotalTokens":123, "account":"omp/provider-account",
        "rateLimits":{"fiveHour":37,"sevenDay":41,"observedAtMs":now - 120_000},
    })).unwrap()).unwrap();
    let mut timeline = st_drivers::harness_timeline::Writer::new(&dir, "omp", "account-producer");
    timeline.append("account-response", st_drivers::harness_timeline::Role::System,
        st_drivers::harness_timeline::EntryType::Usage, json!({
            "semantics":"response", "model":"example", "account":"omp/provider-account",
            "input_tokens":10,"output_tokens":2,"total_tokens":12,
        }), true).unwrap();
    wait_until("limits and usage publish through the native drain", Duration::from_secs(10), || {
        !daemon.store.observations_for(seat, "harness.limits").unwrap_or_default().is_empty()
            && daemon.store.observations_for(seat, "harness.usage").unwrap_or_default().iter()
                .any(|claim| claim.body.pointer("/fields/semantics").and_then(Value::as_str)
                    == Some("session_cumulative"))
            && !daemon.store.observations_for(seat, "harness.timeline").unwrap_or_default().is_empty()
    }).await;
    let limits = daemon.store.observations_for(seat, "harness.limits").unwrap().pop().unwrap();
    let cumulative = daemon.store.observations_for(seat, "harness.usage").unwrap().into_iter()
        .find(|claim| claim.body.pointer("/fields/semantics").and_then(Value::as_str)
            == Some("session_cumulative")).unwrap();
    let timeline = daemon.store.observations_for(seat, "harness.timeline").unwrap().pop().unwrap();
    let captured = std::env::var("ST3_ACCOUNT").ok().filter(|account| !account.is_empty());
    let account = captured.as_ref().map(|account| {
        st_drivers::account::account_label("omp", &format!("declared:{account}"))
    }).unwrap_or_else(|| "omp/provider-account".into());
    assert_eq!(limits.body.pointer("/fields/account").and_then(Value::as_str), Some(account.as_str()));
    assert_eq!(limits.body.pointer("/fields/account_ref").and_then(Value::as_str), captured.as_deref());
    assert_eq!(limits.body.pointer("/fields/five_hour_percent").and_then(Value::as_f64), Some(37.0));
    assert_eq!(limits.body.pointer("/fields/measured_at_unix_ms").and_then(Value::as_u64), Some(now - 120_000));
    assert_eq!(cumulative.body.pointer("/fields/total_tokens").and_then(Value::as_u64), Some(123));
    assert_eq!(cumulative.body.pointer("/fields/account").and_then(Value::as_str),
        captured.as_ref().map(|_| account.as_str()));
    assert_eq!(timeline.body.pointer("/fields/body/account").and_then(Value::as_str), Some(account.as_str()));
    assert_eq!(timeline.body.pointer("/fields/body/total_tokens").and_then(Value::as_u64), Some(12));
    assert_alive(&mut channel, "the attribution channel");
    assert!(stop(channel).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_omp_ask_clears_without_poisoning_the_next_incarnation() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let seat = "agent/human-omp";
    let incarnation = "human-1";
    let mut daemon = Daemon::new(root);
    let source = r#"
version 2
agent "human-omp" { workspace "/tmp"; harness "omp" {} }
"#;
    let intent = st3::parse_intent(source, "restart-node").unwrap();
    let plan = daemon.store.mission(&intent, st3::model::IntentInput {
        kdl: source.into(),
        source_name: None,
    }).unwrap();
    daemon.store.apply(&intent, &plan.subject_tokens, "human-omp-source").unwrap();
    daemon.observe_running(seat, incarnation);
    daemon.start().await;
    let client = st3::client::Client::unix(&daemon.socket);
    let mut channel = seat_command(root, &daemon.socket)
        .arg("--catalog").arg(root.join("catalog"))
        .args(["driver", "omp-channel", "--identity", "human-omp"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let mut input = channel.stdin.take().unwrap();

    writeln!(input, "{}", json!({
        "type": "state", "state": "active", "blockedOn": "human",
        "ask": "question", "reason": "Which deployment target?",
    })).unwrap();
    input.flush().unwrap();
    wait_until("the channel publishes the ask", Duration::from_secs(10), || {
        daemon.harness_states(seat, incarnation) == ["working"]
    }).await;
    let status: st3::model::StatusResponse =
        client.get(&format!("/v1/status?subject={seat}")).await.unwrap();
    let blocked = status.subjects.into_iter().find(|status| status.subject == seat)
        .unwrap().harness.unwrap();
    assert_eq!(blocked.state, "working");
    assert_eq!(blocked.blocked_on.as_deref(), Some("human"));
    assert_eq!(blocked.ask.as_deref(), Some("question"));
    assert_eq!(blocked.reason.as_deref(), Some("Which deployment target?"));

    // These are the producer's frames after the matching ask result. The producer's smoke
    // scenario proves that an unrelated result emits only timeline data, never this state.
    writeln!(input, "{}", json!({
        "type": "timeline", "event": "tool_result", "payload": {"toolCallId": "ask-1"},
    })).unwrap();
    writeln!(input, "{}", json!({"type": "state", "state": "active"})).unwrap();
    input.flush().unwrap();
    wait_until("the channel publishes the answered state", Duration::from_secs(10), || {
        daemon.harness_states(seat, incarnation) == ["working", "working"]
    }).await;
    let status: st3::model::StatusResponse =
        client.get(&format!("/v1/status?subject={seat}")).await.unwrap();
    let answered = status.subjects.into_iter().find(|status| status.subject == seat)
        .unwrap().harness.unwrap();
    assert_eq!(answered.state, "working");
    assert!(answered.blocked_on.is_none());
    assert!(answered.ask.is_none());
    assert!(answered.reason.is_none());
    assert!(stop(channel).is_empty());

    // A delayed ask from the old channel must not block a resumed runtime's fresh idle proof.
    daemon.observe_running(seat, "human-2");
    daemon.append(seat, "harness.observed", json!({
        "state": "idle", "driver": "omp", "incarnation_id": "human-2",
        "blocked_on": null, "ask": null, "reason": null, "input_buffer": null, "exit": null,
    }));
    daemon.append(seat, "harness.observed", json!({
        "state": "working", "driver": "omp", "incarnation_id": incarnation,
        "blocked_on": "human", "ask": "question", "reason": "An obsolete question",
    }));
    let status: st3::model::StatusResponse =
        client.get(&format!("/v1/status?subject={seat}")).await.unwrap();
    let resumed = status.subjects.into_iter().find(|status| status.subject == seat)
        .unwrap().harness.unwrap();
    assert_eq!(resumed.state, "idle");
    assert_eq!(resumed.incarnation_id, "human-2");
    assert!(resumed.blocked_on.is_none());
    assert!(resumed.ask.is_none());
    assert!(resumed.reason.is_none());
}

#[test]
fn a_cli_command_says_the_daemon_is_unreachable_and_never_to_start_it() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let started = Instant::now();
    let output = seat_command(root.path(), &socket)
        .env("ST3_DAEMON_WAIT", "1")
        .args(["work", "ls", "--as", "agent/restart-cli"])
        .output()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "not reachable (connection refused); it may be restarting; retrying for up to 1s"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("was not reachable for 1s"), "{stderr}");
    assert!(stderr.contains("Nothing was sent"), "{stderr}");
    assert!(!stderr.contains("st3 up"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cli_command_waits_out_a_daemon_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut daemon = Daemon::new(root);
    daemon.start().await;
    daemon.stop().await;
    let mut command = seat_command(root, &daemon.socket);
    command.env("ST3_DAEMON_WAIT", "20").args([
        "--json",
        "work",
        "ls",
        "--as",
        "agent/restart-cli",
    ]);
    let pending = tokio::task::spawn_blocking(move || command.output().unwrap());
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    daemon.start().await;
    let output = pending.await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("it may be restarting; retrying for up to 20s"),
        "{stderr}"
    );
    let _: Value = serde_json::from_slice(&output.stdout).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_read_mail_is_settled_before_and_after_reopening_the_daemon() {
    if st3::test_support::supervise_test() {
        return;
    }
    for driver in ["omp", "pi"] {
        for transport in ["poll", "push"] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path();
            let seat = "agent/receipt-worker";
            let graph = root.join("graph.db");
            let mut daemon = Daemon::new(root);
            daemon.store = Arc::new(Store::open(&graph, "restart-node").unwrap());
            daemon.observe_running(seat, "same-incarnation");
            daemon.start_with_binding(true).await;

            let socket = daemon.socket.clone();
            daemon.send("message/ready-probe", seat, "READINESS PROBE");
            for lifecycle in ["delivered", "read", "closed"] {
                daemon.store.append_claim(&ClaimInput {
                    subject: "message/ready-probe".into(), kind: format!("message.{lifecycle}"),
                    actor: Some(seat.into()), fields: BTreeMap::from([("status".into(), json!(lifecycle))]),
                    evidence: Vec::new(), expected_subject: None, idempotency_key: None,
                }).unwrap();
            }
            let open_channel = || async {
                let mut channel = seat_command(root, &socket)
                    .env("ST_AGENT", seat)
                    .env("ST3_MAILBOX_TRANSPORT", transport)
                    .arg("--catalog")
                    .arg(root.join("catalog"))
                    .args([
                        "driver",
                        &format!("{driver}-channel"),
                        "--identity",
                        "receipt-worker",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut input = channel.stdin.take().unwrap();
                let (frames, received) = std::sync::mpsc::channel::<Value>();
                let output = channel.stdout.take().unwrap();
                std::thread::spawn(move || {
                    for line in BufReader::new(output).lines() {
                        let Ok(line) = line else { break };
                        if let Ok(frame) = serde_json::from_str(&line) {
                            let _ = frames.send(frame);
                        }
                    }
                });
                assert_eq!(
                    received.recv_timeout(Duration::from_secs(10)).unwrap()["type"],
                    "hello"
                );
                writeln!(input, "{}", json!({"type":"state", "state":"idle"})).unwrap();
                input.flush().unwrap();
                let client = st3::client::Client::new(st3::client::Endpoint::Unix(socket.clone()));
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let status: Value = client.get("/v1/messages/delivery/ready-probe").await.unwrap();
                    let delivery = &status["delivery"]["recipient_delivery"];
                    let ready = matches!(delivery["state"].as_str(), Some("current" | "outdated" | "legacy"));
                    let own_pid = delivery["state"] == "current" || delivery["reason"].as_str()
                        .is_some_and(|reason| reason.contains(&format!("pid {}", channel.id())));
                    if ready && own_pid { break; }
                    assert!(Instant::now() < deadline, "{driver}/{transport}: channel not ready: {status}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                (channel, input, received)
            };
            let (channel, mut input, received) = open_channel().await;
            daemon.send("message/consumed", seat, "CONSUMED SIGNAL");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let frame = received
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if frame["type"] == "message" {
                    assert_eq!(frame["meta"]["messageId"], "message/consumed");
                    break;
                }
            }
            // The provider's context event, rather than a CLI read, proves consumption.
            writeln!(
                input,
                "{}",
                json!({"type":"read", "meta":{"messageId":"message/consumed"}})
            )
            .unwrap();
            input.flush().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut settled = false;
            while let Ok(frame) =
                received.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                if frame["type"] == "settled" {
                    assert_eq!(frame["meta"]["messageId"], "message/consumed");
                    settled = true;
                    break;
                }
            }
            assert!(stop(channel).is_empty());
            let before = daemon
                .store
                .message("message/consumed")
                .unwrap()
                .unwrap()
                .status;

            // A real durable Store reopen discards projection caches as a fresh daemon does.
            daemon.stop().await;
            daemon.store = Arc::new(Store::open(&graph, "restart-node").unwrap());
            daemon.start_with_binding(true).await;
            let (channel, _input, received) = open_channel().await;
            daemon.send("message/unread", seat, "UNREAD SIGNAL");
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut offered = Vec::new();
            while let Ok(frame) =
                received.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                if frame["type"] == "message" {
                    offered.push(frame["meta"]["messageId"].as_str().unwrap().to_owned());
                }
            }
            assert!(stop(channel).is_empty());
            assert_eq!(
                offered,
                ["message/unread"],
                "{driver}/{transport}: native read acknowledged={settled}, pre-restart graph status={before}"
            );
            assert!(
                settled,
                "{driver}/{transport}: the native read was discarded instead of acknowledged"
            );
            assert_eq!(before, "read");
            assert_eq!(
                daemon
                    .store
                    .claims_for("message/consumed", Some("message.read"))
                    .unwrap()
                    .len(),
                1
            );
            daemon.store.replay_replication_graph().unwrap();
            assert_eq!(
                daemon.store.message("message/consumed").unwrap().unwrap().status,
                "read"
            );
            assert_eq!(
                daemon.store.message("message/unread").unwrap().unwrap().status,
                "staged"
            );
            daemon.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivered_unread_mail_stays_in_the_mailbox_after_seat_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    for driver in ["omp", "pi"] {
        for transport in ["push", "poll"] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path();
            let seat = "agent/replay-worker";
            let mut daemon = Daemon::new(root);
            daemon.store = Arc::new(Store::open(&root.join("graph.db"), "restart-node").unwrap());
            daemon.observe_running(seat, "previous");
            for (id, lifecycle) in [
                ("unread-one", "delivered"),
                ("unread-two", "delivered"),
                ("read", "read"),
                ("closed", "closed"),
            ] {
                let subject = format!("message/{id}");
                daemon.send(&subject, seat, &format!("BRIEF {id}"));
                for status in ["delivered", "read", "closed"] {
                    daemon.store.append_claim(&ClaimInput {
                        subject: subject.clone(),
                        kind: format!("message.{status}"),
                        actor: Some(seat.into()),
                        fields: BTreeMap::from([("status".into(), json!(status))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    }).unwrap();
                    if status == lifecycle { break; }
                }
            }
            // Keep the native parent session, replacing only the seat incarnation.
            daemon.append(seat, "runtime.observed", json!({
                "runtime_id": "replay-worker", "incarnation_id": "previous", "status": "exited",
            }));
            daemon.observe_running(seat, "replacement");
            daemon.start_with_binding(true).await;
            let mut channel = seat_command(root, &daemon.socket)
                .env("ST_AGENT", seat)
                .env("ST3_MAILBOX_TRANSPORT", transport)
                .env(st_drivers::omp_session::CHANNEL_SESSION, "same-native-parent")
                .env(st_drivers::pi_session::CHANNEL_SESSION, "same-native-parent")
                .arg("--catalog").arg(root.join("catalog"))
                .args(["driver", &format!("{driver}-channel"), "--identity", "replay-worker"])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
                .spawn().unwrap();
            let mut input = channel.stdin.take().unwrap();
            let (frames, received) = std::sync::mpsc::channel::<Value>();
            let output = channel.stdout.take().unwrap();
            std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(frame) = serde_json::from_str(&line) {
                        let _ = frames.send(frame);
                    }
                }
            });
            assert_eq!(received.recv_timeout(Duration::from_secs(10)).unwrap()["type"], "hello");
            writeln!(input, "{}", json!({"type":"state", "state":"idle"})).unwrap();
            input.flush().unwrap();
            // Several mailbox ticks must not reoffer old mail, regardless of native receipts.
            let deadline = Instant::now() + Duration::from_millis(2_200);
            while let Ok(frame) = received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                assert_ne!(frame["type"], "message", "{driver}/{transport}: duplicate or settled mail: {frame}");
            }
            assert_eq!(daemon.store.message("message/unread-one").unwrap().unwrap().status, "delivered");
            assert_eq!(daemon.store.message("message/read").unwrap().unwrap().status, "read");
            assert_eq!(daemon.store.message("message/closed").unwrap().unwrap().status, "closed");
            assert_eq!(daemon.store.claims_for("message/unread-one", Some("message.delivered")).unwrap().len(), 1);
            let stderr = stop(channel);
            assert!(stderr.is_empty(), "{driver}/{transport}: {stderr}");
            daemon.stop().await;
        }
    }
}

// Claude can create its transcript after SessionStart. Its channel must recover the
// exact hook-bound session, and consumption need not run UserPromptSubmit.
struct ClaudeChannelFixture {
    paths: st_drivers::driver_paths::Paths,
    transcript: PathBuf,
}
impl ClaudeChannelFixture {
    fn new(root: &Path, daemon: &Daemon, wrapper: &str) -> Self {
        let paths = st_drivers::driver_paths::Paths {
            root: root.join("claude-driver"),
            agent_dir: root.join("claude-driver/observations"),
            session_dir: root.join("claude-driver/sessions/claude"),
        };
        std::fs::create_dir_all(&paths.agent_dir).unwrap();
        let transcript =
            root.join("home/.claude/projects/quartz/019fae17-c215-7882-a4d9-5f247168ffce.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let fixture = Self { paths, transcript };
        // No transcript yet: reproduce a real SessionStart binding failure. The
        // lightweight hook binding still identifies this wrapper's native session.
        let mut hook = fixture
            .command(root, daemon, wrapper)
            .args(["driver-hook", "claude-observe", "SessionStart"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(hook.stdin.take().unwrap(), "{}", json!({
            "session_id":"019fae17-c215-7882-a4d9-5f247168ffce",
            "transcript_path":fixture.transcript, "cwd":root.join("workspace"), "source":"startup",
        })).unwrap();
        let output = hook.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "SessionStart failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        daemon.send("message/quartz-ready", "agent/quartz", "READINESS PROBE");
        for lifecycle in ["delivered", "read", "closed"] {
            daemon.store.append_claim(&ClaimInput {
                subject: "message/quartz-ready".into(), kind: format!("message.{lifecycle}"),
                actor: Some("agent/quartz".into()), fields: BTreeMap::from([("status".into(), json!(lifecycle))]),
                evidence: Vec::new(), expected_subject: None, idempotency_key: None,
            }).unwrap();
        }
        fixture
    }
    fn command(&self, root: &Path, daemon: &Daemon, wrapper: &str) -> Command {
        let mut command = seat_command(root, &daemon.socket);
        command
            .envs(self.paths.environment("quartz"))
            .env("ST_AGENT", "agent/quartz")
            .env("ST3_SUBJECT", "agent/quartz")
            .env("ST3_MAILBOX_TRANSPORT", "push")
            .env("ST_CLAUDE_IDENTITY", "quartz")
            .env("ST_CLAUDE_RUNTIME_ID", "quartz")
            .env("ST_CLAUDE_SESSION", wrapper)
            .env("ST_CLAUDE_SESSION_SEQ", "1");
        command
    }
    async fn open(
        &self,
        root: &Path,
        daemon: &Daemon,
        wrapper: &str,
    ) -> (
        Child,
        std::process::ChildStdin,
        std::sync::mpsc::Receiver<Value>,
    ) {
        let mut channel = self
            .command(root, daemon, wrapper)
            .args(["driver", "claude-mcp", "--subject", "agent/quartz"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = channel.stdin.take().unwrap();
        let (sender, received) = std::sync::mpsc::channel::<Value>();
        let output = channel.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Ok(frame) = serde_json::from_str(&line) {
                    let _ = sender.send(frame);
                }
            }
        });
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"})
        )
        .unwrap();
        input.flush().unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(10)).unwrap()["id"],
            1
        );
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        input.flush().unwrap();
        let client = st3::client::Client::new(st3::client::Endpoint::Unix(daemon.socket.clone()));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view: Value = client.get("/v1/messages/delivery/quartz-ready").await.unwrap();
            let delivery = &view["delivery"]["recipient_delivery"];
            let ready = matches!(delivery["state"].as_str(), Some("current" | "outdated" | "legacy"));
            let own_pid = delivery["state"] == "current" || delivery["reason"].as_str()
                .is_some_and(|reason| reason.contains(&format!("pid {}", channel.id())));
            if ready && own_pid { break; }
            assert!(Instant::now() < deadline, "Claude channel not ready: {view}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        (channel, input, received)
    }
    fn append(&self, root: &Path, record: Value) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
            .unwrap();
        let mut record = record;
        record["sessionId"] = json!("019fae17-c215-7882-a4d9-5f247168ffce");
        record["cwd"] = json!(root.join("workspace"));
        writeln!(file, "{record}").unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_idle_staged_mail_recovers_startup_binding_and_both_native_receipt_forms() {
    if st3::test_support::supervise_test() {
        return;
    }
    for mid_turn in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut daemon = Daemon::new(root);
        daemon.observe_running("agent/quartz", "first");
        daemon.start_with_binding(true).await;
        let fixture = ClaudeChannelFixture::new(root, &daemon, "wrapper-first");
        let (channel, _input, received) = fixture.open(root, &daemon, "wrapper-first").await;
        daemon.send("message/idle", "agent/quartz", "QUARTZ IDLE SIGNAL");
        let frame = received.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(frame["method"], "notifications/claude/channel");
        assert!(daemon.has_diagnostic("agent/quartz", "first", "claude-receipt-unavailable"));
        assert_eq!(
            daemon
                .store
                .message("message/idle")
                .unwrap()
                .unwrap()
                .status,
            "staged"
        );
        let content = &frame["params"]["content"];
        fixture.append(
            root,
            json!({"type":"queue-operation","operation":"enqueue","content":content}),
        );
        // Enqueue proves native transport acceptance, but not consumption.
        wait_until(
            "the native Claude acceptance receipt",
            Duration::from_secs(6),
            || {
                daemon
                    .store
                    .message("message/idle")
                    .unwrap()
                    .unwrap()
                    .status
                    == "delivered"
            },
        )
        .await;
        assert!(
            daemon
                .store
                .claims_for("message/idle", Some("message.read"))
                .unwrap()
                .is_empty()
        );
        if mid_turn {
            fixture.append(root, json!({"type":"queue-operation","operation":"remove",
                "reason":"absorbed_mid_turn","content":content,"commandUuid":"native-command","deliveryId":"native-delivery"}));
        } else {
            fixture.append(
                root,
                json!({"type":"user","isMeta":true,"message":{"role":"user","content":content}}),
            );
        }
        wait_until(
            "the native Claude consumption receipt",
            Duration::from_secs(6),
            || {
                daemon
                    .store
                    .message("message/idle")
                    .unwrap()
                    .unwrap()
                    .status
                    == "read"
            },
        )
        .await;
        for kind in ["message.delivered", "message.read"] {
            assert_eq!(
                daemon
                    .store
                    .claims_for("message/idle", Some(kind))
                    .unwrap()
                    .len(),
                1
            );
        }
        assert!(received.recv_timeout(Duration::from_millis(1200)).is_err());
        assert!(stop(channel).is_empty());
        daemon.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_staged_mail_receipted_after_restart_is_not_injected_on_second_restart() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut daemon = Daemon::new(root);
    daemon.observe_running("agent/quartz", "first");
    daemon.start_with_binding(true).await;
    let fixture = ClaudeChannelFixture::new(root, &daemon, "wrapper-first");
    let (channel, _input, received) = fixture.open(root, &daemon, "wrapper-first").await;
    daemon.send("message/restart", "agent/quartz", "QUARTZ RESTART SIGNAL");
    let frame = received.recv_timeout(Duration::from_secs(10)).unwrap();
    let content = frame["params"]["content"].clone();
    assert!(stop(channel).is_empty());
    assert_eq!(
        daemon
            .store
            .message("message/restart")
            .unwrap()
            .unwrap()
            .status,
        "staged"
    );
    // Claude consumed the old notification while the channel was down. Native
    // proof precedes restarting the channel; reoffering it would duplicate work.
    fixture.append(
        root,
        json!({"type":"user","isMeta":true,"message":{"role":"user","content":content}}),
    );
    for incarnation in ["second", "third"] {
        daemon.observe_running("agent/quartz", incarnation);
        // A wrapper restart keeps the provider's resumed transcript and records
        // its lightweight binding before the channel's first mailbox replay.
        std::fs::write(fixture.paths.agent_dir.join("claude-native-session"),
            json!({"incarnation":incarnation,"native_session_id":"019fae17-c215-7882-a4d9-5f247168ffce"}).to_string()).unwrap();
        let (channel, _input, received) = fixture.open(root, &daemon, incarnation).await;
        wait_until(
            "the recovered startup receipt",
            Duration::from_secs(6),
            || {
                daemon
                    .store
                    .message("message/restart")
                    .unwrap()
                    .unwrap()
                    .status
                    == "read"
            },
        )
        .await;
        assert!(
            received.recv_timeout(Duration::from_millis(1200)).is_err(),
            "{incarnation} reinjected consumed mail"
        );
        assert!(stop(channel).is_empty());
    }
    for kind in ["message.delivered", "message.read"] {
        assert_eq!(
            daemon
                .store
                .claims_for("message/restart", Some(kind))
                .unwrap()
                .len(),
            1
        );
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_preboot_mail_is_held_while_live_receipts_survive_outage() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut daemon = Daemon::new(root);
    daemon.observe_running("agent/quartz", "previous");
    daemon.send("message/startup", "agent/quartz", "QUARTZ STARTUP SIGNAL");
    daemon
        .store
        .append_claim(&ClaimInput {
            subject: "message/startup".into(),
            kind: "message.staged".into(),
            actor: Some("agent/quartz".into()),
            fields: BTreeMap::from([
                ("status".into(), json!("staged")),
                ("recipient".into(), json!("agent/quartz")),
                ("transport".into(), json!("claude-channel")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    daemon.observe_running("agent/quartz", "replacement");
    daemon.start_with_binding(true).await;
    let fixture = ClaudeChannelFixture::new(root, &daemon, "wrapper-replacement");
    let (mut channel, _input, received) = fixture.open(root, &daemon, "wrapper-replacement").await;
    assert!(received.recv_timeout(Duration::from_millis(1200)).is_err());
    assert_eq!(daemon.store.message("message/startup").unwrap().unwrap().status, "staged");
    daemon.send("message/live", "agent/quartz", "QUARTZ LIVE SIGNAL");
    let frame = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(frame["params"]["meta"]["messageId"], "message/live");
    // Consumption happens while receipt publication is unavailable. The channel
    // must keep proof, retry only the receipt, and never repeat the notification.
    daemon.stop().await;
    fixture.append(
        root,
        json!({"type":"user","isMeta":true,
        "message":{"role":"user","content":frame["params"]["content"]}}),
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_alive(&mut channel, "the Claude channel");
    daemon.start_with_binding(true).await;
    wait_until(
        "startup receipts after daemon recovery",
        Duration::from_secs(8),
        || {
            daemon
                .store
                .message("message/live")
                .unwrap()
                .unwrap()
                .status
                == "read"
        },
    )
    .await;
    assert!(received.recv_timeout(Duration::from_millis(1200)).is_err());
    assert!(stop(channel).is_empty());
    daemon.observe_running("agent/quartz", "second-restart");
    std::fs::write(fixture.paths.agent_dir.join("claude-native-session"),
        json!({"incarnation":"wrapper-second","native_session_id":"019fae17-c215-7882-a4d9-5f247168ffce"}).to_string()).unwrap();
    let (channel, _input, received) = fixture.open(root, &daemon, "wrapper-second").await;
    assert!(received.recv_timeout(Duration::from_millis(1200)).is_err());
    assert!(stop(channel).is_empty());
    for kind in ["message.delivered", "message.read"] {
        assert_eq!(
            daemon
                .store
                .claims_for("message/live", Some(kind))
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(daemon.store.message("message/startup").unwrap().unwrap().status, "staged");
    assert!(daemon.store.claims_for("message/startup", Some("message.delivered")).unwrap().is_empty());
    assert!(daemon.store.claims_for("message/startup", Some("message.read")).unwrap().is_empty());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_delivered_unread_mail_from_an_old_ledger_is_held_in_a_fresh_session() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let mut daemon = Daemon::new(root);
    daemon.observe_running("agent/quartz", "replacement");
    daemon.send("message/unread", "agent/quartz", "QUARTZ UNREAD SIGNAL");
    daemon
        .store
        .append_claim(&ClaimInput {
            subject: "message/unread".into(),
            kind: "message.delivered".into(),
            actor: Some("agent/quartz".into()),
            fields: BTreeMap::from([("status".into(), json!("delivered"))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    daemon.start_with_binding(true).await;
    let fixture = ClaudeChannelFixture::new(root, &daemon, "wrapper-replacement");
    fixture.append(
        root,
        json!({"type":"user","message":{"role":"user","content":"A fresh native session"}}),
    );
    // The old format lacks acceptance tracking. Its previous offer is neither
    // consumption proof nor a reason to lose delivered-but-unread mail.
    std::fs::write(
        fixture.paths.agent_dir.join("native-channel-handoffs.json"),
        json!({"incarnation":"previous","attempted":["message/unread"],"confirmed":[]}).to_string(),
    )
    .unwrap();
    let (channel, _input, received) = fixture.open(root, &daemon, "wrapper-replacement").await;
    assert!(received.recv_timeout(Duration::from_millis(1200)).is_err());
    assert_eq!(daemon.store.message("message/unread").unwrap().unwrap().status, "delivered");
    assert!(daemon.store.claims_for("message/unread", Some("message.read")).unwrap().is_empty());
    daemon.send("message/fresh", "agent/quartz", "QUARTZ FRESH SIGNAL");
    let frame = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(frame["params"]["meta"]["messageId"], "message/fresh");
    fixture.append(
        root,
        json!({"type":"user","isMeta":true,
        "message":{"role":"user","content":frame["params"]["content"]}}),
    );
    wait_until(
        "the replacement session consumes unread mail",
        Duration::from_secs(6),
        || {
            daemon
                .store
                .message("message/fresh")
                .unwrap()
                .unwrap()
                .status
                == "read"
        },
    )
    .await;
    for kind in ["message.delivered", "message.read"] {
        assert_eq!(
            daemon
                .store
                .claims_for("message/fresh", Some(kind))
                .unwrap()
                .len(),
            1
        );
    }
    assert!(received.recv_timeout(Duration::from_millis(1200)).is_err());
    assert!(stop(channel).is_empty());
    assert_eq!(daemon.store.message("message/unread").unwrap().unwrap().status, "delivered");
    assert!(daemon.store.claims_for("message/unread", Some("message.read")).unwrap().is_empty());
    daemon.stop().await;
}
