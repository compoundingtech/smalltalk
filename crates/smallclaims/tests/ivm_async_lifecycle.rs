//! Deterministic worker shutdown schedules in a real Store. No timing/cost qualification.
use anyhow::Result;
use serde_json::json;
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    ivm::{
        Contribution, Definition, Readiness, View, Views,
        asynchronous::{self, Limits, PageStatus, Worker},
        events::{self, Publisher},
        runtime::ViewRuntime,
        source_cut,
    },
    store::canonical,
};
use std::{
    future::Future,
    sync::{Arc, Mutex, mpsc},
    task::Poll,
};
use tokio::{
    sync::oneshot,
    time::{Duration, timeout},
};

struct PausedStatus {
    pause: Mutex<Option<(oneshot::Sender<()>, mpsc::Receiver<()>)>>,
    panic: bool,
}
impl View for PausedStatus {
    fn definition(&self) -> Definition {
        Definition {
            name: "status",
            fingerprint: "fixture.paused-status.v1",
            kinds: &["agent.status"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        rank: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        assert!(!self.panic, "fixture poison input");
        if let Some((entered, release)) = self.pause.lock().unwrap().take() {
            let _ = entered.send(());
            release.recv_timeout(Duration::from_secs(5))?;
        }
        Ok(vec![Contribution {
            key: claim.subject.clone(),
            register: "status".into(),
            value: claim.body["fields"]["status"].clone(),
            rank: canonical::sortable_key(rank),
        }])
    }
}
fn fixture(view: PausedStatus) -> (Arc<Store>, Arc<ViewRuntime>, Publisher) {
    let runtime = Arc::new(
        ViewRuntime::asynchronous(
            Views::new(vec![Box::new(view)]).unwrap(),
            Limits {
                page_rows: 1,
                ..Limits::default()
            },
        )
        .unwrap(),
    );
    let store = Arc::new(Store::open_memory("alder", runtime.clone()).unwrap());
    store
        .connection
        .batched(|tx| events::install(tx, 128))
        .unwrap()
        .unwrap();
    let publisher = Publisher::attach(&store, 1).unwrap();
    (store, runtime, publisher)
}
fn append(store: &Store, subject: &str) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "agent.status".into(),
            actor: None,
            fields: serde_json::from_value(json!({"status":"working"})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

#[tokio::test]
async fn stop_finishes_the_owned_page_and_leaves_the_next_input_uncertified() {
    let (entered, observed) = oneshot::channel();
    let (release, paused) = mpsc::channel();
    let (store, runtime, publisher) = fixture(PausedStatus {
        pause: Mutex::new(Some((entered, paused))),
        panic: false,
    });
    let first = append(&store, "agent/ada");
    let second = append(&store, "agent/robin");
    let worker = Worker::start(store.clone(), runtime.clone(), publisher.subscribe()).unwrap();
    timeout(Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    let mut stopping = Box::pin(worker.stop());
    // Poll exactly once: cancellation is signalled while the blocking page owns the writer.
    std::future::poll_fn(|cx| match stopping.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("stop returned before its owned page completed"),
    })
    .await;
    let writing_store = store.clone();
    let (writing, queued) = oneshot::channel();
    let writer = tokio::task::spawn_blocking(move || {
        writing.send(()).unwrap();
        append(&writing_store, "agent/avery")
    });
    timeout(Duration::from_secs(5), queued)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    timeout(Duration::from_secs(5), stopping)
        .await
        .unwrap()
        .unwrap();
    let third = timeout(Duration::from_secs(5), writer)
        .await
        .unwrap()
        .unwrap();
    let connection = store.readers.get();
    let cut = source_cut(&connection).unwrap().unwrap();
    assert_eq!(cut.projected, first.store_index);
    assert!(third.store_index > second.store_index);
    assert_eq!(cut.admitted, third.store_index);
    assert_eq!(asynchronous::state(&connection).unwrap().retained_rows, 2);
    assert_eq!(
        runtime
            .views
            .readiness(&connection, "status", cut.epoch)
            .unwrap(),
        Readiness::SourcePending
    );
    drop(connection);
    assert_eq!(
        asynchronous::step(&store, &runtime).unwrap().status,
        PageStatus::More
    );
    assert_eq!(
        asynchronous::step(&store, &runtime).unwrap().status,
        PageStatus::CaughtUp
    );
    assert!(matches!(
        runtime
            .views
            .readiness(&store.readers.get(), "status", cut.epoch)
            .unwrap(),
        Readiness::Ready(_)
    ));
}

#[tokio::test]
async fn dropping_an_idle_worker_closes_progress_without_a_new_commit() {
    let (store, runtime, publisher) = fixture(PausedStatus {
        pause: Mutex::new(None),
        panic: false,
    });
    let worker = Worker::start(store.clone(), runtime, publisher.subscribe()).unwrap();
    let mut progress = worker.progress();
    timeout(Duration::from_secs(5), progress.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        progress.borrow_and_update().as_ref().unwrap().status,
        PageStatus::CaughtUp
    );
    drop(worker);
    timeout(Duration::from_secs(5), async {
        while progress.changed().await.is_ok() {}
    })
    .await
    .unwrap();
    assert_eq!(
        asynchronous::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        0
    );
}

#[tokio::test]
async fn publisher_shutdown_terminates_an_idle_worker_without_a_new_commit() {
    let (store, runtime, publisher) = fixture(PausedStatus {
        pause: Mutex::new(None),
        panic: false,
    });
    let worker = Worker::start(store.clone(), runtime, publisher.subscribe()).unwrap();
    let mut progress = worker.progress();
    timeout(Duration::from_secs(5), progress.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        progress.borrow_and_update().as_ref().unwrap().status,
        PageStatus::CaughtUp
    );
    drop(publisher);
    timeout(Duration::from_secs(5), async {
        while progress.changed().await.is_ok() {}
    })
    .await
    .unwrap();
    timeout(Duration::from_secs(5), worker.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        asynchronous::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        0
    );
}

#[tokio::test]
async fn dropping_a_worker_during_a_page_finishes_only_that_page() {
    let (entered, observed) = oneshot::channel();
    let (release, paused) = mpsc::channel();
    let (store, runtime, publisher) = fixture(PausedStatus {
        pause: Mutex::new(Some((entered, paused))),
        panic: false,
    });
    let first = append(&store, "agent/ada");
    append(&store, "agent/robin");
    let worker = Worker::start(store.clone(), runtime.clone(), publisher.subscribe()).unwrap();
    let mut progress = worker.progress();
    timeout(Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    drop(worker);
    release.send(()).unwrap();
    timeout(Duration::from_secs(5), async {
        while progress.changed().await.is_ok() {}
    })
    .await
    .unwrap();
    let connection = store.readers.get();
    assert_eq!(
        source_cut(&connection).unwrap().unwrap().projected,
        first.store_index
    );
    assert_eq!(asynchronous::state(&connection).unwrap().retained_rows, 1);
    assert_eq!(
        runtime.views.readiness(&connection, "status", 1).unwrap(),
        Readiness::SourcePending
    );
    drop(connection);
    assert_eq!(
        asynchronous::step(&store, &runtime).unwrap().status,
        PageStatus::CaughtUp
    );
}

#[tokio::test]
async fn poison_operator_panic_rolls_back_every_restart_without_certifying_output() {
    let (store, runtime, publisher) = fixture(PausedStatus {
        pause: Mutex::new(None),
        panic: true,
    });
    let input = append(&store, "agent/ada");
    for _ in 0..2 {
        let worker = Worker::start(store.clone(), runtime.clone(), publisher.subscribe()).unwrap();
        let mut progress = worker.progress();
        timeout(Duration::from_secs(5), async {
            while progress.changed().await.is_ok() {}
        })
        .await
        .unwrap();
        let error = timeout(Duration::from_secs(5), worker.stop())
            .await
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("panicked"));
        let connection = store.readers.get();
        let cut = source_cut(&connection).unwrap().unwrap();
        assert_eq!(cut.admitted, input.store_index);
        assert_eq!(cut.projected, 0);
        assert_eq!(asynchronous::state(&connection).unwrap().retained_rows, 1);
        assert_eq!(
            runtime
                .views
                .readiness(&connection, "status", cut.epoch)
                .unwrap(),
            Readiness::SourcePending
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM ivm_heads", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn lagged_commit_notices_trigger_authoritative_queue_catch_up() {
    let (store, runtime, publisher) = fixture(PausedStatus {
        pause: Mutex::new(None),
        panic: false,
    });
    let worker = Worker::start(store.clone(), runtime.clone(), publisher.subscribe()).unwrap();
    let mut progress = worker.progress();
    timeout(Duration::from_secs(5), progress.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        progress.borrow_and_update().as_ref().unwrap().status,
        PageStatus::CaughtUp
    );
    // The current-thread executor cannot service the idle worker while these synchronous
    // commits overflow its one-notice channel. The durable queue remains authoritative.
    let mut input = append(&store, "agent/ada");
    for _ in 0..3 {
        input = append(&store, "agent/ada");
    }
    timeout(Duration::from_secs(5), async {
        loop {
            progress.changed().await.unwrap();
            let page = progress.borrow_and_update();
            if page.as_ref().is_some_and(|page| {
                page.status == PageStatus::CaughtUp
                    && page.source_cut.projected == input.store_index
            }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    worker.stop().await.unwrap();
    let connection = store.readers.get();
    assert_eq!(asynchronous::state(&connection).unwrap().retained_rows, 0);
    assert!(matches!(
        runtime.views.readiness(&connection, "status", 1).unwrap(),
        Readiness::Ready(_)
    ));
}
