//! Targeted candidate invalidation controls. Invoked by the parent integration-test crate.
use super::{
    append, append_raw, check, desired_body, desired_candidates, node, noise, ordinary, row,
};
use serde_json::json;
use smallclaims::{ivm::Readiness, replication::ReplicationInventory, store::Runtime};

pub fn late_prior_host() {
    let source = ordinary();
    let amber = append_raw(&source, desired_body("a", Some("amber")));
    let cobalt = append_raw(&source, desired_body("a", Some("cobalt")));
    check(&source);
    let exchange = source
        .store
        .export_replication_exchange("fixture-fleet", &ReplicationInventory::default())
        .unwrap();
    let target = node("birch", &source.anchor, None);
    let delivery = |claim: &smallclaims::ClaimRecord| {
        let sequence = source
            .store
            .readers
            .get()
            .query_row(
                "SELECT replica_sequence FROM batches WHERE id=?1",
                [&claim.batch_id],
                |r| r.get::<_, u64>(0),
            )
            .unwrap();
        let mut delivery = exchange.clone();
        delivery.envelopes.retain(|e| e.sequence == sequence);
        assert_eq!(delivery.envelopes.len(), 1);
        delivery
    };
    let receive = |delivery: &smallclaims::replication::ReplicationExchange| {
        target
            .store
            .receive_replication_exchange("alder", "fixture-fleet", delivery)
            .unwrap();
        target.store.validate_replication_backlog().unwrap();
        target.store.project_replication_backlog().unwrap();
        check(&target);
    };
    receive(&delivery(&cobalt));
    let subject = "agent/fixture/a";
    let (selected, before) = target
        .store
        .read_snapshot(|_| {
            let c = target.store.readers.get();
            assert!(desired_candidates(&c, "amber")?.is_empty());
            Ok((
                row(&c, "desired-by-host", subject)?,
                target
                    .runtime
                    .views
                    .changed_keys(&c, "desired-by-host", 1, 0, 8, None)?,
            ))
        })
        .unwrap();
    receive(&delivery(&amber));
    target
        .store
        .read_snapshot(|_| {
            let c = target.store.readers.get();
            assert_eq!(row(&c, "desired-by-host", subject)?, selected);
            assert_eq!(desired_candidates(&c, "amber")?.len(), 1);
            let page = target.runtime.views.changed_keys(
                &c,
                "desired-by-host",
                1,
                before.frontier,
                8,
                None,
            )?;
            assert_eq!(page.token.generation, before.token.generation + 1);
            assert_eq!(
                page.keys.iter().map(|k| k.key.as_str()).collect::<Vec<_>>(),
                vec![subject]
            );
            Ok(())
        })
        .unwrap();
    // The old row-only equality control sees no change and would miss the new host's work.
    assert_eq!(
        row(&target.store.readers.get(), "desired-by-host", subject).unwrap(),
        selected
    );
    let token = target
        .runtime
        .views
        .token(&target.store.readers.get(), "desired-by-host", 1)
        .unwrap();
    receive(&delivery(&amber));
    assert_eq!(
        target
            .runtime
            .views
            .token(&target.store.readers.get(), "desired-by-host", 1)
            .unwrap(),
        token
    );
    noise(&target, subject);
    assert_eq!(
        target
            .runtime
            .views
            .token(&target.store.readers.get(), "desired-by-host", 1)
            .unwrap(),
        token
    );
    let frontier = target
        .runtime
        .views
        .changed_keys(
            &target.store.readers.get(),
            "desired-by-host",
            1,
            0,
            8,
            None,
        )
        .unwrap()
        .frontier;
    let mut writer = target.store.connection.write();
    let tx = writer.transaction().unwrap();
    let body = desired_body("a", Some("indigo"));
    target
        .runtime
        .append_claim_tx(
            &tx,
            &target.store.origin,
            subject,
            "intent.desired",
            Some("person/fixture"),
            &body,
            &[],
            None,
        )
        .unwrap();
    assert_eq!(desired_candidates(&tx, "indigo").unwrap().len(), 1);
    tx.rollback().unwrap();
    drop(writer);
    target
        .store
        .read_snapshot(|_| {
            let c = target.store.readers.get();
            assert!(desired_candidates(&c, "indigo")?.is_empty());
            let page =
                target
                    .runtime
                    .views
                    .changed_keys(&c, "desired-by-host", 1, frontier, 8, None)?;
            assert!(page.keys.is_empty());
            assert_eq!(page.token, token);
            assert_eq!(row(&c, "desired-by-host", subject)?, selected);
            Ok(())
        })
        .unwrap();
    check(&target);
}

pub fn owner_markers() {
    for (key, value) in [
        ("owner_run", "mission-run/fixture"),
        ("owner_step", "step-run/fixture/build"),
        ("owner_generation", "run-generation/fixture"),
    ] {
        let n = ordinary();
        append_raw(&n, desired_body("a", Some("amber")));
        check(&n);
        let mut body = desired_body("a", Some("cobalt"));
        body[key] = json!(value);
        let claim = append_raw(&n, body);
        n.store
            .read_snapshot(|_| {
                let c = n.store.readers.get();
                assert!(matches!(
                    n.runtime.views.readiness(&c, "desired-by-host", 1)?,
                    Readiness::Fenced
                ));
                assert!(n.runtime.views.token(&c, "desired-by-host", 1).is_err());
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM claims WHERE id=?1",
                        [&claim.id],
                        |r| r.get::<_, u64>(0)
                    )?,
                    1
                );
                assert_eq!(
                    row(&c, "desired-by-host", "agent/fixture/a")?.unwrap()["host"],
                    "amber"
                );
                Ok(())
            })
            .unwrap();
    }
}

pub fn auth_restoration() {
    let n = ordinary();
    let a = "agent/fixture/a";
    append(
        &n,
        a,
        "runtime.observed",
        None,
        json!({"incarnation_id":"current","status":"running"}),
    );
    append(
        &n,
        a,
        "harness.observed",
        None,
        json!({"incarnation_id":"current","state":"working","provider_auth":false}),
    );
    append(
        &n,
        "step-run/fixture/build",
        "work.claimed",
        Some(a),
        json!({"claim_incarnation":"current","status":"claimed"}),
    );
    check(&n);
    let before = row(&n.store.readers.get(), "agent-card", a)
        .unwrap()
        .unwrap();
    assert_eq!(before["status"], "needs-login");
    append(
        &n,
        a,
        "harness.observed",
        None,
        json!({"incarnation_id":"current","state":"working","provider_auth":true}),
    );
    check(&n);
    let after = row(&n.store.readers.get(), "agent-card", a)
        .unwrap()
        .unwrap();
    assert_eq!(after["status"], "working");
    assert_eq!(after["work"], before["work"]);
    super::replicate_permutations(&n, 0);
}
