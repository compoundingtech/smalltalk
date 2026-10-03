//! A send or reply that st did not confirm may still have landed. Running the same command again
//! reports the message it already sent instead of sending it twice, and `conversations status
//! --idempotency-key` says whether an unconfirmed send landed.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::Value;
use st3::api::AppState;
use st3::store::Store;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};

const WORKER: &str = "agent/example/worker";
const PERSON: &str = "person/avery";

/// An in-process daemon on its own socket, and the store behind it.
async fn daemon(root: &Path) -> (PathBuf, Arc<Store>, tokio::task::JoinHandle<()>) {
    let socket = root.join("st3.sock");
    let state = AppState {
        store: Arc::new(Store::open_memory("message-send-once").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "message-send-once".into(),
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

/// Answers the proxy loses while the daemon still applies the request, as under load.
#[derive(Default)]
struct Losses {
    /// How many more `POST /v1/messages` answers to lose.
    sends: AtomicUsize,
    /// Whether mailbox page reads go unanswered.
    page_reads: AtomicBool,
    /// Whether page reads go unanswered once a send has been answered.
    page_reads_after_a_send: AtomicBool,
}

/// Forward every request on `front` to the daemon on `back`, and close the connection without a
/// byte in place of each answer `losses` names.
async fn lossy_proxy(
    front: PathBuf,
    back: PathBuf,
    losses: Arc<Losses>,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(&front).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut caller, _) = listener.accept().await.unwrap();
            let back = back.clone();
            let losses = losses.clone();
            tokio::spawn(async move {
                let request = read_request(&mut caller).await;
                let mut daemon = UnixStream::connect(&back).await.unwrap();
                daemon.write_all(&request).await.unwrap();
                let mut answer = Vec::new();
                daemon.read_to_end(&mut answer).await.unwrap();
                let line = String::from_utf8_lossy(&request[..request.len().min(64)]).to_string();
                let lose = if line.starts_with("POST /v1/messages ") {
                    let lose = losses
                        .sends
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                            left.checked_sub(1)
                        })
                        .is_ok();
                    if !lose && losses.page_reads_after_a_send.load(Ordering::SeqCst) {
                        losses.page_reads.store(true, Ordering::SeqCst);
                    }
                    lose
                } else if line.starts_with("GET /v1/messages/page") {
                    losses.page_reads.load(Ordering::SeqCst)
                } else {
                    false
                };
                if !lose {
                    let _ = caller.write_all(&answer).await;
                }
            });
        }
    })
}

/// One whole HTTP request: its head, then the body its `Content-Length` names.
async fn read_request(stream: &mut UnixStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the caller closed before a whole request");
        request.extend_from_slice(&buffer[..read]);
        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
        if request.len() >= end + 4 + length {
            return request;
        }
    }
}

/// Run the st CLI against `socket` with none of this process's st environment, so a harness
/// running the suite lends it no seat identity, projection root or incarnation.
async fn st(socket: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn message_count(store: &Store) -> usize {
    store.messages(None, true).unwrap().len()
}

const SEND: &[&str] = &[
    "--json",
    "conversations",
    "send",
    WORKER,
    "--from",
    PERSON,
    "--subject",
    "Merge train",
    "--body",
    "The merge train is live.",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_the_same_send_twice_sends_one_message() {
    let root = tempfile::tempdir().unwrap();
    let (socket, store, server) = daemon(root.path()).await;

    let first = value(&st(&socket, &[], SEND).await);
    let second = value(&st(&socket, &[], SEND).await);
    assert_eq!(message_count(&store), 1, "a repeated send sent twice");
    assert_eq!(first["already_sent"], false);
    assert_eq!(second["subject"], first["subject"]);
    assert_eq!(second["already_sent"], true);
    assert_eq!(second["sent_at"], first["sent_at"]);
    assert_eq!(second["idempotency_key"], first["idempotency_key"]);
    assert!(
        first["idempotency_key"]
            .as_str()
            .unwrap()
            .starts_with("st3-message:v1:"),
        "{first}"
    );

    // The human form says so too, and still prints the message on stdout.
    let human = st(&socket, &[], &SEND[1..]).await;
    assert!(human.status.success(), "{}", stderr(&human));
    assert_eq!(
        String::from_utf8_lossy(&human.stdout).trim(),
        first["subject"].as_str().unwrap()
    );
    assert!(
        stderr(&human).contains("already sent"),
        "{}",
        stderr(&human)
    );
    assert_eq!(message_count(&store), 1);

    // Different words are a different message.
    let mut other = SEND.to_vec();
    *other.last_mut().unwrap() = "The merge train is paused.";
    let other = value(&st(&socket, &[], &other).await);
    assert_eq!(other["already_sent"], false);
    assert_ne!(other["subject"], first["subject"]);
    assert_eq!(message_count(&store), 2);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_the_same_reply_twice_sends_one_reply() {
    let root = tempfile::tempdir().unwrap();
    let (socket, store, server) = daemon(root.path()).await;
    let original = value(
        &st(
            &socket,
            &[],
            &[
                "--json",
                "conversations",
                "send",
                PERSON,
                "--from",
                WORKER,
                "--subject",
                "Question",
                "--body",
                "Shall I start?",
            ],
        )
        .await,
    );
    let parent = original["subject"].as_str().unwrap();
    let reply = [
        "--json",
        "conversations",
        "reply",
        parent,
        "--from",
        PERSON,
        "--body",
        "Yes, start now.",
    ];

    let first = value(&st(&socket, &[], &reply).await);
    let second = value(&st(&socket, &[], &reply).await);
    assert_eq!(message_count(&store), 2, "a repeated reply sent twice");
    assert_eq!(first["already_sent"], false);
    assert_eq!(second["subject"], first["subject"]);
    assert_eq!(second["in_reply_to"], parent);
    assert_eq!(second["already_sent"], true);

    // A reply's own lifecycle writes carry keys too; they are not sends.
    let settled = format!(
        "reply-settled:{}:closed",
        first["subject"].as_str().unwrap()
    );
    let status = value(
        &st(
            &socket,
            &[],
            &[
                "--json",
                "conversations",
                "status",
                "--idempotency-key",
                &settled,
            ],
        )
        .await,
    );
    assert_eq!(status["landed"], false, "{status}");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_whose_answer_was_lost_is_found_and_never_sent_twice() {
    let root = tempfile::tempdir().unwrap();
    let (back, store, server) = daemon(root.path()).await;
    let front = root.path().join("front.sock");
    let losses = Arc::new(Losses::default());
    let proxy = lossy_proxy(front.clone(), back, losses.clone()).await;
    // The daemon applies the send and its in-process retry, and neither answer arrives.
    losses.sends.store(2, Ordering::SeqCst);

    let lost = st(&front, &[], SEND).await;
    assert!(!lost.status.success());
    assert_eq!(message_count(&store), 1);
    let rerun = st(&front, &[], SEND).await;
    assert_eq!(
        message_count(&store),
        1,
        "running the send again sent twice"
    );
    let rerun = value(&rerun);
    assert_eq!(rerun["already_sent"], true);

    // The failure named the key, the command that checks it, and that running it again is safe.
    let error = stderr(&lost);
    let key = rerun["idempotency_key"].as_str().unwrap();
    assert!(
        error.contains(&format!("st conversations status --idempotency-key {key}")),
        "{error}"
    );
    assert!(error.contains("safe"), "{error}");
    let status = value(
        &st(
            &front,
            &[],
            &[
                "--json",
                "conversations",
                "status",
                "--idempotency-key",
                key,
            ],
        )
        .await,
    );
    assert_eq!(status["landed"], true, "{status}");
    assert_eq!(status["id"], rerun["subject"]);
    proxy.abort();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_by_key_says_whether_a_send_landed() {
    let root = tempfile::tempdir().unwrap();
    let (socket, _store, server) = daemon(root.path()).await;
    let sent = value(&st(&socket, &[], SEND).await);
    let key = sent["idempotency_key"].as_str().unwrap();

    let landed = value(
        &st(
            &socket,
            &[],
            &[
                "--json",
                "conversations",
                "status",
                "--idempotency-key",
                key,
            ],
        )
        .await,
    );
    assert_eq!(landed["landed"], true, "{landed}");
    assert_eq!(landed["idempotency_key"], key);
    assert_eq!(landed["id"], sent["subject"]);
    assert_eq!(landed["from"], PERSON);
    assert_eq!(landed["to"], WORKER);
    assert_eq!(landed["sent_at"], sent["sent_at"]);
    assert!(landed["delivery"]["phase"].is_string(), "{landed}");
    let human = st(
        &socket,
        &[],
        &["conversations", "status", "--idempotency-key", key],
    )
    .await;
    assert!(human.status.success(), "{}", stderr(&human));
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("landed"), "{human}");
    assert!(human.contains(sent["subject"].as_str().unwrap()), "{human}");

    let unknown = value(
        &st(
            &socket,
            &[],
            &[
                "--json",
                "conversations",
                "status",
                "--idempotency-key",
                "st3-message:v1:never-sent",
            ],
        )
        .await,
    );
    assert_eq!(unknown["landed"], false, "{unknown}");
    assert_eq!(unknown["idempotency_key"], "st3-message:v1:never-sent");
    let human = st(
        &socket,
        &[],
        &[
            "conversations",
            "status",
            "--idempotency-key",
            "st3-message:v1:never-sent",
        ],
    )
    .await;
    assert!(human.status.success(), "{}", stderr(&human));
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("not landed"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeat_in_the_next_hour_finds_the_first_send_and_later_ones_send_anew() {
    let root = tempfile::tempdir().unwrap();
    let (socket, store, server) = daemon(root.path()).await;
    let at = |hour: &'static str| [("ST3_MESSAGE_KEY_HOUR", hour)];

    let first = value(&st(&socket, &at("490000"), SEND).await);
    // An hour boundary between a lost answer and its retry still finds the first send.
    let next_hour = value(&st(&socket, &at("490001"), SEND).await);
    assert_eq!(
        message_count(&store),
        1,
        "the next hour's repeat sent twice"
    );
    assert_eq!(first["already_sent"], false);
    assert_eq!(next_hour["already_sent"], true);
    assert_eq!(next_hour["subject"], first["subject"]);
    assert_eq!(next_hour["idempotency_key"], first["idempotency_key"]);
    // Two hours on, the same words are a new message.
    let later = value(&st(&socket, &at("490002"), SEND).await);
    assert_eq!(later["already_sent"], false);
    assert_ne!(later["subject"], first["subject"]);
    assert_eq!(message_count(&store), 2);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_key_names_one_message_at_any_hour() {
    let root = tempfile::tempdir().unwrap();
    let (socket, store, server) = daemon(root.path()).await;
    let keyed = |body: &'static str| {
        let mut args = SEND.to_vec();
        *args.last_mut().unwrap() = body;
        args.extend(["--idempotency-key", "release-note-1"]);
        args
    };

    let first = value(
        &st(
            &socket,
            &[("ST3_MESSAGE_KEY_HOUR", "490000")],
            &keyed("Release is out."),
        )
        .await,
    );
    assert_eq!(first["idempotency_key"], "release-note-1");
    assert_eq!(first["already_sent"], false);
    let days_later = value(
        &st(
            &socket,
            &[("ST3_MESSAGE_KEY_HOUR", "490100")],
            &keyed("Release is out."),
        )
        .await,
    );
    assert_eq!(days_later["subject"], first["subject"]);
    assert_eq!(days_later["already_sent"], true);
    assert_eq!(message_count(&store), 1);

    // The same key with different words is refused, not sent.
    let changed = st(&socket, &[], &keyed("Release is delayed.")).await;
    assert!(!changed.status.success());
    assert!(
        stderr(&changed).contains("idempotency-mismatch"),
        "{}",
        stderr(&changed)
    );
    // Identical words under another explicit key are another message: no hour, no lookback.
    let mut other = keyed("Release is out.");
    *other.last_mut().unwrap() = "release-note-2";
    let other = value(&st(&socket, &[], &other).await);
    assert_eq!(other["already_sent"], false);
    assert_ne!(other["subject"], first["subject"]);
    assert_eq!(message_count(&store), 2);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_landed_send_succeeds_when_the_projection_cannot_be_refreshed() {
    let root = tempfile::tempdir().unwrap();
    let (back, store, server) = daemon(root.path()).await;
    let front = root.path().join("front.sock");
    let losses = Arc::new(Losses::default());
    let proxy = lossy_proxy(front.clone(), back.clone(), losses.clone()).await;
    let projection = root.path().join("mailbox");
    std::fs::create_dir_all(&projection).unwrap();
    let projection = projection.to_str().unwrap();

    // The command refreshes the projection before it sends; lose only the refresh after it.
    losses.page_reads_after_a_send.store(true, Ordering::SeqCst);
    let sent = st(&front, &[("ST3_MESSAGE_ROOT", projection)], &SEND[1..]).await;
    assert_eq!(message_count(&store), 1);
    assert!(
        sent.status.success(),
        "a landed send failed: {}",
        stderr(&sent)
    );
    let subject = store.messages(None, true).unwrap()[0].subject.clone();
    assert_eq!(String::from_utf8_lossy(&sent.stdout).trim(), subject);
    let warning = stderr(&sent);
    assert!(warning.contains(&subject), "{warning}");
    assert!(warning.contains("st3-message:v1:"), "{warning}");
    proxy.abort();
    server.abort();
}
