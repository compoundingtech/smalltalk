use super::*;
use smallclaims::ivm::{
    Definition,
    install::{Mutation, Namespace},
};
use std::sync::atomic::AtomicUsize;

// Scheduler fixture, not a public-row source certificate. It uses point replacements to
// check snapshot release, stale preparation, transactional failure and source isolation.
struct Fixture {
    source: &'static str,
    view: &'static str,
    gate: Option<Arc<Gate>>,
    fail: bool,
    captures: Arc<AtomicUsize>,
    storage_failure: Arc<AtomicBool>,
    begin_failure: AtomicUsize,
    commit_failure: AtomicUsize,
    covered: AtomicBool,
    fail_fence: Arc<AtomicBool>,
    fence_failures: AtomicUsize,
    arm_fence_failure: Arc<AtomicBool>,
    panic_publish: Arc<AtomicBool>,
}

struct Gate {
    entered: Notify,
    release: (Mutex<bool>, std::sync::Condvar),
    once: AtomicBool,
}

struct FixtureView {
    name: &'static str,
    source: &'static str,
}
impl View for FixtureView {
    fn definition(&self) -> Definition {
        Definition {
            name: self.name,
            fingerprint: "fixture.v1",
            kinds: &[],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn installed_source(&self) -> Option<&'static str> {
        Some(self.source)
    }
}

struct FixtureOperator {
    name: &'static str,
    source: &'static str,
}
impl Operator for FixtureOperator {
    fn name(&self) -> &'static str {
        self.name
    }
    fn source(&self) -> &'static str {
        self.source
    }
    fn fingerprint(&self) -> &'static str {
        "fixture.v1"
    }
    fn create_schema(&self, _: &Connection) -> Result<()> {
        Ok(())
    }
    fn apply(&self, _: &Transaction<'_>, _: &Namespace, _: &[Mutation]) -> Result<bool> {
        unreachable!("reactor never dispatches synchronous operators")
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        Ok(())
    }
    fn reclaim(&self, _: &Transaction<'_>, _: &Namespace, _: usize) -> Result<bool> {
        Ok(true)
    }
}

impl Source for Fixture {
    fn name(&self) -> &'static str {
        self.source
    }
    fn views(&self) -> Vec<Box<dyn View>> {
        vec![Box::new(FixtureView {
            name: self.view,
            source: self.source,
        })]
    }
    fn operators(&self) -> Vec<Box<dyn Operator>> {
        vec![Box::new(FixtureOperator {
            name: self.view,
            source: self.source,
        })]
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        c.execute_batch("CREATE TABLE IF NOT EXISTS local_fixture_reactor_pending(source TEXT PRIMARY KEY,value INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS local_fixture_reactor_output(source TEXT PRIMARY KEY,value INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS local_fixture_reactor_scope(source TEXT PRIMARY KEY,commits INTEGER NOT NULL,scoped INTEGER NOT NULL);")?;
        Ok(())
    }
    fn open(&self, tx: &Transaction<'_>, cx: &Context) -> Result<()> {
        assert!(tx.query_row(
            "SELECT scoped FROM local_fixture_reactor_scope WHERE source=?1",
            [self.source],
            |r| r.get::<_, bool>(0)
        )?);
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_install_sources WHERE name=?1)",
            [self.source],
            |r| r.get(0),
        )?;
        if !exists {
            cx.installer
                .register_source(tx, self.source, "fixture-alder.v1", 1)?;
        }
        cx.views.register_installed(tx, &cx.installer, self.view)?;
        Ok(())
    }
    fn begin(&self, tx: &Transaction<'_>, _: &Context) -> Result<()> {
        tx.execute("INSERT INTO local_fixture_reactor_scope VALUES(?1,0,1) ON CONFLICT(source) DO UPDATE SET scoped=1", [self.source])?;
        hook_fault(if self.fail_fence.load(Ordering::SeqCst) {
            self.fence_failures.fetch_add(1, Ordering::SeqCst);
            2
        } else {
            self.begin_failure.load(Ordering::SeqCst)
        })
    }
    fn commit(&self, tx: &Transaction<'_>, _: &Context) -> Result<()> {
        tx.execute(
            "UPDATE local_fixture_reactor_scope SET commits=commits+1,scoped=0 WHERE source=?1",
            [self.source],
        )?;
        hook_fault(self.commit_failure.load(Ordering::SeqCst))
    }
    fn capture(
        &self,
        _: &Store,
        c: &Connection,
        _: &Context,
        _: u64,
        budget: Budget,
    ) -> Result<Capture> {
        assert!(budget.rows >= 1 && budget.bytes >= 16);
        self.captures.fetch_add(1, Ordering::SeqCst);
        let value = c
            .query_row(
                "SELECT value FROM local_fixture_reactor_pending WHERE source=?1",
                [self.source],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(Capture {
            work: value.map(|value| {
                Box::new(Facts {
                    source: self.source,
                    value,
                    gate: self.gate.clone(),
                    fail: self.fail,
                    storage_failure: self.storage_failure.clone(),
                    fail_fence: self.fail_fence.clone(),
                    arm_fence_failure: self.arm_fence_failure.clone(),
                    panic_publish: self.panic_publish.clone(),
                }) as Box<dyn Captured>
            }),
            wake_at_unix_ms: None,
        })
    }
    fn coverage(&self, _: &Store, _: &Connection, _: &Context, _: u64) -> Result<bool> {
        Ok(self.covered.load(Ordering::SeqCst))
    }
}

struct Facts {
    source: &'static str,
    value: i64,
    gate: Option<Arc<Gate>>,
    fail: bool,
    storage_failure: Arc<AtomicBool>,
    fail_fence: Arc<AtomicBool>,
    arm_fence_failure: Arc<AtomicBool>,
    panic_publish: Arc<AtomicBool>,
}
impl Captured for Facts {
    fn prepare(self: Box<Self>, _: &Context) -> Result<Box<dyn Page>> {
        if let Some(gate) = &self.gate
            && !gate.once.swap(true, Ordering::SeqCst)
        {
            // Positive signal after capture's snapshot has ended.
            gate.entered.notify_one();
            let released = gate.release.0.lock().unwrap();
            let (released, _) = gate
                .release
                .1
                .wait_timeout_while(released, std::time::Duration::from_secs(10), |released| {
                    !*released
                })
                .unwrap();
            assert!(*released, "fixture preparation was never released");
        }
        Ok(self)
    }
}
impl Page for Facts {
    fn publish(
        self: Box<Self>,
        _: &Store,
        tx: &Transaction<'_>,
        _: &Context,
        _: u64,
    ) -> Result<bool> {
        let current = tx
            .query_row(
                "SELECT value FROM local_fixture_reactor_pending WHERE source=?1",
                [self.source],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        if current != Some(self.value) {
            return Ok(true);
        }
        tx.execute("INSERT INTO local_fixture_reactor_output VALUES(?1,?2) ON CONFLICT(source) DO UPDATE SET value=excluded.value", params![self.source,self.value])?;
        if self.storage_failure.load(Ordering::SeqCst) {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("fixture writer contention".into()),
            )
            .into());
        }
        assert!(
            !self.panic_publish.load(Ordering::SeqCst),
            "fixture page panic"
        );
        if self.fail && self.arm_fence_failure.load(Ordering::SeqCst) {
            self.fail_fence.store(true, Ordering::SeqCst);
        }
        anyhow::ensure!(!self.fail, "fixture publication failure");
        tx.execute(
            "DELETE FROM local_fixture_reactor_pending WHERE source=?1",
            [self.source],
        )?;
        Ok(false)
    }
}

fn source(
    source: &'static str,
    view: &'static str,
    gate: Option<Arc<Gate>>,
    fail: bool,
) -> Arc<Fixture> {
    Arc::new(Fixture {
        source,
        view,
        gate,
        fail,
        captures: Arc::new(AtomicUsize::new(0)),
        storage_failure: Arc::new(AtomicBool::new(false)),
        begin_failure: AtomicUsize::new(0),
        commit_failure: AtomicUsize::new(0),
        covered: AtomicBool::new(false),
        fail_fence: Arc::new(AtomicBool::new(false)),
        fence_failures: AtomicUsize::new(0),
        arm_fence_failure: Arc::new(AtomicBool::new(false)),
        panic_publish: Arc::new(AtomicBool::new(false)),
    })
}

fn replace(store: &Store, source: &str, value: i64) {
    store.connection.batched(|tx| tx.execute("INSERT INTO local_fixture_reactor_pending VALUES(?1,?2) ON CONFLICT(source) DO UPDATE SET value=excluded.value", params![source,value])).unwrap().unwrap();
}

fn output(store: &Store, source: &str) -> Option<i64> {
    store
        .read_snapshot(|_| {
            Ok(store
                .readers
                .get()
                .query_row(
                    "SELECT value FROM local_fixture_reactor_output WHERE source=?1",
                    [source],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .unwrap()
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_preparation_releases_snapshot_and_writer_and_stale_pages_retry() {
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate {
        entered: Notify::new(),
        release: (Mutex::new(false), std::sync::Condvar::new()),
        once: AtomicBool::new(false),
    });
    let fixture = source(
        "fixture.alpha",
        "fixture.alpha.rows",
        Some(gate.clone()),
        false,
    );
    let store = Arc::new(
        Store::open_with_collection_sources(
            &root.path().join("graph.db"),
            "alder",
            vec![fixture.clone()],
        )
        .unwrap(),
    );
    replace(&store, fixture.source, 1);
    assert!(store.start_collection_reactor().unwrap());
    assert!(!store.start_collection_reactor().unwrap());
    assert!(Arc::ptr_eq(
        &store.ivm_installer().unwrap(),
        &store.collection_sources().unwrap().cx.installer
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    // Preparation is blocked with owned facts. Readers and the one admission writer remain live.
    assert_eq!(store.read_snapshot(Ok).unwrap(), 0);
    replace(&store, fixture.source, 2);
    assert_eq!(output(&store, fixture.source), None);
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_one();
    wait_for(|| output(&store, fixture.source) == Some(2)).await;
    let prepared = fixture.captures.load(Ordering::SeqCst);
    assert!(prepared >= 2, "stale value 1 must be recaptured");
    let weak = Arc::downgrade(&store);
    drop(store);
    wait_for(|| weak.upgrade().is_none()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_publication_rolls_back_and_fences_only_its_source() {
    let root = tempfile::tempdir().unwrap();
    let bad = source("fixture.bad", "fixture.bad.rows", None, true);
    let good = source("fixture.good", "fixture.good.rows", None, false);
    let store = Arc::new(
        Store::open_with_collection_sources(
            &root.path().join("graph.db"),
            "alder",
            vec![bad.clone(), good.clone()],
        )
        .unwrap(),
    );
    replace(&store, bad.source, 1);
    replace(&store, good.source, 1);
    store.start_collection_reactor().unwrap();
    wait_for(|| output(&store, good.source) == Some(1)).await;
    assert_eq!(
        output(&store, bad.source),
        None,
        "publication failure rolls back its writes"
    );
    let registry = store.collection_sources().unwrap();
    let available = |source| {
        store
            .read_snapshot(|_| {
                Ok(store.readers.get().query_row(
                    "SELECT available FROM ivm_install_sources WHERE name=?1",
                    [source],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .unwrap()
    };
    assert!(!available(bad.source));
    assert!(available(good.source));
    let failed_attempts = bad.captures.load(Ordering::SeqCst);
    replace(&store, good.source, 2);
    wait_for(|| output(&store, good.source) == Some(2)).await;
    assert_eq!(
        bad.captures.load(Ordering::SeqCst),
        failed_attempts,
        "a commit cannot auto-recover a fenced source"
    );
    assert!(registry.retry_source("unknown").is_err());
    registry.retry_source(bad.source).unwrap();
    wait_for(|| bad.captures.load(Ordering::SeqCst) > failed_attempts).await;
}

#[test]
fn registry_rejects_duplicate_lifetimes_and_managed_hooks_roll_back() {
    let root = tempfile::tempdir().unwrap();
    let fixture = source("fixture.alpha", "fixture.alpha.rows", None, false);
    assert!(Registry::new("alder".into(), vec![fixture.clone(), fixture.clone()]).is_err());
    let store = Store::open_with_collection_sources(
        &root.path().join("graph.db"),
        "alder",
        vec![fixture.clone()],
    )
    .unwrap();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute(
            "INSERT INTO local_fixture_reactor_pending VALUES(?1,1)",
            [fixture.source],
        )
        .unwrap();
        tx.rollback().unwrap();
    }
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT count(*) FROM local_fixture_reactor_pending",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT commits FROM local_fixture_reactor_scope WHERE source=?1",
                [fixture.source],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    replace(&store, fixture.source, 2);
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT commits FROM local_fixture_reactor_scope WHERE source=?1",
                [fixture.source],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    drop(store);
    let reopened =
        Store::open_with_collection_sources(&root.path().join("graph.db"), "alder", vec![fixture])
            .unwrap();
    assert_eq!(
        reopened
            .readers
            .get()
            .query_row("SELECT value FROM local_fixture_reactor_pending", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let prepared = reopened
        .prepared_ivm_publisher()
        .expect("publisher prepared before exposure");
    assert!(Arc::ptr_eq(
        &prepared,
        &reopened.ivm_publisher().unwrap().unwrap()
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_failure_rolls_back_and_retries_without_a_source_repair() {
    let root = tempfile::tempdir().unwrap();
    let fixture = source("fixture.alpha", "fixture.alpha.rows", None, false);
    fixture.storage_failure.store(true, Ordering::SeqCst);
    let store = Arc::new(
        Store::open_with_collection_sources(
            &root.path().join("graph.db"),
            "alder",
            vec![fixture.clone()],
        )
        .unwrap(),
    );
    replace(&store, fixture.source, 7);
    store.start_collection_reactor().unwrap();
    wait_for(|| fixture.captures.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(output(&store, fixture.source), None);
    assert!(
        store
            .read_snapshot(|_| Ok(store.readers.get().query_row(
                "SELECT available FROM ivm_install_sources WHERE name=?1",
                [fixture.source],
                |r| r.get::<_, bool>(0),
            )?))
            .unwrap()
    );
    fixture.storage_failure.store(false, Ordering::SeqCst);
    wait_for(|| output(&store, fixture.source) == Some(7)).await;
}

fn hook_fault(fault: usize) -> Result<()> {
    match fault {
        0 => Ok(()),
        1 => anyhow::bail!("fixture capture gap"),
        2 => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        )
        .into()),
        _ => panic!("fixture hook panic"),
    }
}

#[test]
fn logical_capture_hook_gaps_keep_native_admission_and_isolate_sources() {
    for begin in [true, false] {
        for failure in [1, 3] {
            let root = tempfile::tempdir().unwrap();
            let bad = source("fixture.bad", "fixture.bad.rows", None, false);
            let good = source("fixture.good", "fixture.good.rows", None, false);
            let store = Store::open_with_collection_sources(
                &root.path().join("graph.db"),
                "alder",
                vec![bad.clone(), good.clone()],
            )
            .unwrap();
            if begin {
                bad.begin_failure.store(failure, Ordering::SeqCst);
            } else {
                bad.commit_failure.store(failure, Ordering::SeqCst);
            }
            replace(&store, bad.source, 11);
            assert_eq!(
                store
                    .readers
                    .get()
                    .query_row(
                        "SELECT value FROM local_fixture_reactor_pending WHERE source=?1",
                        [bad.source],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap(),
                11
            );
            let registry = store.collection_sources().unwrap();
            assert!(
                registry
                    .cx
                    .installer
                    .position(&store.readers.get(), bad.source)
                    .is_err()
            );
            assert!(
                registry
                    .cx
                    .installer
                    .position(&store.readers.get(), good.source)
                    .is_ok()
            );
        }
    }
}

#[test]
fn storage_failure_in_either_capture_hook_rolls_back_the_native_transaction() {
    for begin in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let fixture = source("fixture.alpha", "fixture.alpha.rows", None, false);
        let store = Store::open_with_collection_sources(
            &root.path().join("graph.db"),
            "alder",
            vec![fixture.clone()],
        )
        .unwrap();
        if begin {
            fixture.begin_failure.store(2, Ordering::SeqCst);
        } else {
            fixture.commit_failure.store(2, Ordering::SeqCst);
        }
        let error = {
            let mut writer = store.connection.write();
            match writer.transaction() {
                Err(error) => error,
                Ok(tx) => {
                    tx.execute(
                        "INSERT INTO local_fixture_reactor_pending VALUES(?1,13)",
                        [fixture.source],
                    )
                    .unwrap();
                    tx.commit().unwrap_err()
                }
            }
        };
        assert!(storage_failure(&error));
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT count(*) FROM local_fixture_reactor_pending",
                    [],
                    |r| r.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
        assert!(
            store
                .collection_sources()
                .unwrap()
                .cx
                .installer
                .position(&store.readers.get(), fixture.source)
                .is_ok()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_durable_fence_refuses_reads_even_if_native_coverage_claims_ready() {
    let root = tempfile::tempdir().unwrap();
    let fixture = source("fixture.alpha", "fixture.alpha.rows", None, true);
    fixture.covered.store(true, Ordering::SeqCst);
    fixture.arm_fence_failure.store(true, Ordering::SeqCst);
    let store = Arc::new(
        Store::open_with_collection_sources(
            &root.path().join("graph.db"),
            "alder",
            vec![fixture.clone()],
        )
        .unwrap(),
    );
    let registry = store.collection_sources().unwrap();
    assert!(
        store
            .read_snapshot(|_| registry.coverage(
                fixture.source,
                &store,
                &store.readers.get(),
                clock_ms()
            ))
            .unwrap()
    );
    replace(&store, fixture.source, 17);
    store.start_collection_reactor().unwrap();
    wait_for(|| fixture.fence_failures.load(Ordering::SeqCst) > 0).await;
    assert!(registry.unavailable[0].load(Ordering::Acquire));
    // The armed hook makes the separate durable fencing transaction fail at begin.
    assert!(
        registry
            .cx
            .installer
            .position(&store.readers.get(), fixture.source)
            .is_ok()
    );
    assert!(
        fixture
            .coverage(&store, &store.readers.get(), &registry.cx, clock_ms())
            .unwrap()
    );
    assert!(
        !store
            .read_snapshot(|_| registry.coverage(
                fixture.source,
                &store,
                &store.readers.get(),
                clock_ms()
            ))
            .unwrap()
    );
    assert_eq!(output(&store, fixture.source), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_panic_returns_the_writer_and_preserves_later_native_writes() {
    let root = tempfile::tempdir().unwrap();
    let bad = source("fixture.bad", "fixture.bad.rows", None, false);
    let good = source("fixture.good", "fixture.good.rows", None, false);
    bad.panic_publish.store(true, Ordering::SeqCst);
    let store = Arc::new(
        Store::open_with_collection_sources(
            &root.path().join("graph.db"),
            "alder",
            vec![bad.clone(), good.clone()],
        )
        .unwrap(),
    );
    replace(&store, bad.source, 1);
    replace(&store, good.source, 2);
    store.start_collection_reactor().unwrap();
    wait_for(|| output(&store, good.source) == Some(2)).await;
    assert_eq!(output(&store, bad.source), None);
    replace(&store, good.source, 3);
    wait_for(|| output(&store, good.source) == Some(3)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_siblings_capture_hook_failure_does_not_fence_valid_publication() {
    for begin in [true, false] {
        for failure in [1, 3] {
            let root = tempfile::tempdir().unwrap();
            let gate = Arc::new(Gate {
                entered: Notify::new(),
                release: (Mutex::new(false), std::sync::Condvar::new()),
                once: AtomicBool::new(false),
            });
            let idle = source("fixture.idle", "fixture.idle.rows", None, false);
            let active = source(
                "fixture.active",
                "fixture.active.rows",
                Some(gate.clone()),
                false,
            );
            let store = Arc::new(
                Store::open_with_collection_sources(
                    &root.path().join("graph.db"),
                    "alder",
                    vec![idle.clone(), active.clone()],
                )
                .unwrap(),
            );
            replace(&store, active.source, 19);
            store.start_collection_reactor().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
                .await
                .unwrap();
            // B's owned page is prepared after releasing its read snapshot. A has no page.
            if begin {
                idle.begin_failure.store(failure, Ordering::SeqCst);
            } else {
                idle.commit_failure.store(failure, Ordering::SeqCst);
            }
            *gate.release.0.lock().unwrap() = true;
            gate.release.1.notify_one();
            wait_for(|| output(&store, active.source) == Some(19)).await;
            let registry = store.collection_sources().unwrap();
            assert!(
                registry
                    .cx
                    .installer
                    .position(&store.readers.get(), idle.source)
                    .is_err()
            );
            assert!(
                registry
                    .cx
                    .installer
                    .position(&store.readers.get(), active.source)
                    .is_ok()
            );
            assert!(registry.unavailable[0].load(Ordering::Acquire));
            assert!(!registry.unavailable[1].load(Ordering::Acquire));
            assert_eq!(output(&store, idle.source), None);
            // A keeps failing; later native B admission and its next publication still succeed.
            replace(&store, active.source, 23);
            wait_for(|| output(&store, active.source) == Some(23)).await;
            assert!(
                registry
                    .cx
                    .installer
                    .position(&store.readers.get(), active.source)
                    .is_ok()
            );
        }
    }
}

// A cache marker is an honest independent source without an IVM row namespace.
// It echoes one bounded native index only; it certifies no production snapshot.
struct CacheOnly {
    ready: Arc<Mutex<Option<u64>>>,
    fail: AtomicBool,
}
impl Source for CacheOnly {
    fn name(&self) -> &'static str {
        "fixture.cache-only"
    }
    fn views(&self) -> Vec<Box<dyn View>> {
        vec![]
    }
    fn operators(&self) -> Vec<Box<dyn Operator>> {
        vec![]
    }
    fn create_schema(&self, _: &Connection) -> Result<()> {
        Ok(())
    }
    fn open(&self, tx: &Transaction<'_>, cx: &Context) -> Result<()> {
        cx.installer
            .register_source(tx, self.name(), "fixture.cache-only.v1", 1)
    }
    fn capture(
        &self,
        store: &Store,
        c: &Connection,
        cx: &Context,
        _: u64,
        budget: Budget,
    ) -> Result<Capture> {
        assert!(Arc::ptr_eq(&store.ivm_views().unwrap(), &cx.views));
        assert!(budget.rows >= 1 && budget.bytes >= 8);
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("fixture cache capture failure");
        }
        let index = smallclaims::store::current_index(c)?;
        Ok(Capture {
            work: (*self.ready.lock().unwrap() != Some(index)).then(|| {
                Box::new(CacheMarker {
                    index,
                    ready: self.ready.clone(),
                }) as Box<dyn Captured>
            }),
            wake_at_unix_ms: None,
        })
    }
    fn coverage(&self, store: &Store, c: &Connection, cx: &Context, _: u64) -> Result<bool> {
        assert!(Arc::ptr_eq(&store.ivm_views().unwrap(), &cx.views));
        Ok(*self.ready.lock().unwrap() == Some(smallclaims::store::current_index(c)?))
    }
}
struct CacheMarker {
    index: u64,
    ready: Arc<Mutex<Option<u64>>>,
}
impl Captured for CacheMarker {
    fn prepare(self: Box<Self>, _: &Context) -> Result<Box<dyn Page>> {
        Ok(self)
    }
}
impl Page for CacheMarker {
    fn publish(
        self: Box<Self>,
        store: &Store,
        tx: &Transaction<'_>,
        cx: &Context,
        _: u64,
    ) -> Result<bool> {
        assert!(Arc::ptr_eq(&store.ivm_views().unwrap(), &cx.views));
        if smallclaims::store::current_index(tx)? != self.index {
            return Ok(true);
        }
        // No DML and no strong Store retained: the stable native cut is the sole marker.
        *self.ready.lock().unwrap() = Some(self.index);
        Ok(false)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_only_source_uses_borrowed_owner_and_guarded_shared_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let source = Arc::new(CacheOnly {
        ready: Arc::new(Mutex::new(None)),
        fail: AtomicBool::new(false),
    });
    let store = Arc::new(
        Store::open_with_collection_sources(
            &root.path().join("cache.db"),
            "alder",
            vec![source.clone()],
        )
        .unwrap(),
    );
    assert!(store.prepared_ivm_publisher().is_some());
    let registry = store.collection_sources().unwrap();
    assert!(registry.names[0].is_empty());
    assert!(
        !store
            .read_snapshot(|_| registry.coverage(
                source.name(),
                &store,
                &store.readers.get(),
                clock_ms()
            ))
            .unwrap()
    );
    assert!(store.start_collection_reactor().unwrap());
    assert!(!store.start_collection_reactor().unwrap());
    wait_for(|| source.ready.lock().unwrap().is_some()).await;
    assert!(
        store
            .read_snapshot(|_| registry.coverage(
                source.name(),
                &store,
                &store.readers.get(),
                clock_ms()
            ))
            .unwrap()
    );
    source.fail.store(true, Ordering::SeqCst);
    registry.wake();
    wait_for(|| registry.unavailable[0].load(Ordering::Acquire)).await;
    assert!(
        !store
            .read_snapshot(|_| registry.coverage(
                source.name(),
                &store,
                &store.readers.get(),
                clock_ms()
            ))
            .unwrap()
    );
    // The factory guard refuses even though this fixture's raw cache marker still matches.
    assert!(
        store
            .read_snapshot(|_| source.coverage(
                &store,
                &store.readers.get(),
                &registry.cx,
                clock_ms()
            ))
            .unwrap()
    );
    let weak = Arc::downgrade(&store);
    drop(store);
    wait_for(|| weak.upgrade().is_none()).await;
}
