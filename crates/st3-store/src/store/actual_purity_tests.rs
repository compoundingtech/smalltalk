fn actual_subject(category: usize) -> &'static str {
    if category == 0 {
        "daemon/alder"
    } else {
        "agent/alder.worker"
    }
}

fn actual_snapshot(store: &Store, subject: &str) -> Value {
    let index = store.index().unwrap();
    let connection = store.readers.get();
    let status = store.status(Some(subject)).unwrap().subjects.remove(0);
    let actual = latest_actual_at(&connection, subject, Some(index)).unwrap();
    let selected = selected_actual_source_at(&connection, subject, Some(index), None).unwrap();
    let authority = runtime_only_authority(
        &connection,
        subject,
        index,
        &[
            "status",
            "runtime_id",
            "incarnation_id",
            "terminal",
            "reachability",
        ],
    )
    .unwrap();
    let mut reads = card_fold::CardReads::new(index);
    let card = store
        .agent_card_statuses(&connection, &[subject.into()], index, &mut reads)
        .unwrap()
        .remove(subject)
        .unwrap()
        .0;
    json!({
        "actual": actual, "selected": selected, "authority": authority,
        "origin": store.selected_actual_origin(subject).unwrap(),
        "card_actual": card.actual, "card_claim": card.actual_claim,
        "card_origin": card.actual_origin,
        "status_actual": status.actual, "status_claim": status.actual_claim,
        "status_origin": status.actual_origin,
    })
}

fn repair_fixture(category: usize) -> (ReplicationExchange, String, Value) {
    let source = Store::open_memory("alder").unwrap();
    let reference = Store::open_memory("birch").unwrap();
    let subject = actual_subject(category);
    let kind = if category == 0 {
        "daemon.diagnostic"
    } else {
        "runtime.observed"
    };
    let append = |time, original: bool| {
        source.set_write_clock_at(time).unwrap();
        let fields = if category == 0 {
            json!({"severity":"warning", "code":"repair-test", "reason":if original {"original"} else {"replacement"}})
        } else {
            json!({"status":"running", "runtime_id":if original {"original"} else {"replacement"},
                "incarnation_id":"one", "terminal":original, "reachability":"reachable"})
        };
        let mut connection = source.connection.write();
        let tx = connection.transaction().unwrap();
        let claim = append_claim_tx(
            &tx,
            "alder",
            subject,
            kind,
            None,
            &json!({"fields":fields}),
            &[],
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        claim
    };
    let replacement = append(10, false);
    let original = append(20, true);
    if category == 2 {
        // Force the authority reader's inherited-field path as well as its newest-row path.
        source.set_write_clock_at(30).unwrap();
        let mut connection = source.connection.write();
        let tx = connection.transaction().unwrap();
        append_claim_tx(
            &tx,
            "alder",
            subject,
            kind,
            None,
            &json!({"fields":{"status":"running", "incarnation_id":"two"}}),
            &[],
            None,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    receive_and_project(
        &reference,
        "alder",
        &exchange_from(&source, &ReplicationInventory::default()),
    );
    let record = reference
        .replica_records(false)
        .unwrap()
        .into_iter()
        .find(|r| r.claim_id.as_deref() == Some(original.id.as_str()))
        .unwrap();
    reference
        .connection
        .write()
        .execute(
            "UPDATE replica_records SET state='unknown' WHERE record_ref=?1",
            [&record.record_ref],
        )
        .unwrap();
    reference
        .repair_replica_record(
            &record.record_ref,
            &replacement.id,
            "replace an unsupported original",
            "person/robin",
            "repair",
        )
        .unwrap();
    let exchange = exchange_from(&reference, &ReplicationInventory::default());
    reference
        .connection
        .write()
        .execute("DELETE FROM claims WHERE id=?1", [&original.id])
        .unwrap();
    (exchange, original.id, actual_snapshot(&reference, subject))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn actual_state_ignores_repaired_originals_under_permutations_splits_and_duplicates(
        category in 0usize..3,
        ranks in proptest::collection::vec(any::<u64>(), 8),
        width in 1usize..6,
        copies in 1usize..4,
    ) {
        let subject = actual_subject(category);
        let (exchange, original, expected) = repair_fixture(category);
        prop_assert!(exchange.envelopes.len() <= ranks.len());
        let mut shuffled = (0..exchange.envelopes.len()).collect::<Vec<_>>();
        shuffled.sort_by_key(|&index| (ranks[index], index));
        for order in [(0..exchange.envelopes.len()).rev().collect::<Vec<_>>(), shuffled] {
            let target = Store::open_memory("cedar").unwrap();
            for chunk in order.chunks(width) {
                let envelopes = chunk.iter().flat_map(|&index|
                    std::iter::repeat_n(exchange.envelopes[index].clone(), copies)).collect();
                let admission = receive_and_project(&target, "birch", &exchange_of("birch", envelopes));
                prop_assert_eq!((admission.invalid, admission.unknown), (0, 0));
                // Warm the public status cache before the next receipt/repair changes its inputs.
                target.status(Some(subject)).unwrap();
            }
            prop_assert!(target.claim_by_id(&original).unwrap().is_some());
            prop_assert_eq!(actual_snapshot(&target, subject), expected.clone());
            receive_and_project(&target, "birch", &exchange);
            prop_assert_eq!(actual_snapshot(&target, subject), expected.clone());
            // Trimming a local receipt must not revive a logically retired source.
            target.connection.write().execute(
                "DELETE FROM replica_records WHERE claim_id=?1", [&original],
            ).unwrap();
            prop_assert_eq!(actual_snapshot(&target, subject), expected.clone());
            target.replay_replication_graph().unwrap();
            prop_assert_eq!(actual_snapshot(&target, subject), expected.clone());
        }
    }
}
