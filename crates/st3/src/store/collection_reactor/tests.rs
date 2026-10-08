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
        Ok(())
    }
    fn commit(&self, tx: &Transaction<'_>, _: &Context) -> Result<()> {
        tx.execute(
            "UPDATE local_fixture_reactor_scope SET commits=commits+1,scoped=0 WHERE source=?1",
            [self.source],
        )?;
        Ok(())
    }
    fn capture(&self, c: &Connection, _: &Context, _: u64, budget: Budget) -> Result<Capture> {
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
                }) as Box<dyn Captured>
            }),
            wake_at_unix_ms: None,
        })
    }
    fn coverage(&self, _: &Connection, _: &Context, _: u64) -> Result<bool> {
        Ok(false)
    }
}

struct Facts {
    source: &'static str,
    value: i64,
    gate: Option<Arc<Gate>>,
    fail: bool,
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
    fn publish(self: Box<Self>, tx: &Transaction<'_>, _: &Context, _: u64) -> Result<bool> {
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
    assert_eq!(store.read_snapshot(|index| Ok(index)).unwrap(), 0);
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
    assert!(reopened.ivm_publisher().unwrap().is_some());
}
