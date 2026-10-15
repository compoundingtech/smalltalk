//! Bulk paths hold the store's one writer for a bounded page at a time. Each stage runs in its
//! own `holds::longest_during` scope, under its own charge so a failure names it, and a stage
//! whose longest writer hold passes the bound fails here. The bound is the 100 ms transaction
//! target with room for a debug build on a shared CI runner; a path that holds the writer for its
//! whole input passes it by seconds.
use super::*;
use std::time::Duration;

const FLEET: &str = "5b4b1a8e-7c3d-4e2f-9a1b-0c6d5e4f3a2b";
const BOUND: Duration = Duration::from_millis(500);

/// Run one stage and fail when any writer hold it asked for lasted longer than `BOUND`. Returns
/// the stage's result and its longest hold.
fn stage<T>(name: &'static str, work: impl FnOnce() -> T) -> (T, Duration) {
    let (result, longest) = smallclaims::windows::holds::longest_during(|| {
        smallclaims::performance::with_cpu(Some(name), None, work)
    });
    eprintln!("{name}: longest writer hold {longest:?}");
    assert!(longest <= BOUND, "{name} held the writer for {longest:?}, past the {BOUND:?} bound");
    (result, longest)
}

/// A peer catching up on 1,500 claims: export, receipt, admission, repairs and projection each
/// hold the writer one page at a time.
#[test]
fn a_catch_up_exchange_holds_the_writer_one_page_at_a_time() {
    let directory = tempfile::tempdir().unwrap();
    let node = |name: &str| {
        let store = Store::open(&directory.path().join(format!("{name}.sqlite3")), name).unwrap();
        store.bind_fleet(FLEET).unwrap();
        store
    };
    let source = node("birch");
    let target = node("cedar");
    for index in 0..1500 {
        source
            .append_claim(&ClaimInput {
                subject: format!("message/bound-{index}"),
                kind: "message.sent".into(),
                actor: Some("person/avery".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/avery")),
                    ("to".into(), json!(format!("agent/bound/{}", index % 40))),
                    ("content".into(), json!(format!("note {index} {}", "x".repeat(200)))),
                    ("status".into(), json!("sent")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let mut received = Duration::ZERO;
    for _ in 0..16 {
        let inventory = target.replication_inventory().unwrap();
        let (exchange, _) = stage("bound/catch-up/export", || {
            source.export_replication_exchange(FLEET, &inventory).unwrap()
        });
        if exchange.envelopes.is_empty() {
            break;
        }
        let (_, held) = stage("bound/catch-up/receive", || {
            target.receive_replication_exchange("birch", FLEET, &exchange).unwrap()
        });
        received = received.max(held);
        stage("bound/catch-up/admit", || target.validate_replication_backlog().unwrap());
        stage("bound/catch-up/repairs", || target.apply_replication_repairs().unwrap());
        stage("bound/catch-up/project", || target.project_replication_backlog().unwrap());
    }
    assert_eq!(
        target.replication_inventory().unwrap().digest,
        source.replication_inventory().unwrap().digest,
        "the catch-up finished"
    );
    assert!(received > Duration::ZERO, "the receive stage asked for no writer hold");
}

/// A late claim about one request under a standing root with 200 child runs rebuilds that
/// request's part of the tree, not all 200 runs in one hold.
#[test]
fn a_late_claim_in_a_large_standing_tree_holds_the_writer_for_its_own_part() {
    let (controller, worker, _root, children) = super::tests::replicated_run_tree(200);
    let earlier = worker
        .claims_for(&children[0].subject, None)
        .unwrap()
        .iter()
        .map(|claim| claim.accepted_at_unix_ms)
        .min()
        .unwrap();
    worker
        .set_step_state(&children[199].steps[0].subject, "blocked", Some("waits on a person"))
        .unwrap();
    controller.set_write_clock_at(earlier).unwrap();
    controller
        .set_step_state(&children[100].steps[0].subject, "blocked", Some("blocked early"))
        .unwrap();
    let inventory = worker.replication_inventory().unwrap();
    let (exchange, _) = stage("bound/standing-tree/export", || {
        super::tests::exchange_from(&controller, &inventory)
    });
    stage("bound/standing-tree/receive", || {
        worker
            .receive_replication_exchange("controller", &exchange.fleet_id, &exchange)
            .unwrap()
    });
    stage("bound/standing-tree/admit", || worker.validate_replication_backlog().unwrap());
    stage("bound/standing-tree/repairs", || worker.apply_replication_repairs().unwrap());
    FULL_REPLAYS.with(|replays| replays.set(0));
    let (projected, held) = stage("bound/standing-tree/project", || {
        worker.project_replication_backlog().unwrap()
    });
    assert!(projected);
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "the late claim replayed the store");
    assert!(held > Duration::ZERO, "the projection asked for no writer hold");
}
