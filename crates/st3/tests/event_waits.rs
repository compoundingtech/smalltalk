//! Controls for `st3::test_support::events`, against the real events API of an in-process daemon.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::api::AppState;
use st3::client::Client;
use st3::model::ClaimInput;
use st3::store::Store;
use st3::test_support::events::{Mark, claim};
use tokio::sync::{Notify, watch};

const SEAT: &str = "agent/waiter";

/// A daemon on its own runtime thread: dropping the runtime closes its open long polls, just as
/// a daemon that exits does. Aborting only the listener task would leave them answering.
struct Daemon {
    root: PathBuf,
    store: Arc<Store>,
    event_notify: watch::Sender<u64>,
    server: Option<(
        tokio::sync::oneshot::Sender<()>,
        std::thread::JoinHandle<()>,
    )>,
}

impl Daemon {
    async fn start(root: &Path) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let store = Arc::new(Store::open(&root.join("daemon.sqlite3"), "wait-node").unwrap());
        // Durability is not under test, and the empty-page control appends 250 claims: without
        // this each one waits for its own fsync.
        store
            .connection
            .write()
            .pragma_update(None, "synchronous", "OFF")
            .unwrap();
        let mut daemon = Self {
            root: root.to_path_buf(),
            store,
            event_notify: watch::channel(0_u64).0,
            server: None,
        };
        let state = AppState {
            store: daemon.store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: daemon.event_notify.clone(),
            node: "wait-node".into(),
            state_dir: root.join("daemon"),
            pty_root: root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            owning_person: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let socket = daemon.socket();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::select! {
                    result = st3::api::serve_unix(&socket, st3::api::router(state)) => result.unwrap(),
                    _ = stopped => {}
                }
            });
        });
        daemon.server = Some((stop, thread));
        let started = Instant::now();
        // Readiness of this in-process listener is its socket accepting; nothing else waits here.
        while std::os::unix::net::UnixStream::connect(daemon.socket()).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the test daemon did not listen"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        daemon
    }

    fn socket(&self) -> PathBuf {
        self.root.join("st3.sock")
    }

    fn client(&self) -> Client {
        Client::unix(self.socket())
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
        self.event_notify
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    async fn stop(&mut self) {
        if let Some((stop, thread)) = self.server.take() {
            let _ = stop.send(());
            tokio::task::spawn_blocking(move || thread.join().unwrap())
                .await
                .unwrap();
        }
    }
}

fn observed(state: &str) -> Value {
    json!({"state": state})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_that_lasted_100_ms_is_found_from_a_mark_taken_before_it() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(root.path()).await;
    let client = daemon.client();
    let mut mark = Mark::take(&client, SEAT).await.unwrap();
    daemon.append(SEAT, "harness.observed", observed("blocked"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    daemon.append(SEAT, "harness.observed", observed("ready"));
    // A snapshot read now would only ever see "ready"; the log still has "blocked".
    let wanted = [("state", json!("blocked"))];
    let event = mark
        .event(
            &client,
            claim("harness.observed", &wanted),
            Duration::from_secs(10),
            "blocked",
        )
        .await
        .unwrap();
    assert_eq!(event.body["fields"]["state"], "blocked");
    // The mark moved past it: the next wait continues with the later state.
    let wanted = [("state", json!("ready"))];
    mark.event(
        &client,
        claim("harness.observed", &wanted),
        Duration::from_secs(10),
        "ready",
    )
    .await
    .unwrap();
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_filtered_page_moves_the_cursor_and_the_wait_is_answered_by_the_append() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(root.path()).await;
    let client = daemon.client();
    let mut mark = Mark::take(&client, SEAT).await.unwrap();
    // More rows than one page holds (200), none on the awaited subject: the daemon filters by
    // subject after its row limit, so the first page is empty but continues past them.
    for index in 0..250 {
        daemon.append(
            "agent/other",
            "harness.observed",
            json!({"state": "idle", "background_jobs": index}),
        );
    }
    let appender = {
        let store = daemon.store.clone();
        let events = daemon.event_notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            store
                .append_claim(&ClaimInput {
                    subject: SEAT.into(),
                    kind: "harness.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("state".into(), json!("ready"))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            events.send_modify(|value| *value = value.wrapping_add(1));
        })
    };
    let started = Instant::now();
    let wanted = [("state", json!("ready"))];
    mark.event(
        &client,
        claim("harness.observed", &wanted),
        Duration::from_secs(20),
        "ready",
    )
    .await
    .unwrap();
    // Answered by the append (about 300 ms), not by a poll interval or a 30 s long-poll timeout.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // Two empty pages through the 250 rows, then the open poll the append answers.
    assert!(mark.polls() <= 4, "{} polls", mark.polls());
    appender.await.unwrap();
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_gap_fails_at_once() {
    let root = tempfile::tempdir().unwrap();
    let mut first = Daemon::start(&root.path().join("first")).await;
    for index in 0..20 {
        first.append(
            "agent/other",
            "harness.observed",
            json!({"state": "idle", "background_jobs": index}),
        );
    }
    // A mark from another daemon is past this one's log: a test bug, never worth waiting for.
    let mut mark = Mark::take(&first.client(), SEAT).await.unwrap();
    first.stop().await;
    let mut second = Daemon::start(&root.path().join("second")).await;
    let started = Instant::now();
    let error = mark
        .event(
            &second.client(),
            |_| true,
            Duration::from_secs(30),
            "anything",
        )
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        format!("{error:#}").contains("outside the retained log"),
        "{error:#}"
    );
    second.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_exits_fails_the_wait_at_once() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(root.path()).await;
    let client = daemon.client();
    let mut mark = Mark::take(&client, SEAT).await.unwrap();
    let (stop, thread) = daemon.server.take().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = stop.send(());
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
    });
    let started = Instant::now();
    let error = mark
        .event(&client, |_| true, Duration::from_secs(30), "anything")
        .await
        .unwrap_err();
    // The open long poll ends with the daemon, about 300 ms in, not at the 30 s ceiling.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}: {error:#}",
        started.elapsed()
    );
    assert!(
        format!("{error:#}").contains("did not answer an event wait"),
        "{error:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ceiling_failure_names_the_predicate_and_the_last_20_events() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(root.path()).await;
    let client = daemon.client();
    let mut mark = Mark::take(&client, SEAT).await.unwrap();
    for index in 0..25 {
        daemon.append(
            SEAT,
            "harness.observed",
            json!({"state": "idle", "background_jobs": index}),
        );
    }
    let wanted = [("state", json!("ready"))];
    let error = mark
        .event(
            &client,
            claim("harness.observed", &wanted),
            Duration::from_millis(500),
            "the seat becomes ready",
        )
        .await
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("the seat becomes ready: not seen within the ceiling"),
        "{text}"
    );
    assert!(text.contains("last 20 events on agent/waiter"), "{text}");
    assert!(
        text.contains("\"background_jobs\":24") && !text.contains("\"background_jobs\":4,"),
        "{text}"
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_wait_rechecks_only_after_a_new_event() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(root.path()).await;
    let client = daemon.client();
    let mut mark = Mark::take(&client, SEAT).await.unwrap();
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let appender = {
        let store = daemon.store.clone();
        let events = daemon.event_notify.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            ready.store(true, std::sync::atomic::Ordering::SeqCst);
            store
                .append_claim(&ClaimInput {
                    subject: SEAT.into(),
                    kind: "harness.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("state".into(), json!("ready"))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            events.send_modify(|value| *value = value.wrapping_add(1));
        })
    };
    let counted = checks.clone();
    let value = mark
        .state(
            &client,
            || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let ready = ready.load(std::sync::atomic::Ordering::SeqCst);
                async move { Ok(ready.then_some("ready")) }
            },
            Duration::from_secs(10),
            "ready",
        )
        .await
        .unwrap();
    assert_eq!(value, "ready");
    // Once before any event, once after the one event: no interval in between.
    assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 2);
    appender.await.unwrap();
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_subject_keeps_its_own_mark_so_a_wait_on_one_skips_nothing_of_another() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(root.path()).await;
    let client = daemon.client();
    // One mark per subject, both before the action. One cursor shared by both waits skips b's
    // "ready": the daemon moves the cursor past the rows it filtered out for agent/a.
    let mut a = Mark::take(&client, "agent/a").await.unwrap();
    let mut b = Mark::take(&client, "agent/b").await.unwrap();
    daemon.append("agent/a", "harness.observed", observed("idle"));
    daemon.append("agent/b", "harness.observed", observed("ready"));
    daemon.append("agent/a", "harness.observed", observed("ready"));
    let wanted = [("state", json!("ready"))];
    a.event(
        &client,
        claim("harness.observed", &wanted),
        Duration::from_secs(10),
        "a ready",
    )
    .await
    .unwrap();
    let event = b
        .event(
            &client,
            claim("harness.observed", &wanted),
            Duration::from_secs(10),
            "b ready",
        )
        .await
        .unwrap();
    assert_eq!(event.subject, "agent/b");
    daemon.stop().await;
}
