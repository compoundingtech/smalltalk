#[test]
fn mailbox_fence_cost_is_bounded_with_sparse_observation_history() {
    fn measure(
        history: usize,
        seats: usize,
        observed_incarnation: Option<&str>,
        state: &str,
    ) -> (i32, std::time::Duration) {
        let store = Store::open_memory("node").unwrap();
        let mut connection = store.connection.lock().unwrap();
        let tx = connection.transaction().unwrap();
        for seat in 0..seats {
            for n in 0..=history {
                let id = format!("observation-{seat}-{n}");
                let subject = format!("agent/eval.worker-{seat}");
                let kind = if n == 0 {
                    "runtime.observed"
                } else {
                    "harness.observed"
                };
                let fields = if n == 0 {
                    json!({"status":"running", "incarnation_id":"current"})
                } else {
                    let mut fields = json!({"state":state, "driver":"claude"});
                    if let Some(incarnation) = observed_incarnation {
                        fields["incarnation_id"] = json!(incarnation);
                    }
                    fields
                };
                tx.execute(
                    "INSERT INTO batches VALUES (?1,'node',?2,NULL,?1,?3)",
                    params![id, (seat * (history + 1) + n) as i64, (n + 1).to_string()],
                )
                .unwrap();
                tx.execute("INSERT INTO claims (id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                    VALUES (?1,?1,?5,?2,'node',?3,'[]',?4)",
                    params![id, kind, json!({"fields":fields}).to_string(), (n + 1).to_string(), subject]).unwrap();
            }
        }
        tx.commit().unwrap();
        let start = std::time::Instant::now();
        for seat in 0..seats {
            let fence = crate::mailbox::Fence::new(
                &format!("agent/eval.worker-{seat}"),
                "current",
                "delivery",
            );
            let admission = check_mailbox_incarnation(&connection, &fence);
            if state == "ended" && observed_incarnation != Some("prior") {
                assert_eq!(admission.unwrap_err().code, "stale-mailbox-session");
            } else {
                admission.unwrap();
            }
        }
        let elapsed = start.elapsed();
        let query = newest_claims_of_kind_query(
            "claims.id, claims.store_index, claims.body, claims.accepted_at_unix_ms",
            "harness.observed",
        );
        // Count both paths: this also detects accidentally reintroducing the display fold.
        let steps = [
            query,
            // #949's streaming state read must not return as an unbounded fallback either.
            newest_claims_of_kind_query("claims.id, claims.body", "harness.observed"),
            newest_mailbox_harness_state_query(),
            format!(
                "{} LIMIT 1",
                newest_claims_of_kind_query("claims.id, claims.body", "runtime.observed")
            ),
        ]
        .iter()
        .map(|query| {
            connection
                .prepare_cached(query)
                .unwrap()
                .reset_status(rusqlite::StatementStatus::VmStep)
        })
        .sum();
        (steps, elapsed)
    }
    for (seats, incarnation, state) in [
        (1, Some("current"), "idle"),
        (1, Some("prior"), "idle"),
        (1, None, "idle"),
        (7, Some("current"), "idle"),
        (1, Some("current"), "ended"),
    ] {
        let small = measure(100, seats, incarnation, state);
        let populated = measure(20_000, seats, incarnation, state);
        eprintln!(
            "mailbox incarnation checks (runtime/state lookup VM steps, full admission elapsed): {seats} seats, {incarnation:?}, {state}, 100 sparse reports per seat: {small:?}; 20,000 per seat: {populated:?}"
        );
        assert!(small.0 > 0);
        assert!(
            populated.0 <= small.0 + 1_000,
            "fence admission scanned observation history: {small:?} -> {populated:?}"
        );
    }
}

// Insert authoritative observations directly, so old wire shapes and replica arrival order
// can be exercised without the modern claim-input validator normalizing them.
fn insert_observation(
    connection: &Connection,
    id: &str,
    kind: &str,
    body: Value,
    time: u128,
    writer: &str,
) {
    connection
        .execute(
            "INSERT INTO batches VALUES (?1,?2,0,NULL,?1,?3)",
            params![id, writer, time.to_string()],
        )
        .unwrap();
    let subject = if kind.starts_with("work.") {
        "step-run/eval/work"
    } else {
        "agent/eval.worker"
    };
    connection.execute(
        "INSERT INTO claims (id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
         VALUES (?1,?1,?2,?3,?4,'agent/eval.worker',?5,'[]',?6)",
        params![id, subject, kind, writer, body.to_string(), time.to_string()],
    ).unwrap();
}

fn insert_runtime(connection: &Connection) {
    insert_observation(
        connection,
        "runtime",
        "runtime.observed",
        json!({"fields":{"status":"running", "incarnation_id":"current"}}),
        100,
        "node",
    );
}

fn assert_ended(connection: &Connection, expected: bool) {
    let folded = current_harness_at(connection, "agent/eval.worker", None)
        .unwrap()
        .is_some_and(|harness| harness.state == "ended");
    assert_eq!(
        folded, expected,
        "fixture must exercise the expected display-fold decision"
    );
    assert_eq!(
        mailbox_harness_ended(connection, "agent/eval.worker", "current", "runtime").unwrap(),
        folded
    );
    let admitted = check_mailbox_incarnation(
        connection,
        &crate::mailbox::Fence::new("agent/eval.worker", "current", "delivery"),
    );
    if expected {
        assert_eq!(admitted.unwrap_err().code, "stale-mailbox-session");
    } else {
        admitted.unwrap();
    }
}

#[test]
fn mailbox_fence_state_matches_display_fold_for_incarnations_and_legacy_shapes() {
    for (body, time, expected) in [
        (
            json!({"fields":{"state":"ended", "incarnation_id":"current"}}),
            50,
            true,
        ),
        (
            json!({"fields":{"state":"ended", "incarnation_id":"prior"}}),
            200,
            false,
        ),
        (json!({"state":"ended"}), 50, false),
        (json!({"state":"ended"}), 200, true),
        (json!({"fields":{"state":"ended"}}), 200, true),
        (
            json!({"fields":{"state":"ended", "incarnation_id":null}}),
            200,
            true,
        ),
        (
            json!({"fields":{"state":"ended", "incarnation_id":123}}),
            200,
            true,
        ),
        (json!({"fields":null, "state":"ended"}), 200, false),
        (
            json!({"fields":{"state":null, "incarnation_id":"current"}}),
            200,
            false,
        ),
        (
            json!({"fields":{"state":"idle", "incarnation_id":"current"}}),
            200,
            false,
        ),
    ] {
        let store = Store::open_memory("node").unwrap();
        let connection = store.connection.lock().unwrap();
        insert_runtime(&connection);
        assert_ended(&connection, false);
        insert_observation(
            &connection,
            "observation",
            "harness.observed",
            body,
            time,
            "node",
        );
        assert_ended(&connection, expected);
    }
}

#[test]
fn mailbox_fence_state_matches_display_fold_for_work_and_prompt_overrides() {
    for kind in ["work.claimed", "work.progress", "work.renewed"] {
        for incarnation in ["current", "prior"] {
            for time in [50, 150, 250] {
                let store = Store::open_memory("node").unwrap();
                let connection = store.connection.lock().unwrap();
                insert_runtime(&connection);
                insert_observation(
                    &connection,
                    "ended",
                    "harness.observed",
                    json!({"fields":{"state":"ended", "incarnation_id":"current"}}),
                    200,
                    "node",
                );
                assert_ended(&connection, true);
                insert_observation(
                    &connection,
                    "work",
                    kind,
                    json!({"fields":{"claim_incarnation":incarnation}}),
                    time,
                    "node",
                );
                assert_ended(
                    &connection,
                    !(kind != "work.renewed" && incarnation == "current" && time > 200),
                );
            }
        }
    }
    for code in ["provider-auth-expired", "provider-trust-prompt"] {
        let store = Store::open_memory("node").unwrap();
        let connection = store.connection.lock().unwrap();
        insert_runtime(&connection);
        insert_observation(
            &connection,
            "ended",
            "harness.observed",
            json!({"fields":{"state":"ended", "incarnation_id":"current"}}),
            200,
            "node",
        );
        insert_observation(
            &connection,
            "foreign-prompt",
            "harness.diagnostic",
            json!({"fields":{"code":code, "incarnation_id":"prior"}}),
            500,
            "node",
        );
        assert_ended(&connection, true);
        insert_observation(
            &connection,
            "prompt",
            "harness.diagnostic",
            json!({"fields":{"code":code, "incarnation_id":"current"}}),
            150,
            "node",
        );
        assert_ended(&connection, false);
        insert_observation(
            &connection,
            "restored",
            "harness.diagnostic",
            json!({"fields":{"code":"provider-auth-restored", "incarnation_id":"current"}}),
            250,
            "node",
        );
        assert_ended(&connection, true);
    }
}

#[test]
fn mailbox_fence_state_uses_canonical_order_instead_of_replica_arrival() {
    for reverse in [false, true] {
        let store = Store::open_memory("node").unwrap();
        let connection = store.connection.lock().unwrap();
        insert_runtime(&connection);
        let mut observations = vec![
            ("a-older", json!({"state":"ended"}), 100, "a"), // legacy before runtime at equal time
            (
                "b-current",
                json!({"fields":{"state":"ended", "incarnation_id":"current"}}),
                200,
                "a",
            ),
            ("c-legacy", json!({"state":"idle"}), 200, "z"), // beats explicit at equal time
            (
                "d-prior",
                json!({"fields":{"state":"ended", "incarnation_id":"prior"}}),
                300,
                "z",
            ),
        ];
        if reverse {
            observations.reverse();
        }
        for (id, body, time, writer) in observations {
            insert_observation(&connection, id, "harness.observed", body, time, writer);
        }
        assert_ended(&connection, false);
        insert_observation(
            &connection,
            "e-current",
            "harness.observed",
            json!({"fields":{"state":"ended", "incarnation_id":"current"}}),
            200,
            "zz",
        );
        assert_ended(&connection, true);
    }
}

#[test]
fn mailbox_fence_state_index_backfills_existing_stores_and_seeks_both_epochs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    {
        let store = Store::open(&path, "node").unwrap();
        let connection = store.connection.lock().unwrap();
        connection
            .execute_batch("DROP INDEX claims_harness_state_incarnation_accepted_index")
            .unwrap();
        insert_runtime(&connection);
        insert_observation(
            &connection,
            "ended",
            "harness.observed",
            json!({"fields":{"state":"ended", "incarnation_id":"current"}}),
            200,
            "node",
        );
        insert_observation(
            &connection,
            "legacy",
            "harness.observed",
            json!({"state":"idle"}),
            250,
            "node",
        );
    }
    let store = Store::open(&path, "node").unwrap();
    let connection = store.connection.lock().unwrap();
    assert_ended(&connection, false);
    for incarnation in [Some("current"), None] {
        let plan = connection
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                newest_mailbox_harness_state_query()
            ))
            .unwrap()
            .query_map(params!["agent/eval.worker", incarnation], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("SEARCH claims USING INDEX claims_harness_state_incarnation_accepted_index (subject=? AND <expr>=?)"), "{plan}");
    }
}
