//! What a work renewal holds the single writer for, and that it answers what it always has.
use super::*;

struct Renewal {
    _directory: tempfile::TempDir,
    store: Store,
    subject: String,
}

fn request(key: &str, summary: Option<&str>) -> WorkRequest {
    WorkRequest {
        actor: Some("agent/node.worker".into()),
        incarnation: Some("current".into()),
        summary: summary.map(Into::into),
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    }
}

/// One claimed step whose claim history holds `history` progress reports.
fn claimed_step(history: usize) -> Renewal {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("renew.sqlite3"), "node").unwrap();
    let source = "version 2\nmission \"renewal\" state=\"ready\" { goal \"Hold work.\"; step \"work\" { assigned-to \"agent/node.worker\" } }";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &plan.subject_tokens, "renewal-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "renewal".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "renewal-run".into(),
        })
        .unwrap();
    let subject = run.steps[0].subject.clone();
    store.set_step_state(&subject, "ready", None).unwrap();
    store
        .work_action(&subject, "claim", &request("renewal-claim", None))
        .unwrap();
    for index in 0..history {
        // A report a millisecond: claims accepted in the same millisecond are sorted together.
        std::thread::sleep(std::time::Duration::from_millis(1));
        store
            .work_action(
                &subject,
                "progress",
                &request(
                    &format!("renewal-progress-{index}"),
                    Some(&format!("report {index}")),
                ),
            )
            .unwrap();
    }
    Renewal {
        _directory: directory,
        store,
        subject,
    }
}

fn writer_work(key: &str) -> smallclaims::sqlite::work::SqliteWork {
    WORK_ACTION_WRITER_WORK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .copied()
        .unwrap_or_else(|| panic!("the work action `{key}` ran no writer job"))
}

#[test]
fn a_renewal_holds_the_writer_for_a_fixed_number_of_statements_whatever_the_claim_history() {
    let mut costs = Vec::new();
    for history in [0, 200] {
        let renewal = claimed_step(history);
        // Most renewals only extend the local lease; one whose replicated expiry is near also
        // publishes a claim. A summary makes this one publish.
        let quiet = format!("renew-quiet-{history}");
        let published = format!("renew-published-{history}");
        renewal
            .store
            .work_action(&renewal.subject, "renew", &request(&quiet, None))
            .unwrap();
        renewal
            .store
            .work_action(
                &renewal.subject,
                "renew",
                &request(&published, Some("still here")),
            )
            .unwrap();
        let (quiet, published) = (writer_work(&quiet), writer_work(&published));
        eprintln!(
            "renew writer history={history} quiet: statements={} vm_steps={} fullscan_steps={} sorts={}; published: statements={} vm_steps={} fullscan_steps={} sorts={}",
            quiet.statements,
            quiet.vm_steps,
            quiet.fullscan_steps,
            quiet.sorts,
            published.statements,
            published.vm_steps,
            published.fullscan_steps,
            published.sorts,
        );
        costs.push((quiet, published));
    }
    let (short, long) = (costs[0], costs[1]);
    // Every statement the job runs, counted once: the step, its owner, the lease updates, the
    // idempotency receipt; a published renewal adds its claim append.
    assert!(
        short.0.statements <= 13,
        "a quiet renewal grew past its 13-statement writer budget: {}",
        short.0.statements
    );
    assert!(
        short.1.statements <= 25,
        "a published renewal grew past its 25-statement writer budget: {}",
        short.1.statements
    );
    assert_eq!(
        long.0.statements, short.0.statements,
        "a quiet renewal's writer statements grew with the claim history"
    );
    assert_eq!(
        long.1.statements, short.1.statements,
        "a published renewal's writer statements grew with the claim history"
    );
    for (name, short, long) in [("quiet", short.0, long.0), ("published", short.1, long.1)] {
        assert!(
            long.vm_steps <= short.vm_steps + short.vm_steps / 4,
            "a {name} renewal's writer work grew with the claim history: {} to {} VM steps",
            short.vm_steps,
            long.vm_steps,
        );
    }
}

/// What the writer answered for a work action before a renewal's view moved to a reader: the
/// committed step row enriched inside a writer job.
fn view_enriched_in_writer(store: &Store, subject: &str) -> StepRunView {
    store
        .connection
        .batched(|transaction| -> rusqlite::Result<StepRunView> {
            let mut view = transaction.query_row(
                "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                        lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                 FROM step_runs WHERE subject=?1",
                [subject],
                step_run_from_row,
            )?;
            enrich_step_queue(transaction, &mut view)?;
            Ok(view)
        })
        .unwrap()
        .unwrap()
}

/// A view without the one field that moves with the clock while a claim is active.
fn timeless(view: &StepRunView) -> Value {
    let mut value = serde_json::to_value(view).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("execution_elapsed_ms");
    value
}

#[test]
fn a_renewal_answers_what_the_writer_answered_when_it_enriched_the_view() {
    let renewal = claimed_step(3);
    let (store, subject) = (&renewal.store, renewal.subject.as_str());
    store
        .work_action_extending(
            subject,
            "extend",
            &WorkRequest {
                reason: Some("more to do".into()),
                ..request("renewal-extend", None)
            },
            Some(60_000),
        )
        .unwrap();
    for (key, summary) in [
        ("parity-quiet", None),
        ("parity-published", Some("still here")),
    ] {
        let before = view_enriched_in_writer(store, subject);
        let renewed = store
            .work_action(subject, "renew", &request(key, summary))
            .unwrap();
        let after = view_enriched_in_writer(store, subject);
        assert_eq!(timeless(&renewed), timeless(&after), "{key}");
        assert!(
            (before.execution_elapsed_ms..=after.execution_elapsed_ms)
                .contains(&renewed.execution_elapsed_ms),
            "{key}: elapsed {} outside {}..={}",
            renewed.execution_elapsed_ms,
            before.execution_elapsed_ms,
            after.execution_elapsed_ms,
        );
        assert!(renewed.claim_expires_at_unix_ms > before.claim_expires_at_unix_ms);
        assert!(renewed.execution_started_at_unix_ms.is_some(), "{key}");
        assert_eq!(renewed.timeout_extension_ms, 60_000, "{key}");
        assert_eq!(
            renewed.progress_summary.as_deref(),
            Some("report 2"),
            "{key}"
        );

        // An exact retry answers the committed lease again, with its view filled the same way.
        let replay = store
            .work_action(subject, "renew", &request(key, summary))
            .unwrap();
        assert_eq!(
            timeless(&replay),
            timeless(&view_enriched_in_writer(store, subject)),
            "{key} retry"
        );
        assert_eq!(
            replay.claim_expires_at_unix_ms,
            renewed.claim_expires_at_unix_ms
        );
    }
    assert!(
        store
            .work_action(
                subject,
                "renew",
                &WorkRequest {
                    incarnation: Some("old".into()),
                    ..request("parity-stale", None)
                },
            )
            .is_err(),
        "a renewal from another incarnation is still refused"
    );
}

#[test]
fn the_last_replicated_lease_is_what_sorting_every_lease_claim_found() {
    let renewal = claimed_step(0);
    let (store, subject) = (&renewal.store, renewal.subject.as_str());
    let every_lease_claim = canonical_sql(
        "SELECT json_extract(body, '$.fields.claim_expires_at_unix_ms')
         FROM claims WHERE subject=?1
           AND kind IN ('work.claimed','work.renewed','work.progress')
         ORDER BY CANONICAL_DESC(claims) LIMIT 1",
    );
    let read = |query: &str| -> Option<u64> {
        store
            .readers
            .get()
            .query_row(query, [subject], |row| row.get(0))
            .unwrap()
    };
    // Reports and published renewals interleave, many in the same millisecond, some apart.
    for index in 0..40 {
        let key = format!("lease-order-{index}");
        match index % 3 {
            0 => store
                .work_action(subject, "progress", &request(&key, Some("report")))
                .map(drop),
            1 => store
                .work_action(subject, "renew", &request(&key, Some("published")))
                .map(drop),
            _ => store
                .work_action(subject, "renew", &request(&key, None))
                .map(drop),
        }
        .unwrap();
        if index % 7 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            read(&last_replicated_lease_query()),
            read(&every_lease_claim),
            "after action {index}"
        );
    }
    assert!(read(&last_replicated_lease_query()).is_some());
}


#[test]
fn the_last_replicated_lease_seeks_each_kinds_newest_millisecond() {
    let renewal = claimed_step(0);
    let connection = renewal.store.readers.get();
    let mut statement = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            last_replicated_lease_query()
        ))
        .unwrap();
    let plan = statement
        .query_map([&renewal.subject], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter().any(|detail| detail
            == "SEARCH claims USING INDEX claims_subject_kind_accepted_index (subject=? AND kind=? AND <expr>=? AND accepted_at_unix_ms=?)"),
        "the candidates are not read by their exact millisecond: {plan:#?}"
    );
    assert!(
        !plan.iter().any(|detail| detail.starts_with("SCAN claims")),
        "{plan:#?}"
    );
}
