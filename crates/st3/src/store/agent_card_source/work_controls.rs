//! Count actual transactional repairs without treating scheduling receipts as readiness.
use super::*;

fn captured_clock(store: &Store) -> Mutation {
    tests::capture(store)
        .into_iter()
        .find(|row| key_parts(&row.key).unwrap().0 == "local_agent_card_clock")
        .unwrap()
}

#[test]
fn counted_clock_spends_one_shared_page_and_rollback_restores_work() {
    let store = Store::open_memory("node").unwrap();
    let ns = tests::context(&store);
    tests::clock(&store);
    let clock = captured_clock(&store);
    let kernel = Kernel::new("node");
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    kernel
        .apply(&tx, &ns, std::slice::from_ref(&clock))
        .unwrap();
    for index in 0..257 {
        queue(&tx, &ns, "card", &format!("agent/work-control/{index:03}")).unwrap();
    }
    tx.commit().unwrap();
    let tx = writer.transaction().unwrap();
    let first = kernel
        .apply_with_work(&tx, &ns, std::slice::from_ref(&clock))
        .unwrap();
    assert_eq!(first.namespace, ns);
    assert_eq!(first.used, WORK);
    let pending = diagnostic(&tx, &ns).unwrap();
    assert_eq!(pending["card_pending"], true);
    assert_eq!(pending["authority"]["closed"], true);
    assert_eq!(pending["lifecycle"]["closed"], true);
    assert_eq!(pending["launch"]["closed"], true);
    let stamp = first.clock.unwrap();
    let row = clock.new.as_ref().unwrap();
    assert_eq!(stamp.revision, row["revision"].as_u64().unwrap());
    assert_eq!(stamp.at.to_string(), row["at_ms"].as_str().unwrap());
    assert_eq!(
        stamp.snapshot_index,
        row["snapshot_index"].as_u64().unwrap()
    );
    assert_eq!(
        tx.query_row(
            "SELECT count(*) FROM local_agent_card_source_work WHERE namespace=?1 AND kind='card'",
            [ns.as_str()],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        129
    );
    tx.rollback().unwrap();
    assert_eq!(writer.query_row("SELECT count(*) FROM local_agent_card_source_work WHERE namespace=?1 AND kind='card'", [ns.as_str()], |r| r.get::<_,usize>(0)).unwrap(),257);
    for expected in [128, 128, 1] {
        let tx = writer.transaction().unwrap();
        let applied = kernel
            .apply_with_work(&tx, &ns, std::slice::from_ref(&clock))
            .unwrap();
        assert_eq!(applied.used, expected);
        assert_eq!(applied.namespace, ns);
        tx.commit().unwrap();
    }
    assert!(!card_work(&writer, &ns).unwrap());
    assert_eq!(diagnostic(&writer, &ns).unwrap()["card_pending"], false);
    assert!(
        kernel
            .apply_with_work(&writer.transaction().unwrap(), &ns, &[])
            .unwrap()
            .clock
            .is_none()
    );
}

#[test]
fn explicit_composed_binding_retains_receiver_and_position_fences() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_with_ivm_views(
        &root.path().join("source.sqlite"),
        "node",
        Arc::new(smallclaims::ivm::Views::new(agent_card_ivm::definitions()).unwrap()),
    )
    .unwrap();
    let ns = tests::context(&store);
    let at = tests::clock(&store);
    let clock = captured_clock(&store);
    let installer =
        smallclaims::ivm::install::Installer::new(vec![Box::new(Kernel::new("node"))]).unwrap();
    let native = agent_source::capture_fingerprint_for("node").unwrap();
    let suffix = native.rsplit_once(";receiver-sha256=").unwrap().1;
    let common = format!("fixture.union.receiver-bound;receiver-sha256={suffix}");
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    installer
        .register_source(&tx, agent_card_ivm::SOURCE, &common, 1)
        .unwrap();
    Kernel::new("node").apply(&tx, &ns, &[clock]).unwrap();
    let position = installer.position(&tx, agent_card_ivm::SOURCE).unwrap();
    let cut = smallclaims::ivm::source_cut(&tx).unwrap().unwrap();
    assert!(
        certify_coverage(&tx, &ns, "node", &position, &cut, at).is_err(),
        "default capture must reject the union descriptor"
    );
    assert!(
        certify_coverage_for_source(&tx, &ns, "other-node", &common, &position, &cut, at).is_err(),
        "a foreign receiver cannot certify the same namespace"
    );
    assert!(certify_coverage_for_source(&tx, &ns, "node", "unbound", &position, &cut, at).is_err());
    installer
        .source_gap(&tx, agent_card_ivm::SOURCE, "fixture coverage lost")
        .unwrap();
    assert!(
        certify_coverage_for_source(&tx, &ns, "node", &common, &position, &cut, at).is_err(),
        "an explicit matching descriptor cannot clear a source gap"
    );
    tx.rollback().unwrap();
}
