const SEAT: &str = "agent/control/worker";
const ACTOR: &str = "person/avery";

fn publish(store: &Store, source: &str, key: &str) -> String {
    let intent = parse_intent(source, store.origin()).unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: Some(key.into()),
            },
        )
        .unwrap();
    assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
    store
        .apply_as(&intent, &preview.subject_tokens, key, Some(ACTOR))
        .unwrap();
    store.selected_desired_token(SEAT).unwrap().unwrap()
}

fn seat_source(host: &str) -> String {
    format!(
        "version 2\nagent \"control/worker\" {{ host {host:?}; restart \"always\"; command \"true\" }}\n"
    )
}

fn observe(store: &Store, state: &str) {
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("runtime_id".into(), json!("control-worker")),
                ("status".into(), json!(state)),
                ("host".into(), json!("amber")),
                ("incarnation_id".into(), json!("41:2026-10-08T00:00:00Z")),
                ("terminal".into(), json!(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn view(store: &Store) -> SubjectStatus {
    store.status(Some(SEAT)).unwrap().subjects.remove(0)
}

fn witness(label: &str, store: &Store) {
    let status = view(store);
    eprintln!(
        "stopped-seat-control {}",
        json!({
            "phase": label, "origin": store.origin(), "desired_token": status.desired_token,
            "kind": status.kind, "conflicts": status.conflicts, "actual": status.actual,
        })
    );
}

fn interleaved_stop(pinned: bool) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("amber.sqlite3"), "amber").unwrap());
    let original = publish(&store, &seat_source("amber"), "initial");
    observe(&store, "running");
    let writer = store.clone();
    STATUS_AFTER_DESIRED_READ.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            std::thread::spawn(move || {
                publish(
                    &writer,
                    "version 2\nstop \"agent/control/worker\"\n",
                    "stop",
                );
                observe(&writer, "stopped");
            })
            .join()
            .unwrap();
        }))
    });
    // The request reader loan alone intentionally has no BEGIN. A pinned caller must still
    // hold one coherent answer while this independently committed stop lands.
    let answer = store
        .readers
        .request_read(|| {
            if pinned {
                store.read_snapshot(|_| Ok(view(&store))).unwrap()
            } else {
                view(&store)
            }
        })
        .unwrap();
    eprintln!(
        "stopped-seat-interleave {}",
        json!({
            "pinned": pinned, "desired_token": answer.desired_token,
            "kind": answer.kind, "conflicts": answer.conflicts, "actual": answer.actual,
        })
    );
    assert!(
        answer.conflicts.is_empty(),
        "a linear stop must not appear as a declaration fork: {answer:?}"
    );
    assert_eq!(answer.desired_token.as_deref(), Some(original.as_str()));
    assert_eq!(answer.actual.as_ref().unwrap()["status"], "running");
    let after = view(&store);
    assert_eq!(after.kind.as_deref(), Some("stop"));
    assert!(after.conflicts.is_empty());
    assert_eq!(after.actual.unwrap()["status"], "stopped");
}

#[test]
fn a_raw_stopped_seat_status_keeps_token_conflicts_and_actual_in_one_read() {
    interleaved_stop(false);
}

#[test]
fn a_pinned_stopped_seat_status_keeps_token_conflicts_and_actual_in_one_read() {
    interleaved_stop(true);
}

#[test]
fn stopped_actual_on_both_replicas_does_not_resolve_a_real_stop_fork() {
    let directory = tempfile::tempdir().unwrap();
    let amber = Store::open(&directory.path().join("amber.sqlite3"), "amber").unwrap();
    let cobalt = Store::open(&directory.path().join("cobalt.sqlite3"), "cobalt").unwrap();
    let original = publish(&amber, &seat_source("amber"), "initial");
    observe(&amber, "running");
    receive_and_project(
        &cobalt,
        "amber",
        &exchange_from(&amber, &cobalt.replication_inventory().unwrap()),
    );
    let stop = "version 2\nstop \"agent/control/worker\"\n";
    let left = publish(&amber, stop, "left-stop");
    let right = publish(&cobalt, stop, "right-stop");
    assert_ne!(left, right);
    for (store, token) in [(&amber, &left), (&cobalt, &right)] {
        assert_eq!(
            store.claim_by_id(token).unwrap().unwrap().predecessors,
            vec![original.clone()]
        );
    }
    observe(&amber, "stopped");
    receive_and_project(
        &cobalt,
        "amber",
        &exchange_from(&amber, &cobalt.replication_inventory().unwrap()),
    );
    receive_and_project(
        &amber,
        "cobalt",
        &exchange_from(&cobalt, &amber.replication_inventory().unwrap()),
    );
    for store in [&amber, &cobalt] {
        witness("real-stop-fork", store);
        let status = view(store);
        assert_eq!(status.actual.unwrap()["status"], "stopped");
        assert_eq!(status.conflicts.len(), 1);
        let intent = parse_intent(&seat_source("cobalt"), "cobalt").unwrap();
        let before = store.index().unwrap();
        let error = store
            .apply_as(
                &intent,
                &BTreeMap::from([(SEAT.into(), vec![status.desired_token.unwrap()])]),
                "selected-only-start",
                Some(ACTOR),
            )
            .unwrap_err();
        assert_eq!(error.code, "stale-subject");
        assert_eq!(
            store.index().unwrap(),
            before,
            "refused start must not change the graph"
        );
    }
}

#[test]
fn a_serial_stop_then_move_uses_the_stop_token_and_preserves_stale_start_rejection() {
    let directory = tempfile::tempdir().unwrap();
    let amber = Store::open(&directory.path().join("amber.sqlite3"), "amber").unwrap();
    let cobalt = Store::open(&directory.path().join("cobalt.sqlite3"), "cobalt").unwrap();
    let original = publish(&amber, &seat_source("amber"), "initial");
    observe(&amber, "running");
    receive_and_project(
        &cobalt,
        "amber",
        &exchange_from(&amber, &cobalt.replication_inventory().unwrap()),
    );
    let stop = publish(&amber, "version 2\nstop \"agent/control/worker\"\n", "stop");
    observe(&amber, "stopped");
    receive_and_project(
        &cobalt,
        "amber",
        &exchange_from(&amber, &cobalt.replication_inventory().unwrap()),
    );
    for store in [&amber, &cobalt] {
        witness("serial-stop", store);
        let status = view(store);
        assert_eq!(status.desired_token.as_deref(), Some(stop.as_str()));
        assert!(status.conflicts.is_empty());
        assert_eq!(status.actual.unwrap()["status"], "stopped");
        let ended = store.declaration_ended_by_stop(SEAT).unwrap().unwrap();
        assert_eq!(ended.token, stop);
        assert_eq!(ended.declaration.member.unwrap().host, "amber");
    }
    let intent = parse_intent(&seat_source("cobalt"), "cobalt").unwrap();
    let error = cobalt
        .apply_as(
            &intent,
            &BTreeMap::from([(SEAT.into(), vec![original])]),
            "stale-start",
            Some(ACTOR),
        )
        .unwrap_err();
    assert_eq!(error.code, "stale-subject");
    let moved = publish(&cobalt, &seat_source("cobalt"), "fresh-move");
    assert_eq!(
        cobalt.claim_by_id(&moved).unwrap().unwrap().predecessors,
        vec![stop]
    );
    receive_and_project(
        &amber,
        "cobalt",
        &exchange_from(&cobalt, &amber.replication_inventory().unwrap()),
    );
    for store in [&amber, &cobalt] {
        witness("fresh-move", store);
        let status = view(store);
        assert_eq!(status.desired_token.as_deref(), Some(moved.as_str()));
        assert!(status.conflicts.is_empty());
        assert_eq!(
            store
                .desired_subject_with_writer(SEAT)
                .unwrap()
                .unwrap()
                .0
                .member
                .unwrap()
                .host,
            "cobalt"
        );
    }
}
