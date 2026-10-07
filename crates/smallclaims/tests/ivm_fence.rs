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

fn install_gap(store: &Store, views: &Views, gap: Option<&str>) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute_batch("CREATE TABLE capture_state(id INTEGER PRIMARY KEY,gap TEXT); INSERT INTO capture_state VALUES(1,NULL)").unwrap();
    tx.execute("UPDATE capture_state SET gap=?1", [gap])
        .unwrap();
    views
        .install_gap_trigger(&tx, "capture_state", "gap")
        .unwrap();
    tx.commit().unwrap();
}

#[test]
fn raw_gap_commit_fences_same_index_and_changed_evidence_without_restoring_ready() {
    let (store, views) = fixture();
    install_gap(&store, &views, None);
    let before = capture(&store, &views, "alpha");
    let publisher = Publisher::attach(&store, 4).unwrap();
    let mut notices = publisher.subscribe();
    // No managed transaction or Rust finalizer runs on this autocommit path.
    store
        .connection
        .write()
        .execute("UPDATE capture_state SET gap='raw source mutation'", [])
        .unwrap();
    assert!(notices.try_recv().is_ok());
    let first = capture(&store, &views, "alpha");
    assert!(matches!(first.availability.readiness, Readiness::Fenced));
    assert_eq!(first.source_cut, before.source_cut);
    assert_eq!(first.keys, before.keys);
    assert_eq!(
        first.snapshot.semantic_generation,
        before.snapshot.semantic_generation
    );
    assert_eq!(
        first.availability.error.as_deref(),
        Some("raw source mutation")
    );
    assert!(matches!(
        capture(&store, &views, "beta").availability.readiness,
        Readiness::Fenced
    ));
    store
        .connection
        .write()
        .execute("UPDATE capture_state SET gap='raw source mutation'", [])
        .unwrap();
    assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(capture(&store, &views, "alpha"), first);
    store
        .connection
        .write()
        .execute("UPDATE capture_state SET gap='new evidence'", [])
        .unwrap();
    assert!(notices.try_recv().is_ok());
    let changed = capture(&store, &views, "alpha");
    assert_ne!(changed.availability.token, first.availability.token);
    assert_eq!(changed.availability.error.as_deref(), Some("new evidence"));
    store
        .connection
        .write()
        .execute("UPDATE capture_state SET gap=NULL", [])
        .unwrap();
    assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(capture(&store, &views, "alpha"), changed);
}

#[test]
fn raw_gap_rollback_publishes_nothing_and_existing_gap_fences_on_install() {
    let (store, views) = fixture();
    install_gap(&store, &views, None);
    let before = capture(&store, &views, "alpha");
    let publisher = Publisher::attach(&store, 4).unwrap();
    let mut notices = publisher.subscribe();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("UPDATE capture_state SET gap='rolled back'", [])
            .unwrap();
        tx.rollback().unwrap();
    }
    assert!(matches!(notices.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(capture(&store, &views, "alpha"), before);
    let (existing_store, existing_views) = fixture();
    install_gap(&existing_store, &existing_views, Some("already incomplete"));
    let existing = capture(&existing_store, &existing_views, "alpha");
    assert!(matches!(existing.availability.readiness, Readiness::Fenced));
    assert_eq!(
        existing.availability.error.as_deref(),
        Some("already incomplete")
    );
}

#[test]
fn gap_singleton_deletion_identity_change_and_extra_row_fence() {
    for (sql, reason) in [
        ("DELETE FROM capture_state", "source gap state removed"),
        (
            "UPDATE capture_state SET id=2",
            "source gap state identity changed",
        ),
        (
            "INSERT INTO capture_state VALUES(2,NULL)",
            "source gap state has multiple rows",
        ),
        (
            "INSERT OR REPLACE INTO capture_state VALUES(1,'replacement gap')",
            "replacement gap",
        ),
    ] {
        let (store, views) = fixture();
        install_gap(&store, &views, None);
        store.connection.write().execute(sql, []).unwrap();
        let after = capture(&store, &views, "alpha");
        assert!(matches!(after.availability.readiness, Readiness::Fenced));
        assert_eq!(after.availability.error.as_deref(), Some(reason));
    }
}

#[test]
fn gap_installer_rejects_unsupported_shapes_before_ddl() {
    let (store, views) = fixture();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute_batch("CREATE TABLE good(id INTEGER PRIMARY KEY,gap TEXT); INSERT INTO good VALUES(1,NULL);
        CREATE TABLE no_key(gap TEXT); INSERT INTO no_key VALUES(NULL);
        CREATE TABLE empty(id INTEGER PRIMARY KEY,gap TEXT);
        CREATE TABLE multiple(id INTEGER PRIMARY KEY,gap TEXT); INSERT INTO multiple VALUES(1,NULL),(2,NULL);
        CREATE TABLE composite(a INTEGER,b INTEGER,gap TEXT,PRIMARY KEY(a,b)); INSERT INTO composite VALUES(1,1,NULL);
        CREATE VIEW gap_view AS SELECT * FROM good;").unwrap();
    let before: i64 = tx
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    for (table, column) in [
        ("good; DROP TABLE good", "gap"),
        ("good", "gap'"),
        ("good", "missing"),
        ("no_key", "gap"),
        ("empty", "gap"),
        ("multiple", "gap"),
        ("composite", "gap"),
        ("gap_view", "gap"),
        ("absent", "gap"),
    ] {
        assert!(
            views.install_gap_trigger(&tx, table, column).is_err(),
            "{table}/{column}"
        );
        assert_eq!(
            tx.query_row::<i64, _, _>("PRAGMA schema_version", [], |r| r.get(0))
                .unwrap(),
            before
        );
    }
    tx.rollback().unwrap();
}

#[test]
fn gap_trigger_installation_rolls_back_and_explicit_reinstall_coalesces_evidence() {
    let (store, views) = fixture();
    install_gap(&store, &views, Some("persistent gap"));
    let before = capture(&store, &views, "alpha");
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        views
            .install_gap_trigger(&tx, "capture_state", "gap")
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(capture(&store, &views, "alpha"), before);
    let (fresh, fresh_views) = fixture();
    {
        let mut writer = fresh.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute_batch("CREATE TABLE capture_state(id INTEGER PRIMARY KEY,gap TEXT); INSERT INTO capture_state VALUES(1,'gap')").unwrap();
        fresh_views
            .install_gap_trigger(&tx, "capture_state", "gap")
            .unwrap();
        tx.rollback().unwrap();
    }
    assert!(matches!(
        capture(&fresh, &fresh_views, "alpha")
            .availability
            .readiness,
        Readiness::Ready(_)
    ));
    assert_eq!(
        fresh
            .readers
            .get()
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name LIKE 'ivm_source_gap_%'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn gap_error_is_bounded_and_trigger_targets_only_installer_registry() {
    let (store, all) = fixture();
    let only = Views::new(vec![Box::new(Empty("alpha"))]).unwrap();
    install_gap(&store, &only, None);
    let text = "界".repeat(2000);
    store
        .connection
        .write()
        .execute("UPDATE capture_state SET gap=?1", [&text])
        .unwrap();
    assert_eq!(
        capture(&store, &all, "alpha")
            .availability
            .error
            .unwrap()
            .chars()
            .count(),
        1024
    );
    assert!(matches!(
        capture(&store, &all, "beta").availability.readiness,
        Readiness::Ready(_)
    ));
}
