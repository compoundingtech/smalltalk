//! The production watch stage skips only after a successful dependency-recorded evaluation.
use super::*;
use serde_json::json;

const START: u128 = 1_900_000_000_000;
const ITEM: &str = "stage/github-watches";
const SEATS: &str = "version 2\nagent \"example.planner\" { host \"away\"; workspace \"/tmp\"; command \"true\" }\nagent \"example.reviewer\" { host \"away\"; workspace \"/tmp\"; command \"true\" }\n";

struct Clock;
impl Clock {
    fn at(at: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(at));
        Self
    }
    fn set(&self, at: u128) {
        smallclaims::store::set_thread_clock(Some(at));
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

fn fixture() -> (WatchFixture, Reconciler<FakeRuntime>) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    store.set_write_clock_at(now_ms()).unwrap();
    apply_source(&store, SEATS, "seats");
    let thread = crate::github_watch::ThreadRef::parse("acme/garden#12").unwrap();
    let fixture = WatchFixture {
        observer: thread.observer(),
        resource: thread.resource(),
        store: store.clone(),
    };
    let reconciler = Reconciler::new(
        store,
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_resource_provider(Arc::new(FakeResourceProvider));
    (fixture, reconciler)
}

#[test]
fn settled_watch_stage_has_a_constant_three_statement_budget() {
    let _clock = Clock::at(START);
    for count in [0, 8, 64] {
        let (fixture, reconciler) = fixture();
        for number in 1..=count {
            fixture.watch(&format!("acme/garden#{number}"), "agent/example.planner");
        }
        for _ in 0..3 {
            reconciler.reconcile_github_watches_stage();
        }
        let reads = reconciler.incremental.reads_of(ITEM);
        assert!(reads.contains("kind:intent.desired"));
        assert!(reads.contains("kind:record.repaired"));
        let before = smallclaims::sqlite::work::total();
        reconciler.reconcile_github_watches_stage();
        let cost = smallclaims::sqlite::work::total() - before;
        assert!(
            cost.statements <= 3 && cost.vm_steps <= 100 && cost.fullscan_steps == 0,
            "settled watch stage must only observe the empty change feed, roster={count}: {cost:?}"
        );
        assert_eq!(reconciler.incremental.reads_of(ITEM), reads);
    }
}

#[tokio::test]
async fn a_pass_selects_a_watch_born_after_an_empty_watch_stage() {
    let _clock = Clock::at(START);
    let (fixture, reconciler) = fixture();
    reconciler.reconcile_once().unwrap();
    let watch = fixture.watch("acme/garden#12", "agent/example.planner");
    reconciler.reconcile_once().unwrap();
    assert!(reconciler.incremental.reads_of(ITEM).contains(&watch));
    assert!(fixture.running(&watch) && fixture.running(&fixture.observer));
    assert!(reconciler.runtime.starts.lock().unwrap().is_empty());
    // Unrelated claim traffic must not run the roster reader in the actual pass.
    fixture
        .store
        .put_document("doc/unrelated", b"unrelated", &None, "unrelated")
        .unwrap();
    smallclaims::sqlite::histogram::take();
    reconciler.reconcile_once().unwrap();
    let queries = smallclaims::sqlite::histogram::take();
    assert!(
        !queries
            .keys()
            .any(|sql| sql.contains("FROM desired WHERE kind IN")),
        "{queries:?}"
    );
}

#[tokio::test]
async fn a_pass_selects_projection_only_seat_removal_without_a_feed_record() {
    let _clock = Clock::at(START);
    let (fixture, reconciler) = fixture();
    let watch = fixture.watch("acme/garden#12", "agent/example.planner");
    for _ in 0..3 {
        reconciler.reconcile_once().unwrap();
    }
    assert!(fixture.running(&watch));
    let before = fixture.store.changes_since(u64::MAX, i64::MAX).unwrap();
    // Reader-side projection fixture: the production discard_desired_owned_by API performs
    // this DELETE for retired run members. Its native integration control is also retained.
    fixture
        .store
        .connection
        .write()
        .execute(
            "DELETE FROM desired WHERE subject='agent/example.planner'",
            [],
        )
        .unwrap();
    assert!(
        fixture
            .store
            .changes_since(before.index, before.local)
            .unwrap()
            .changes
            .is_empty()
    );
    reconciler.reconcile_once().unwrap();
    let view = fixture.store.watch_view(&watch).unwrap().unwrap();
    assert_eq!(view["state"], "ended");
    assert_eq!(view["ended"], "seat-ended");
    assert!(!fixture.running(&fixture.observer));
}

#[test]
fn a_watch_stage_keeps_and_evaluates_its_clock_only_deadline() {
    let clock = Clock::at(START);
    let (fixture, reconciler) = fixture();
    let thread = crate::github_watch::ThreadRef::parse("acme/garden#12").unwrap();
    fixture
        .store
        .declare_watch(&thread, "agent/example.planner", Some(START + 1_000))
        .unwrap();
    let watch = thread.watch("agent/example.planner");
    apply_source(
        &fixture.store,
        "version 2\nstop \"agent/example.planner\"",
        "stopped-seat-keeps-watch",
    );
    reconciler.reconcile_github_watches_stage();
    assert_eq!(reconciler.incremental.next_due(ITEM), Some(START + 1_001));
    clock.set(START + 999);
    reconciler.reconcile_github_watches_stage();
    assert!(fixture.running(&watch));
    clock.set(START + 1_001); // Preserve the existing arm_restart(until + 1) boundary.
    reconciler.reconcile_github_watches_stage();
    assert_eq!(
        fixture.store.watch_view(&watch).unwrap().unwrap()["ended"],
        "deadline"
    );
    assert_eq!(fixture.wakes("agent/example.planner").len(), 1);
    reconciler.reconcile_github_watches_stage();
    assert_eq!(fixture.wakes("agent/example.planner").len(), 1);
}

#[test]
fn watch_selection_observes_local_fresh_context_and_a_declaration_after_prior_observe() {
    // SystemLocal append timestamps are captured on the real writer thread. Use an actual
    // wall-clock anchor here, rather than comparing that receipt with a simulated future watch.
    let _clock = Clock::at(now_ms());
    let (fixture, reconciler) = fixture();
    let watch = fixture.watch("acme/garden#12", "agent/example.planner");
    reconciler.reconcile_github_watches_stage();
    let reset = fixture
        .store
        .append_claim(&ClaimInput {
            subject: "agent/example.planner".into(),
            kind: "runtime.action.requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), json!("fresh-context")),
                ("operation".into(), json!("reset")),
                ("incarnation_id".into(), json!("old")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("reset".into()),
        })
        .unwrap();
    assert!(crate::store::local_observation_position(&reset).is_some());
    let (_, spec) = fixture.store.live_watch(&watch).unwrap().unwrap();
    assert!(reset.accepted_at_unix_ms >= spec.since_unix_ms);
    reconciler.reconcile_github_watches_stage();
    assert_eq!(
        fixture.store.watch_view(&watch).unwrap().unwrap()["ended"],
        "seat-ended"
    );
    assert!(fixture.wakes("agent/example.planner").is_empty());
    // Simulate an earlier stage consuming the declaration change before watch selection.
    fixture.watch("acme/garden#13", "agent/example.reviewer");
    reconciler.incremental.observe(&fixture.store).unwrap();
    reconciler.reconcile_github_watches_stage();
    let new_watch = crate::github_watch::ThreadRef::parse("acme/garden#13")
        .unwrap()
        .watch("agent/example.reviewer");
    assert!(reconciler.incremental.reads_of(ITEM).contains(&new_watch));
    assert!(fixture.running(&new_watch));
}

#[test]
fn watch_selection_retains_remote_union_and_observer_retargeting() {
    let clock = Clock::at(START);
    let (fixture, reconciler) = fixture();
    let local = fixture.watch("acme/garden#12", "agent/example.planner");
    let peer = Store::open_memory("away").unwrap();
    peer.set_write_clock_at(START + 10).unwrap();
    apply_source(&peer, SEATS, "peer-seats");
    let thread = crate::github_watch::ThreadRef::parse("acme/garden#13").unwrap();
    peer.declare_watch(&thread, "agent/example.reviewer", None)
        .unwrap();
    fixture
        .store
        .import_replication("away", &peer.export_replication(0).unwrap())
        .unwrap();
    clock.set(START + 20);
    fixture.store.set_write_clock_at(START + 20).unwrap();
    apply_source(
        &fixture.store,
        &crate::github_watch::observer_source("acme/garden", &fixture.resource, false),
        "local-observer",
    );
    reconciler.reconcile_github_watches_stage();
    fixture.store.end_watch(&local, "unwatched", None).unwrap();
    reconciler.reconcile_github_watches_stage();
    assert!(
        fixture.running(&fixture.observer),
        "a remote watch still uses the observer"
    );
    apply_source(
        &fixture.store,
        "version 2\nresource \"alternate\" { kind \"vcs.repository\" }\nobserver \"alternate\" { resource \"resource/alternate\"; provider \"github.repository\"; locator \"acme/garden\"; field \"issues\" }",
        "alternate-observer",
    );
    // A local non-watch subscription also participates in standing-resource selection.
    apply_source(
        &fixture.store,
        "version 2\nsubscription \"standing\" { observer \"observer/github/acme/garden\"; on \"issues\"; to \"agent/example.planner\"; delivery \"message\" }",
        "standing-subscription",
    );
    reconciler.reconcile_github_watches_stage();
    let standing = fixture
        .store
        .repository_observers()
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == &fixture.observer)
        .unwrap()
        .1;
    assert_eq!(standing.resource, "resource/alternate");
    clock.set(START + 30);
    peer.set_write_clock_at(START + 30).unwrap();
    apply_source(
        &peer,
        &crate::github_watch::stop_source(&thread.watch("agent/example.reviewer")).unwrap(),
        "peer-unwatch",
    );
    fixture
        .store
        .import_replication("away", &peer.export_replication(0).unwrap())
        .unwrap();
    apply_source(
        &fixture.store,
        "version 2\nsubscription \"standing\" { stop }",
        "standing-stops",
    );
    reconciler.reconcile_github_watches_stage();
    assert!(
        !fixture.running(&fixture.observer),
        "removing the last union member stops the standing observer"
    );
}

#[test]
fn a_clean_watch_stage_retains_fault_but_a_failed_safety_pass_retries() {
    let clock = Clock::at(START);
    let (fixture, mut reconciler) = fixture();
    reconciler.reconcile_github_watches_stage();
    let reads = reconciler.incremental.reads_of(ITEM);
    reconciler
        .record_fault("daemon/node", ITEM, Err(anyhow::anyhow!("retained fault")))
        .unwrap();
    reconciler.reconcile_github_watches_stage();
    assert_eq!(
        fixture
            .store
            .reconcile_fault("daemon/node", ITEM)
            .unwrap()
            .as_deref(),
        Some("retained fault")
    );
    assert_eq!(reconciler.incremental.reads_of(ITEM), reads);
    clock.set(START + crate::incremental::FULL_PASS_INTERVAL_MS);
    reconciler.fault_injection = Some(Arc::new(FailScope(ITEM)));
    reconciler.reconcile_github_watches_stage();
    assert!(reconciler.incremental.needs(ITEM, now_ms()));
    reconciler.fault_injection = None;
    reconciler.reconcile_github_watches_stage();
    assert!(
        fixture
            .store
            .reconcile_fault("daemon/node", ITEM)
            .unwrap()
            .is_none()
    );
    assert!(!reconciler.incremental.needs(ITEM, now_ms()));
}
