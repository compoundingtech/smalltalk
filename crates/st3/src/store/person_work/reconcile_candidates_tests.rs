//! Native cancellation controls plus explicitly synthetic retained-history reader budgets.
use super::*;
use rusqlite::StatementStatus;

fn insert_retained_ask(
    connection: &Connection,
    id: &str,
    subject: &str,
    ask: &ClaimRecord,
    time: u128,
) {
    // These rows model retained reader history, not admission or replication qualification.
    connection
        .execute(
            "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
         VALUES(?1,'alder',1,?1,?2)",
            params![id, time.to_string()],
        )
        .unwrap();
    connection.execute(
        "INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
         VALUES(?1,?1,?2,'work.person-asked','alder',?3,?4,'[]',?5)",
        params![id, subject, ask.actor, ask.body.to_string(), time.to_string()],
    ).unwrap();
}

#[test]
fn person_ask_candidate_cost_does_not_grow_with_settled_history() {
    let (store, _, input) = super::tests::fixture();
    let ask = store.ask_person(&input).unwrap();
    let record = request(&store.readers.get(), &ask.subject)
        .unwrap()
        .unwrap();
    let mut costs = Vec::new();
    for n in 1..=10_000 {
        {
            let connection = store.connection.write();
            let subject = format!("step-run/history-{n}/ask");
            connection.execute(
                "INSERT INTO step_runs(subject,run_id,generation_id,step_path,definition_hash,status,
                 attempt,assignee,available_to,agentless,title,goals,created_at_unix_ms,updated_at_unix_ms,constraints)
                 SELECT ?1,run_id,generation_id,?1,definition_hash,'completed',
                 attempt,assignee,available_to,agentless,title,goals,created_at_unix_ms,updated_at_unix_ms,constraints
                 FROM step_runs WHERE subject=?2", params![subject, ask.subject],
            ).unwrap();
            insert_retained_ask(
                &connection,
                &format!("historical-ask-{n}"),
                &subject,
                &record,
                1,
            );
        }
        if n == 100 || n == 10_000 {
            let connection = store.readers.get();
            let query = canonical_sql(PERSON_ASKS_FOR_RECONCILE);
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("; ");
            assert!(plan.contains("USING INDEX step_runs_open_index"), "{plan}");
            {
                let statement = connection.prepare_cached(&query).unwrap();
                statement.reset_status(StatementStatus::VmStep);
                statement.reset_status(StatementStatus::FullscanStep);
            }
            let candidates = person_asks_for_reconcile(&connection).unwrap();
            assert_eq!(
                candidates.iter().map(|claim| &claim.id).collect::<Vec<_>>(),
                [&record.id]
            );
            let statement = connection.prepare_cached(&query).unwrap();
            let vm = statement.get_status(StatementStatus::VmStep);
            assert!(
                statement.get_status(StatementStatus::FullscanStep) <= 8,
                "only the sparse open-step index may be walked: {plan}"
            );
            assert!(
                vm > 0 && vm < 500,
                "candidate reader VM budget: {vm}, {plan}"
            );
            drop(statement);
            drop(connection);
            let before = smallclaims::sqlite::work::total();
            assert!(!store.reconcile_person_asks().unwrap());
            let cost = smallclaims::sqlite::work::total() - before;
            assert!(
                cost.statements <= 100 && cost.vm_steps <= 10_000,
                "full native reconciliation must not re-read every settled ask: {cost:?}"
            );
            costs.push((vm, cost));
        }
    }
    assert!(
        costs[1].0 <= costs[0].0 * 2
            && costs[1].1.statements <= costs[0].1.statements + 2
            && costs[1].1.vm_steps <= costs[0].1.vm_steps * 2,
        "settled-history growth must not grow current-ask work: {costs:?}"
    );
}

#[test]
fn person_ask_selection_keeps_native_stale_requester_cancellation() {
    let (store, _, input) = super::tests::fixture();
    let ask = store.ask_person(&input).unwrap();
    let stop =
        crate::graph::parse_internal_intent("version 2\nstop \"agent/alder.asker\"", "alder")
            .unwrap();
    store.apply_internal(&stop, "retire-requester").unwrap();
    assert_eq!(
        step(&store.readers.get(), &ask.subject)
            .unwrap()
            .unwrap()
            .status,
        "ready"
    );
    assert!(store.reconcile_person_asks().unwrap());
    assert_eq!(
        step(&store.readers.get(), &ask.subject)
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert_eq!(
        store
            .claims_for(&ask.subject, Some("work.person-cancelled"))
            .unwrap()
            .len(),
        1
    );
    assert!(!store.reconcile_person_asks().unwrap());
}

#[test]
fn duplicate_retained_person_ask_candidates_recheck_status_after_first_cancellation() {
    let (store, _, input) = super::tests::fixture();
    let ask = store.ask_person(&input).unwrap();
    let record = request(&store.readers.get(), &ask.subject)
        .unwrap()
        .unwrap();
    {
        let connection = store.connection.write();
        insert_retained_ask(
            &connection,
            "later-retained-ask",
            &ask.subject,
            &record,
            record.accepted_at_unix_ms + 1,
        );
    }
    let stop =
        crate::graph::parse_internal_intent("version 2\nstop \"agent/alder.asker\"", "alder")
            .unwrap();
    store.apply_internal(&stop, "retire-requester").unwrap();
    assert_eq!(
        person_asks_for_reconcile(&store.readers.get())
            .unwrap()
            .len(),
        2
    );
    assert!(store.reconcile_person_asks().unwrap());
    assert_eq!(
        store
            .claims_for(&ask.subject, Some("work.person-cancelled"))
            .unwrap()
            .len(),
        1,
        "materialized candidates must not repeat a cancellation after its status changed"
    );
}

#[test]
fn settled_person_ask_reconciliation_does_not_wait_for_the_writer() {
    let (store, _, input) = super::tests::fixture();
    let ask = store.ask_person(&input).unwrap();
    store
        .finish_person_step(
            &PersonStepResponse {
                delegation: None,
                subject: ask.subject,
                actor: "person/avery".into(),
                summary: "Friday".into(),
                evidence: vec![],
                episode: None,
                idempotency_key: "answer".into(),
                answer: None,
            },
            false,
        )
        .unwrap();
    let (held, is_held) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let (done, result) = std::sync::mpsc::sync_channel(1);
    let completed = std::thread::scope(|scope| {
        let store = &store;
        let writer = scope.spawn(move || {
            store.hold_writer_for_test(|| {
                held.send(()).unwrap();
                released.recv().unwrap();
            })
        });
        is_held.recv().unwrap();
        let reconcile = scope.spawn(move || done.send(store.reconcile_person_asks()).unwrap());
        let completed = result.recv_timeout(std::time::Duration::from_secs(2));
        release.send(()).unwrap();
        writer.join().unwrap();
        reconcile.join().unwrap();
        completed
    });
    assert!(
        !completed
            .expect("settled-only asks submitted an unnecessary writer job")
            .unwrap()
    );
}
