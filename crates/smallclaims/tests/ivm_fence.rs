//! Public source-gap fencing preserves admission while rejecting stale derived output.
use smallclaims::{
    Store,
    ivm::{
        self, Definition, Readiness, SourceCut, View, Views,
        events::{self, Publisher},
    },
    store::runtime::Plain,
};
use std::sync::Arc;
use tokio::sync::broadcast::error::TryRecvError;

struct Empty(&'static str);
impl View for Empty {
    fn definition(&self) -> Definition {
        Definition {
            name: self.0,
            fingerprint: "gap-fixture.v1",
            kinds: &["event.value"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
}
fn fixture() -> (Store, Arc<Views>) {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let views =
        Arc::new(Views::new(vec![Box::new(Empty("alpha")), Box::new(Empty("beta"))]).unwrap());
    {
        let mut writer = store.connection.write();
        writer
            .execute_batch("CREATE TABLE local_observation(id INTEGER PRIMARY KEY,value TEXT)")
            .unwrap();
        views.create_schema(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        views
            .initialize_empty(
                &tx,
                SourceCut {
                    epoch: 1,
                    admitted: 0,
                    projected: 0,
                    local_generation: 0,
                },
            )
            .unwrap();
        events::install(&tx, 32).unwrap();
        tx.commit().unwrap();
    }
    (store, views)
}
fn capture(store: &Store, views: &Views, name: &str) -> events::Boundary {
    store
        .read_snapshot(|_| events::capture(&store.readers.get(), views, name))
        .unwrap()
}

#[test]
fn finalizer_source_gap_preserves_same_index_source_and_fences_only_named_output() {
    let (store, views) = fixture();
    let before = capture(&store, &views, "alpha");
    let publisher = Publisher::attach(&store, 4).unwrap();
    let mut notices = publisher.subscribe();
    let adapter = views.clone();
    store
        .install_transaction_finalizer(move |tx| {
            adapter.fence(tx, "alpha", "uncaptured authority dependency")
        })
        .unwrap();
    store
        .connection
        .batched(|tx| {
            tx.execute(
                "INSERT INTO local_observation VALUES(1,'admitted local fact')",
                [],
            )
        })
        .unwrap()
        .unwrap();
    assert!(notices.try_recv().is_ok());
    let after = capture(&store, &views, "alpha");
    assert!(matches!(after.availability.readiness, Readiness::Fenced));
    assert_eq!(
        after.availability.error.as_deref(),
        Some("uncaptured authority dependency")
    );
    assert_eq!(after.source_cut, before.source_cut);
    assert_eq!(after.keys, before.keys);
    assert_ne!(after.availability.token, before.availability.token);
    assert!(matches!(
        capture(&store, &views, "beta").availability.readiness,
        Readiness::Ready(_)
    ));
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT value FROM local_observation WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        "admitted local fact"
    );
    assert!(views.token(&store.readers.get(), "alpha", 1).is_err());
}

#[test]
fn rolled_back_all_view_fence_is_not_published_and_does_not_change_cut_or_status() {
    let (store, views) = fixture();
    let before = capture(&store, &views, "alpha");
    let publisher = Publisher::attach(&store, 4).unwrap();
    let mut notices = publisher.subscribe();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        views.fence_all(&tx, "rolled-back gap").unwrap();
        tx.execute("INSERT INTO local_observation VALUES(1,'rolled back')", [])
            .unwrap();
        tx.rollback().unwrap();
    }
    assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(capture(&store, &views, "alpha"), before);
    assert!(matches!(
        capture(&store, &views, "beta").availability.readiness,
        Readiness::Ready(_)
    ));
}

#[test]
fn repeated_fence_coalesces_but_changed_evidence_wakes_while_already_unavailable() {
    let (store, views) = fixture();
    let publisher = Publisher::attach(&store, 4).unwrap();
    let mut notices = publisher.subscribe();
    store
        .connection
        .batched(|tx| views.fence_all(tx, "first gap"))
        .unwrap()
        .unwrap();
    assert!(notices.try_recv().is_ok());
    let first = capture(&store, &views, "alpha");
    store
        .connection
        .batched(|tx| views.fence(tx, "alpha", "first gap"))
        .unwrap()
        .unwrap();
    assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(capture(&store, &views, "alpha"), first);
    store
        .connection
        .batched(|tx| views.fence(tx, "alpha", "new evidence"))
        .unwrap()
        .unwrap();
    assert!(notices.try_recv().is_ok());
    let second = capture(&store, &views, "alpha");
    assert_eq!(second.source_cut, first.source_cut);
    assert_eq!(second.keys, first.keys);
    assert_ne!(second.availability.token, first.availability.token);
    assert_eq!(second.availability.error.as_deref(), Some("new evidence"));
    assert_eq!(
        capture(&store, &views, "beta")
            .availability
            .error
            .as_deref(),
        Some("first gap")
    );
}

#[test]
fn unknown_view_fails_without_silent_fence_or_new_ready_registration() {
    let (store, views) = fixture();
    let before = capture(&store, &views, "alpha");
    assert!(
        store
            .connection
            .batched(|tx| views.fence(tx, "undeclared", "gap"))
            .unwrap()
            .is_err()
    );
    assert_eq!(capture(&store, &views, "alpha"), before);
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM ivm_views WHERE name='undeclared'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        ivm::source_cut(&store.readers.get()).unwrap().unwrap(),
        before.source_cut
    );
}
