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
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Daemon {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            socket: root.join("st3.sock"),
            store: Arc::new(Store::open_memory("restart-node").unwrap()),
            server: None,
        }
    }

    async fn start(&mut self) {
        let state = AppState {
            store: self.store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
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
            let _ = st3::api::serve_unix(&socket, st3::api::router(state)).await;
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
        assert!(std::os::unix::net::UnixStream::connect(&self.socket).is_err());
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
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("st3"));
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
        .arg("--endpoint")
        .arg(socket);
    for directory in ["workspace", "home", "state", "config", "runtime", "drivers"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    command
}

fn driver_log(root: &Path) -> String {
    std::fs::read_to_string(root.join("state/st3/driver-api-warnings.log")).unwrap_or_default()
}

fn files_containing(directory: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle)) {
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
    tokio::time::sleep(Duration::from_millis(1_500)).await;
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
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_alive(&mut driver, "a running Claude driver");
    assert!(files_containing(&root.join("drivers"), "AMBER LANTERN").is_empty());
    daemon.start().await;
    wait_until(
        "the driver projects the message to the Claude channel",
        Duration::from_secs(10),
        || !files_containing(&root.join("drivers"), "AMBER LANTERN").is_empty(),
    )
    .await;
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

#[test]
fn a_cli_command_says_the_daemon_is_unreachable_and_never_to_start_it() {
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

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_in_a_network_sandbox_can_reach_its_local_runtime() {
    if Command::new("bwrap").arg("--version").output().is_err() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::new(root.path());
    daemon.start().await;
    let socket = daemon.socket.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "--proc", "/proc", "--unshare-net", "--"])
            .arg(assert_cmd::cargo::cargo_bin!("st3"))
            .arg("--endpoint")
            .arg(socket)
            .args(["--json", "machines", "--limit", "1"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "sandboxed runtime request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _: Value = serde_json::from_slice(&output.stdout).unwrap();
}
