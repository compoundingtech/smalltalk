use super::*;
use std::collections::BTreeMap;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

struct Probe {
    discoveries: AtomicUsize,
    fetches: AtomicUsize,
    fail: AtomicBool,
    released: std::sync::Mutex<bool>,
    release: Condvar,
    started: Notify,
}

impl Probe {
    fn new(block: bool) -> Arc<Self> {
        Arc::new(Self {
            discoveries: AtomicUsize::new(0),
            fetches: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            released: std::sync::Mutex::new(!block),
            release: Condvar::new(),
            started: Notify::new(),
        })
    }

    fn unblock(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl Transport for Probe {
    fn discover(&self) -> Result<Advertisement> {
        self.discoveries.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.release.wait(released).unwrap();
        }
        anyhow::ensure!(
            !self.fail.load(Ordering::SeqCst),
            "synthetic authentication failure"
        );
        Ok(Advertisement {
            observed_at: "2026-01-01T00:00:00Z".into(),
            refs: BTreeMap::from([(
                format!("{}I{}/current", super::super::PREFIX, "1".repeat(40)),
                "a".repeat(40),
            )]),
        })
    }

    fn sync(&self) -> Result<()> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn sliding_budget_admits_six_reads_and_spaces_discovery_and_fetch() {
    let start = Instant::now();
    let mut budget = Budget::default();
    for second in [0, 10, 20, 30, 40, 50] {
        let at = start + Duration::from_secs(second);
        assert!(budget.admit(at));
        assert!(!budget.admit(at + Duration::from_millis(1)));
    }
    assert_eq!(budget.starts.len(), 6);
    assert!(!budget.admit(start + Duration::from_secs(59)));
    assert!(budget.admit(start + Duration::from_secs(60)));
    assert_eq!(budget.starts.len(), 6);
    assert_eq!(
        budget.available_at(start + Duration::from_secs(61)),
        start + Duration::from_secs(70)
    );
}

#[tokio::test]
async fn canceled_client_and_concurrent_requests_share_one_owned_discovery() {
    let probe = Probe::new(true);
    let owner = Owner::new(probe.clone());
    let first = {
        let owner = owner.clone();
        tokio::spawn(async move { owner.observe(false).await.unwrap() })
    };
    probe.started.notified().await;
    first.abort();
    let mut readers = Vec::new();
    for _ in 0..10 {
        let owner = owner.clone();
        readers.push(tokio::spawn(
            async move { owner.observe(false).await.unwrap() },
        ));
    }
    assert_eq!(owner.sync().await.unwrap(), SyncAdmission::InFlight);
    probe.unblock();
    for reader in readers {
        let observation = reader.await.unwrap();
        assert!(observation.advertisement.is_some());
        assert!(observation.error.is_none());
    }
    assert_eq!(probe.discoveries.load(Ordering::SeqCst), 1);
    let diagnostics = owner.diagnostics().await;
    assert_eq!(diagnostics.counters.discoveries_started, 1);
    assert_eq!(diagnostics.remote_read_operations_in_last_minute, 1);
    assert!(diagnostics.next_read_after_ms > 0);
    assert_eq!(probe.fetches.load(Ordering::SeqCst), 0);
    let observed = owner.observe(false).await.unwrap();
    assert!(observed.reused);
    assert_eq!(probe.discoveries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn offline_is_zero_reads_and_a_failed_discovery_preserves_the_old_inventory() {
    let probe = Probe::new(false);
    let owner = Owner::new(probe.clone());
    assert!(owner.observe(true).await.unwrap().advertisement.is_none());
    assert_eq!(probe.discoveries.load(Ordering::SeqCst), 0);
    let before = owner.observe(false).await.unwrap();
    probe.fail.store(true, Ordering::SeqCst);
    // Simulate the admission window passing without sleeping in a test.
    owner.state.lock().await.budget.starts.clear();
    let failed = owner.observe(false).await.unwrap();
    assert_eq!(failed.advertisement, before.advertisement);
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("authentication failure")
    );
    assert!(failed.attempted_at.is_some());
    let offline = owner.observe(true).await.unwrap();
    assert!(offline.offline);
    assert_eq!(offline.advertisement, before.advertisement);
    assert_eq!(probe.discoveries.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn explicit_sync_consumes_the_same_read_budget() {
    let probe = Probe::new(false);
    let owner = Owner::new(probe.clone());
    assert_eq!(owner.sync().await.unwrap(), SyncAdmission::Completed);
    assert!(matches!(
        owner.sync().await.unwrap(),
        SyncAdmission::Deferred { .. }
    ));
    let observation = owner.observe(false).await.unwrap();
    assert!(observation.reused);
    assert!(observation.advertisement.is_none());
    assert_eq!(probe.discoveries.load(Ordering::SeqCst), 0);
    assert_eq!(probe.fetches.load(Ordering::SeqCst), 1);
    let diagnostics = owner.diagnostics().await;
    assert_eq!(diagnostics.counters.fetches_started, 1);
    assert_eq!(diagnostics.counters.discoveries_started, 0);
    assert_eq!(diagnostics.remote_read_operations_in_last_minute, 1);
}

#[tokio::test]
async fn repeated_and_offline_reads_share_inventory_and_id_reduction_allocations() {
    let probe = Probe::new(false);
    let owner = Owner::new(probe.clone());
    let first = owner.observe(false).await.unwrap();
    for offline in [false, true] {
        let second = owner.observe(offline).await.unwrap();
        assert!(Arc::ptr_eq(
            first.advertisement.as_ref().unwrap(),
            second.advertisement.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&first.ids, &second.ids));
    }
    assert_eq!(probe.discoveries.load(Ordering::SeqCst), 1);
    assert_eq!(first.advertised_ids().len(), 1);
}
