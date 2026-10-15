// Included in store.rs's tests.
use crate::store::projection_divergence::{DivergenceCheck, projection_divergence_subject};

/// Two members that hold the same envelopes and agree on every projection.
fn two_members_in_step() -> (Store, Store) {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    right.capture_divergences(true);
    let intent = simple("true");
    let preview = left
        .mission(
            &intent,
            IntentInput {
                kdl: "work".into(),
                source_name: None,
            },
        )
        .unwrap();
    left.apply(&intent, &preview.subject_tokens, "work")
        .unwrap();
    receive_and_project(
        &right,
        "left",
        &exchange_from(&left, &right.replication_inventory().unwrap()),
    );
    receive_and_project(
        &left,
        "right",
        &exchange_from(&right, &left.replication_inventory().unwrap()),
    );
    (left, right)
}

/// The exchange a worker starts, from `from` to `to`.
fn compare(from: &Store, to: &Store, relay: &str) {
    let exchange = from.export_replication_summary(TEST_FLEET).unwrap();
    to.receive_replication_exchange(relay, TEST_FLEET, &exchange)
        .unwrap();
}

/// Make `store`'s projection of `exec/work` wrong while its claims stay, as an impure projection
/// would: the same claims, a different desired table.
fn project_wrongly(store: &Store) {
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TEMP TABLE wrong_desired AS SELECT * FROM desired WHERE subject='exec/work';
             PRAGMA foreign_keys=OFF;
             DELETE FROM desired WHERE subject='exec/work';
             PRAGMA foreign_keys=ON;",
        )
        .unwrap();
}

fn project_rightly(store: &Store) {
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             INSERT INTO desired SELECT * FROM wrong_desired;
             PRAGMA foreign_keys=ON;
             DROP TABLE wrong_desired;",
        )
        .unwrap();
}

fn desired_rows(store: &Store) -> i64 {
    store
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM desired WHERE subject='exec/work'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// Age the difference `store` sees with `peer` past the projection delay, so it is a divergence.
fn outlast_projection(store: &Store, peer: &str) {
    let mut sync = store.replication_sync.lock().unwrap();
    let progress = sync.get_mut(peer).unwrap();
    let since = progress.graph_differs_since_unix_ms.expect("graphs differ");
    progress.graph_differs_since_unix_ms = Some(since - REPLICATION_DIVERGED_AFTER_MS);
}

#[test]
fn a_projection_divergence_is_one_p1_fault_that_nothing_repairs() {
    let (left, right) = two_members_in_step();
    compare(&left, &right, "left");
    assert_eq!(right.open_divergences_at_last_check(), None, "no check has run");
    assert_eq!(
        right.check_projection_divergences().unwrap(),
        DivergenceCheck::default(),
        "members that agree have nothing to record"
    );
    assert_eq!(right.open_divergences_at_last_check(), Some(0));

    project_wrongly(&right);
    compare(&left, &right, "left");
    assert_eq!(
        right.check_projection_divergences().unwrap().opened,
        0,
        "one comparison can catch a peer mid-projection"
    );
    outlast_projection(&right, "left");
    compare(&left, &right, "left");
    let inventory = right.replication_inventory().unwrap().digest;
    assert_eq!(inventory, left.replication_inventory().unwrap().digest);

    let check = right.check_projection_divergences().unwrap();
    assert_eq!(
        check,
        DivergenceCheck {
            divergences: 1,
            opened: 1,
            recovered: 0
        }
    );
    let open = right.open_projection_divergences().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(right.open_divergences_at_last_check(), Some(1));
    let fault = open.values().next().unwrap();
    assert_eq!(fault.members, ["right", "left"]);
    assert_eq!(fault.failures.len(), 1);
    let claim = right.claim_by_id(&fault.failures[0]).unwrap().unwrap();
    assert_eq!(claim.subject, "daemon/right");
    assert_eq!(claim.kind, "operational.failure");
    let fields = &claim.body["fields"];
    assert_eq!(fields["condition"], "projection-divergence");
    assert_eq!(fields["severity"], "error");
    let reproduction = &fields["reproduction"];
    assert_eq!(reproduction["projection"], "desired");
    assert_eq!(reproduction["inventory_digest"], inventory.as_str());
    assert!(
        reproduction["writer_high_water"]["left"].as_u64().unwrap() > 0,
        "the claim range names each writer's high water: {reproduction}"
    );
    let digests = &reproduction["digests"];
    assert!(digests["left"].is_string() && digests["right"].is_string());
    assert_ne!(digests["left"], digests["right"]);
    let build = smallclaims::store::checkpoint_agreement::checkpoint_build();
    assert_eq!(reproduction["builds"]["right"], build.as_str());
    assert_eq!(
        reproduction["builds"]["left"],
        build.as_str(),
        "the peer's exchange says its build"
    );

    // It is a high-priority fault, and the same divergence is never recorded twice.
    let faults = right.fault_snapshot(now_ms()).unwrap();
    let item = faults
        .iter()
        .find(|fault| fault.item.subject == "daemon/right")
        .expect("the divergence is a fault");
    assert_eq!(item.item.priority, "high");
    compare(&left, &right, "left");
    assert_eq!(right.check_projection_divergences().unwrap().opened, 0);
    assert_eq!(right.open_projection_divergences().unwrap().len(), 1);

    // Nothing repaired the wrong projection from the peer.
    assert_eq!(desired_rows(&right), 0);
    assert_eq!(desired_rows(&left), 1);

    // Once the two project the same envelopes the same way, the fault ends. The fault claim is an
    // envelope too, so the members compare again once it has reached the peer.
    project_rightly(&right);
    receive_and_project(
        &left,
        "right",
        &exchange_from(&right, &left.replication_inventory().unwrap()),
    );
    compare(&left, &right, "left");
    let check = right.check_projection_divergences().unwrap();
    assert_eq!((check.divergences, check.opened, check.recovered), (0, 0, 1));
    assert!(right.open_projection_divergences().unwrap().is_empty());
    assert_eq!(right.open_divergences_at_last_check(), Some(0));
}

#[test]
fn without_the_check_an_exchange_keeps_nothing_for_a_reproduction() {
    let (left, right) = two_members_in_step();
    right.capture_divergences(false);
    project_wrongly(&right);
    compare(&left, &right, "left");
    outlast_projection(&right, "left");
    compare(&left, &right, "left");
    assert!(right.replication_diverged(), "status still reports it");
    assert!(right.replication_divergences().is_empty());
    assert_eq!(
        right.check_projection_divergences().unwrap(),
        DivergenceCheck::default()
    );
}

/// `count` divergence faults between invented members, every other one ended, as months of
/// history might leave on `store`'s daemon subject.
fn past_divergence_faults(store: &Store, count: usize) {
    for index in 0..count {
        let failure = store
            .append_claim(&crate::model::ClaimInput {
                subject: projection_divergence_subject(store.origin()),
                kind: "operational.failure".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("episode".into(), json!(format!("projection-divergence/{index}"))),
                    ("reproduction".into(), json!({"members": ["right", "cedar"]})),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("measure-{index}")),
            })
            .unwrap();
        if index % 2 == 0 {
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: projection_divergence_subject(store.origin()),
                    kind: "operational.recovered".into(),
                    actor: Some("daemon/runtime".into()),
                    fields: BTreeMap::from([
                        ("episode".into(), json!(format!("projection-divergence/{index}"))),
                        ("failure".into(), json!(failure.id)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("measure-{index}:recovered")),
                })
                .unwrap();
        }
    }
}

#[test]
fn an_old_open_divergence_is_not_recorded_again_after_many_newer_faults() {
    let (left, right) = two_members_in_step();
    project_wrongly(&right);
    compare(&left, &right, "left");
    outlast_projection(&right, "left");
    compare(&left, &right, "left");
    assert_eq!(right.check_projection_divergences().unwrap().opened, 1);
    let episode = right.open_projection_divergences().unwrap().into_keys().next().unwrap();

    // More divergence faults than any newest-first window would read.
    past_divergence_faults(&right, 300);
    compare(&left, &right, "left");
    let check = right.check_projection_divergences().unwrap();
    assert_eq!((check.divergences, check.opened), (1, 0), "the old episode is still open");
    let open = right.open_projection_divergences().unwrap();
    assert_eq!(open[&episode].failures.len(), 1);
    assert_eq!(open.len(), 1 + 150);
    assert_eq!(right.open_divergences_at_last_check(), Some(151));
}

/// What one check costs on a member with many past divergence faults, and what capturing the
/// claim range costs on a large inventory. Prints the numbers; run with `--ignored --nocapture`.
#[test]
#[ignore = "measurement"]
fn measure_projection_divergence_check_cost() {
    let (left, right) = two_members_in_step();
    past_divergence_faults(&right, 300);
    compare(&left, &right, "left");
    let rounds = 1_000;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
        right.check_projection_divergences().unwrap();
    }
    let per_check = started.elapsed() / rounds;
    let inventory = smallclaims::store::CompactReplicationInventory::from_sorted(
        ["alder", "birch", "cedar", "delta"].into_iter().flat_map(|writer| {
            (1..=250_000_u64).map(move |sequence| smallclaims::claim::ReplicaEnvelopeId {
                writer: writer.into(),
                sequence,
                hash: hex::encode(Sha256::digest(format!("{writer}{sequence}").as_bytes())),
            })
        }),
    );
    let started = std::time::Instant::now();
    let high_water = inventory.writer_high_water();
    let capture = started.elapsed();
    assert_eq!(high_water["delta"], 250_000);
    println!(
        "projection divergence check: {per_check:?} per check over {rounds} checks with 300 \
         past faults (150 open); claim-range capture over 1,000,000 envelopes: {capture:?}"
    );
}
