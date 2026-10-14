//! `st conversations wait` returns a person's reply as soon as it lands, or `no reply` at the
//! deadline, and marks the reply read so native delivery does not repeat it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;
use st3::api::AppState;
use st3::store::Store;
use tokio::sync::{Notify, watch};

const WORKER: &str = "agent/example/worker";
const PERSON: &str = "person/avery";
const OTHER: &str = "person/ada";
const PEER: &str = "agent/example/peer";

/// Every read the daemon served, as `METHOD path?query`, so a test can count what a command costs.
#[derive(Default)]
struct Requests(Mutex<Vec<String>>);

impl Requests {
    fn reads(&self, prefix: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with(&format!("GET {prefix}")))
            .count()
    }
}

async fn record(State(requests): State<Arc<Requests>>, request: Request, next: Next) -> Response {
    let target = request.uri().path_and_query().map(ToString::to_string).unwrap_or_default();
    requests.0.lock().unwrap().push(format!("{} {target}", request.method()));
    next.run(request).await
}

/// An in-process daemon on its own socket, and the store behind it.
async fn daemon(root: &Path) -> (PathBuf, Arc<Store>, tokio::task::JoinHandle<()>) {
    let (socket, store, requests, server) = counted_daemon(root).await;
    drop(requests);
    (socket, store, server)
}

async fn counted_daemon(
    root: &Path,
) -> (PathBuf, Arc<Store>, Arc<Requests>, tokio::task::JoinHandle<()>) {
    let socket = root.join("st3.sock");
    let state = AppState {
        owning_person: None,
        // A file, like the daemon's own store: an in-memory store locks a table against a reader
        // while another request writes, which the counting test below does constantly.
        store: Arc::new(Store::open(&root.join("graph.sqlite3"), "message-wait").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "message-wait".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    };
    let store = state.store.clone();
    let requests = Arc::new(Requests::default());
    let app = st3::api::router(state)
        .layer(axum::middleware::from_fn_with_state(requests.clone(), record));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, app).await.unwrap();
    });
    wait_for(&socket).await;
    (socket, store, requests, server)
}

async fn wait_for(socket: &Path) {
    for _ in 0..200 {
        if socket.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("{} did not start", socket.display());
}

/// Run the st CLI against `socket` with none of this process's st environment, so a harness
/// running the suite lends it no seat identity, projection root or incarnation.
async fn st(socket: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let binary = test_bin!("st3-fixture").to_path_buf();
    let socket = socket.to_path_buf();
    let env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let mut command = st3::test_support::command(binary);
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy();
            if name.starts_with("ST_") || name.starts_with("ST3_") {
                command.env_remove(name.as_ref());
            }
        }
        command
            .envs(env)
            .arg("--endpoint")
            .arg(socket)
            .args(["--daemon-wait", "0"])
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn value(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "decode CLI JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}


async fn send(socket: &Path, from: &str, to: &str, body: &str) -> Value {
    value(
        &st(
            socket,
            &[],
            &["--json", "conversations", "send", to, "--from", from, "--body", body],
        )
        .await,
    )
}

fn wait_args<'a>(timeout: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "--json", "conversations", "wait", "--as", WORKER, "--from", PERSON, "--timeout", timeout,
    ];
    args.extend_from_slice(extra);
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_with_no_reply_returns_no_reply_at_the_deadline() {
    let root = tempfile::tempdir().unwrap();
    let (socket, store, server) = daemon(root.path()).await;
    // Mail that predates the wait, and mail from someone else, is not the reply.
    send(&socket, PERSON, WORKER, "Sent before the wait.").await;
    let started = Instant::now();
    let waiting = {
        let socket = socket.clone();
        tokio::spawn(async move { st(&socket, &[], &wait_args("2s", &[])).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&socket, OTHER, WORKER, "Someone else answered.").await;
    let answer = value(&waiting.await.unwrap());
    assert_eq!(answer["reply"], false, "{answer}");
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "returned after {:?}, before the deadline",
        started.elapsed()
    );
    assert!(
        store
            .messages(Some(WORKER), true)
            .unwrap()
            .iter()
            .all(|message| message.status != "read"),
        "a wait that found no reply read mail"
    );

    let human = st(
        &socket,
        &[],
        &["conversations", "wait", "--as", WORKER, "--from", PERSON, "--timeout", "0"],
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&human.stdout).trim(), "no reply");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_returns_the_wait_early_and_is_marked_read() {
    let root = tempfile::tempdir().unwrap();
    let (socket, store, server) = daemon(root.path()).await;
    let started = Instant::now();
    let waiting = {
        let socket = socket.clone();
        tokio::spawn(async move { st(&socket, &[], &wait_args("30s", &[])).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sent = send(&socket, PERSON, WORKER, "Yes, start now.").await;
    let answer = value(&waiting.await.unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "waited {:?} for a reply that had landed",
        started.elapsed()
    );
    assert_eq!(answer["reply"], true, "{answer}");
    assert_eq!(answer["message"]["content"], "Yes, start now.");
    assert_eq!(answer["message"]["subject"], sent["subject"]);
    let stored = store.messages(Some(WORKER), true).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].status, "read");

    // The reply is spent: a second wait does not return it again.
    let again = value(&st(&socket, &[], &wait_args("0", &["--after", sent["subject"].as_str().unwrap()])).await);
    assert_eq!(again["reply"], false, "{again}");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_counts_a_reply_that_beat_the_wait() {
    let root = tempfile::tempdir().unwrap();
    let (socket, _store, server) = daemon(root.path()).await;
    // People have no inbox, so the question the reply follows is any message the seat sent just
    // before it began to wait; the person answers it in the chat, which reaches the seat as mail.
    let question = send(&socket, WORKER, PEER, "Shall I start?").await;
    send(&socket, PERSON, WORKER, "Yes.").await;
    let answer = value(
        &st(&socket, &[], &wait_args("0", &["--after", question["subject"].as_str().unwrap()])).await,
    );
    assert_eq!(answer["reply"], true, "{answer}");
    assert_eq!(answer["message"]["content"], "Yes.");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_events_do_not_make_a_wait_read_the_mailbox() {
    const OTHER_AGENT: &str = "agent/example/other";
    let root = tempfile::tempdir().unwrap();
    let (socket, _store, requests, server) = counted_daemon(root.path()).await;

    // What ten unrelated sends cost on their own, with nobody waiting.
    for index in 0..10 {
        send(&socket, OTHER_AGENT, PEER, &format!("Baseline {index}.")).await;
    }
    let baseline = requests.reads("/v1/messages");
    assert_eq!(requests.reads("/v1/events/page"), 0, "nothing was waiting");

    // The same traffic while a seat waits three seconds for its person.
    let waiting = {
        let socket = socket.clone();
        tokio::spawn(async move { st(&socket, &[], &wait_args("3s", &[])).await })
    };
    for index in 0..10 {
        send(&socket, OTHER_AGENT, PEER, &format!("Unrelated {index}.")).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let answer = value(&waiting.await.unwrap());
    assert_eq!(answer["reply"], false, "{answer}");
    let during = requests.reads("/v1/messages") - baseline;
    assert!(
        during <= baseline + 2,
        "the wait read the mailbox: {during} reads against {baseline} for the sends alone: {:?}",
        requests.0.lock().unwrap()
    );
    let polls = requests.reads("/v1/events/page");
    assert!(polls <= 10, "{polls} event reads in three seconds are not paced");

    // An early reply, amid the same traffic, still returns within the pace interval.
    let waiting = {
        let socket = socket.clone();
        tokio::spawn(async move { st(&socket, &[], &wait_args("30s", &[])).await })
    };
    tokio::time::sleep(Duration::from_millis(700)).await;
    send(&socket, OTHER_AGENT, PEER, "Unrelated again.").await;
    send(&socket, PERSON, WORKER, "Go ahead.").await;
    let landed = Instant::now();
    let answer = value(&waiting.await.unwrap());
    assert_eq!(answer["reply"], true, "{answer}");
    assert_eq!(answer["message"]["content"], "Go ahead.");
    assert!(
        landed.elapsed() < Duration::from_millis(1500),
        "the reply took {:?} to reach the wait",
        landed.elapsed()
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_longer_than_ten_minutes_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let (socket, _store, server) = daemon(root.path()).await;
    let refused = st(&socket, &[], &wait_args("11m", &[])).await;
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(error.contains("at most 10m"), "{error}");
    let allowed = st(&socket, &[], &wait_args("0", &[])).await;
    assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stderr));
    server.abort();
}
