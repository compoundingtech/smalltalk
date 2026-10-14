use super::*;

fn append(store: &Store, kind: &str, fields: Value) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: "agent/grove/cedar".into(),
            kind: kind.into(),
            actor: Some("agent/grove/cedar".into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

#[test]
fn sparse_named_harness_fold_does_not_lookup_each_canonical_key() {
    let store = Store::open_memory("grove").unwrap();
    append(
        &store,
        "runtime.observed",
        json!({
            "status":"running", "runtime_id":"cedar", "incarnation_id":"current"
        }),
    );
    append(
        &store,
        "harness.observed",
        json!({
            "state":"idle", "driver":"codex", "incarnation_id":"current"
        }),
    );
    let measure = || {
        store
            .read_snapshot(|index| {
                let before = STATEMENTS_RUN.with(std::cell::Cell::get);
                let view = current_harness_fold_at(
                    &store.readers.get(),
                    "agent/grove/cedar",
                    Some(index),
                    false,
                    false,
                )
                .unwrap()
                .unwrap();
                let statements = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
                assert_eq!(view.state, "idle");
                assert_eq!(view.driver.as_deref(), Some("codex"));
                assert_eq!(view.reason, None);
                assert_eq!(view.ask, None);
                assert_eq!(view.blocked_on, None);
                Ok(statements)
            })
            .unwrap()
    };
    let before = measure();
    for _ in 0..128 {
        append(
            &store,
            "harness.observed",
            json!({
                "state":"idle", "incarnation_id":"current", "status_transition":false
            }),
        );
    }
    let after = measure();
    println!("sparse named fold statements: one={before}, 129={after}");
    assert!(
        after <= before + 1,
        "canonical SQL already orders named rows: {before} -> {after}"
    );
}

#[test]
fn lazy_named_keys_merge_legacy_nulls_and_preserve_historical_cut() {
    let store = Store::open_memory("grove").unwrap();
    append(
        &store,
        "runtime.observed",
        json!({
            "status":"running", "runtime_id":"cedar", "incarnation_id":"current"
        }),
    );
    let first = append(
        &store,
        "harness.observed",
        json!({
            "state":"idle", "driver":"codex", "reason":"old", "incarnation_id":"current"
        }),
    );
    let cut = store.index().unwrap();
    let legacy = append(
        &store,
        "harness.observed",
        json!({
            "state":"working", "reason":null, "blocked_on":"human", "ask":"permission"
        }),
    );
    let latest = append(
        &store,
        "harness.observed",
        json!({
            "state":"idle", "blocked_on":null, "ask":null, "incarnation_id":"current"
        }),
    );
    // Tie the two newest rows: the receiver's exact canonical batch/position decides,
    // rather than either arrival order or an assumed timestamp order.
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
        params![legacy.accepted_at_unix_ms.to_string(), latest.id],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(writer);
    store
        .read_snapshot(|index| {
            let connection = store.readers.get();
            let view = current_harness_fold_at(
                &connection,
                "agent/grove/cedar",
                Some(index),
                false,
                false,
            )
            .unwrap()
            .unwrap();
            let legacy_key = canonical::claim_key(&connection, &legacy.id).unwrap();
            let latest_key = canonical::claim_key(&connection, &latest.id).unwrap();
            if latest_key > legacy_key {
                assert_eq!(view.claim, latest.id);
                assert_eq!(view.state, "idle");
                assert_eq!(view.blocked_on, None);
                assert_eq!(view.ask, None);
            } else {
                assert_eq!(view.claim, legacy.id);
                assert_eq!(view.state, "blocked");
                assert_eq!(view.blocked_on.as_deref(), Some("human"));
                assert_eq!(view.ask.as_deref(), Some("permission"));
            }
            assert_eq!(view.driver.as_deref(), Some("codex"));
            assert_eq!(view.reason, None);
            let earlier =
                current_harness_fold_at(&connection, "agent/grove/cedar", Some(cut), false, false)
                    .unwrap()
                    .unwrap();
            assert_eq!(earlier.claim, first.id);
            assert_eq!(earlier.state, "idle");
            assert_eq!(earlier.reason.as_deref(), Some("old"));
            Ok(())
        })
        .unwrap();
}
