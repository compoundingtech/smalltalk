//! `st conversations wait` returns a person's reply as soon as it lands, or `no reply` at the
//! deadline, and marks the reply read so native delivery does not repeat it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use st3::api::AppState;
use st3::store::Store;
use tokio::sync::{Notify, watch};

const WORKER: &str = "agent/example/worker";
const PERSON: &str = "person/avery";
const OTHER: &str = "person/blake";

/// An in-process daemon on its own socket, and the store behind it.
async fn daemon(root: &Path) -> (PathBuf, Arc<Store>, tokio::task::JoinHandle<()>) {
    let socket = root.join("st3.sock");
    let state = AppState {
        store: Arc::new(Store::open_memory("message-wait").unwrap()),
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
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, st3::api::router(state))
            .await
            .unwrap();
    });
    wait_for(&socket).await;
    (socket, store, server)
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
    let question = send(&socket, WORKER, PERSON, "Shall I start?").await;
    send(&socket, PERSON, WORKER, "Yes.").await;
    let answer = value(
        &st(&socket, &[], &wait_args("0", &["--after", question["subject"].as_str().unwrap()])).await,
    );
    assert_eq!(answer["reply"], true, "{answer}");
    assert_eq!(answer["message"]["content"], "Yes.");
    server.abort();
}
