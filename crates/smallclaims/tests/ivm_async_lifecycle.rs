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
    let publisher = Publisher::attach(&store, 8).unwrap();
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
    release.send(()).unwrap();
    timeout(Duration::from_secs(5), stopping)
        .await
        .unwrap()
        .unwrap();
    let connection = store.readers.get();
    let cut = source_cut(&connection).unwrap().unwrap();
    assert_eq!(cut.projected, first.store_index);
    assert_eq!(cut.admitted, second.store_index);
    assert_eq!(asynchronous::state(&connection).unwrap().retained_rows, 1);
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
