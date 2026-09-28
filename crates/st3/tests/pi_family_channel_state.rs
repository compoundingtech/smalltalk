#![cfg(unix)]

//! The pi-family channel as the pi and omp extensions run it: `st3 driver pi-channel` (or
//! `omp-channel`) is a child process that reads the extension's frames on stdin and publishes the
//! seat's harness state to the daemon. Each test feeds the frames the extension sends for one
//! situation and reads the seat's current harness view from the store.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::{ClaimInput, CurrentHarnessView};
use st3::store::Store;
use tokio::sync::{Notify, watch};

const IDENTITY: &str = "pi-seat";
const SUBJECT: &str = "agent/pi-seat";

struct Daemon {
    root: tempfile::TempDir,
    socket: PathBuf,
    store: Arc<Store>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Daemon {
    /// A daemon whose store already records the seat's running runtime incarnation.
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let store = Arc::new(Store::open_memory("pi-family-channel").unwrap());
        store
            .append_claim(&ClaimInput {
                subject: SUBJECT.into(),
                kind: "runtime.observed".into(),
                actor: Some(SUBJECT.into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String(IDENTITY.into())),
                    ("incarnation_id".into(), Value::String("pi-seat:i1".into())),
                    ("status".into(), Value::String("running".into())),
                    ("reachability".into(), Value::String("local".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let server = serve(root.path(), &socket, &store).await;
        Self {
            root,
            socket,
            store,
            server,
        }
    }

    /// One channel process, as the extension spawns it for one pi session: it receives `frames`
    /// on stdin, then its stdin closes the way the extension closes it at a session boundary.
    async fn channel(&self, driver: &str, frames: &[Value]) {
        let mut command = channel_command(self, driver);
        command.stdout(Stdio::null());
        let input = frames
            .iter()
            .map(|frame| format!("{frame}\n"))
            .collect::<String>();
        let status = tokio::task::spawn_blocking(move || {
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait().unwrap()
        })
        .await
        .unwrap();
        assert!(status.success(), "the channel exited with {status}");
    }

    fn harness(&self) -> CurrentHarnessView {
        self.store
            .current_harness(SUBJECT)
            .unwrap()
            .expect("the seat has a current harness view")
    }
}

/// Serve the API for `store` on `socket`, and wait until the socket is accepting.
async fn serve(
    root: &std::path::Path,
    socket: &std::path::Path,
    store: &Arc<Store>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "pi-family-channel".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.to_path_buf();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());
    server
}

fn channel_command(daemon: &Daemon, driver: &str) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("st3"));
    command
        .arg("--endpoint")
        .arg(&daemon.socket)
        .arg("--catalog")
        .arg(daemon.root.path().join("catalog"))
        .arg("driver")
        .arg(driver)
        .arg("--identity")
        .arg(IDENTITY)
        .env_remove("PTY_ROOT")
        .env_remove("ST_AGENT")
        .env_remove("ST3_ENDPOINT")
        // The seat's session token, stable for the life of the pi process.
        .env("ST2_PI_CHANNEL_SESSION", "seat-session-token")
        .env("ST2_OMP_CHANNEL_SESSION", "seat-session-token")
        .stdin(Stdio::piped())
        .stderr(Stdio::null());
    command
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn state(word: &str) -> Value {
    json!({"type": "state", "state": word})
}

/// Plain turn edges reach the seat's harness view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_edges_from_the_channel_become_the_seats_harness_state() {
    let daemon = Daemon::start().await;
    daemon.channel("pi-channel", &[state("active")]).await;
    assert_eq!(daemon.harness().state, "working");
}

/// pi replaces its session (`/new`, `/resume`, `/fork`) inside one process. The extension closes
/// the old channel and opens a new one for the new session. The new session's turn must reach the
/// seat's harness view: a seat that is working must not keep reading the old session's idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: a replacement channel's state frames reuse the old channel's claim keys and are dropped"]
async fn a_new_sessions_channel_publishes_its_turn_after_a_session_switch() {
    let daemon = Daemon::start().await;
    daemon
        .channel("pi-channel", &[state("active"), state("idle")])
        .await;
    assert_eq!(daemon.harness().state, "idle");

    // `/new`: the replacement channel's first turn starts.
    daemon.channel("pi-channel", &[state("active")]).await;
    assert_eq!(
        daemon.harness().state,
        "working",
        "the new session is mid-turn but the seat still reads the old session's idle"
    );
}

/// omp's `ask` tool stops the turn on a question for a person; the extension reports it as a
/// human wait. The seat's harness view must say a person is being waited on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: the pi-family channel drops blockedOn, so an omp question reads as plain working"]
async fn an_omp_question_is_published_as_a_human_wait() {
    let daemon = Daemon::start().await;
    daemon
        .channel(
            "omp-channel",
            &[
                state("active"),
                json!({
                    "type": "state",
                    "state": "active",
                    "blockedOn": "human",
                    "ask": "question",
                    "reason": "Which branch should I deploy?",
                }),
            ],
        )
        .await;
    let harness = daemon.harness();
    assert_eq!(
        harness.blocked_on.as_deref(),
        Some("human"),
        "the seat is waiting on a person but reads {harness:?}"
    );
}

/// An omp turn that ends on a provider error it will not retry: omp sends a `turn` frame carrying
/// the error and no idle edge follows. Nothing runs any more, so the seat must not keep reading
/// plain working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: an omp turn that ends on a provider error leaves the seat reading working"]
async fn an_omp_turn_that_ends_on_a_provider_error_does_not_stay_working() {
    let daemon = Daemon::start().await;
    daemon
        .channel(
            "omp-channel",
            &[
                state("active"),
                json!({
                    "type": "turn",
                    "error": {"reason": "402 insufficient credits", "errorId": 1},
                }),
            ],
        )
        .await;
    let harness = daemon.harness();
    assert!(
        harness.state != "working" || harness.reason.is_some(),
        "the turn ended on an error, but the seat reads plain {harness:?}"
    );
}

/// The daemon's API is briefly unreachable (a daemon restart) just as a turn starts. The extension
/// sends each turn edge once, so the channel is the only holder of that report; once the API is
/// back, the seat's harness view must catch up to the working turn rather than keep its last state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: a state report the pi-family channel could not post is dropped and never recovered"]
async fn a_turn_edge_sent_while_the_api_is_unreachable_reaches_the_seat_once_it_returns() {
    use std::io::BufRead as _;

    let mut daemon = Daemon::start().await;
    let mut command = channel_command(&daemon, "pi-channel");
    command.stdout(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    // The hello is written once the channel has bound its incarnation and entered its loop.
    let hello = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        (line, stdout)
    })
    .await
    .unwrap();
    assert!(hello.0.contains("\"hello\""), "{}", hello.0);
    writeln!(stdin, "{}", state("idle")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(daemon.harness().state, "idle");

    daemon.server.abort();
    let _ = (&mut daemon.server).await;
    std::fs::remove_file(&daemon.socket).unwrap();
    writeln!(stdin, "{}", state("active")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    daemon.server = serve(daemon.root.path(), &daemon.socket, &daemon.store).await;

    let mut caught_up = false;
    for _ in 0..50 {
        if daemon.harness().state == "working" {
            caught_up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "the channel is still running"
    );
    drop(stdin);
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
    assert!(
        caught_up,
        "the turn is running, but five seconds after the API returned the seat still reads {:?}",
        daemon.harness().state
    );
}
