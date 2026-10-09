use super::*;

/// A work acknowledgement needs the selected seat's hierarchy, not the fleet's desired graph.
pub(super) fn enrich_under(store: &Store, response: &mut StepRunView) -> Result<(), St3Error> {
    let Some(assignee) = response
        .claimant
        .as_ref()
        .or(response.assigned_to.as_ref())
        .or_else(|| (response.available_to.len() == 1).then(|| &response.available_to[0]))
    else {
        return Ok(());
    };
    let _span = crate::profile::span("work/response-under");
    response.under = store
        .desired_subjects_named(std::slice::from_ref(assignee))
        .map_err(|error| {
            St3Error::new("store-read-failed", format!("read desired agent: {error}"))
        })?
        .into_iter()
        .find(|subject| subject.kind == "agent")
        .map(|subject| crate::graph::agent_under(&subject.desired))
        .unwrap_or_default();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn response() -> StepRunView {
        serde_json::from_value(json!({
            "subject":"step-run/run/work", "run":"mission-run/run",
            "generation":"run-generation/run", "step":"work", "definition_hash":"definition",
            "status":"claimed", "attempt":1, "assigned_to":"agent/node.worker",
            "agentless":false, "title":null, "worker_reported":false,
            "claimant":null, "claim_incarnation":null, "claim_expires_at_unix_ms":null,
            "readiness_epoch":1, "blocked_reason":null, "not_before_unix_ms":null,
            "created_at_unix_ms":1, "updated_at_unix_ms":2
        }))
        .unwrap()
    }

    #[test]
    fn acknowledgement_hierarchy_has_a_constant_sql_budget_with_large_desired_graph() {
        let store = Store::open_memory("node").unwrap();
        let anchor = store
            .append_claim(&ClaimInput {
                subject: "resource/fixture".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), json!("custom.test.latency"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let desired = json!({"children":[{"name":"under", "arguments":["node.lead"],
            "properties":{"reason":"review"}}]})
        .to_string();
        store
            .connection
            .batched(|tx| -> anyhow::Result<()> {
                tx.execute(
                    "INSERT INTO desired(subject,kind,revision,claim_id,body)
                VALUES ('agent/node.worker','agent','fixture',?1,?2)",
                    rusqlite::params![anchor.id, desired],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let mut costs = vec![];
        for size in [0, 4096] {
            if size > 0 {
                store
                    .connection
                    .batched(|tx| -> anyhow::Result<()> {
                        for index in 0..size {
                            tx.execute(
                                "INSERT INTO desired(subject,kind,revision,claim_id,body)
                            VALUES (?1,'agent','fixture',?2,'{}')",
                                rusqlite::params![format!("agent/unrelated-{index}"), anchor.id],
                            )?;
                        }
                        Ok(())
                    })
                    .unwrap()
                    .unwrap();
            }
            let cost = store
                .read_snapshot(|_| {
                    let c = store.readers.get();
                    let steps = Arc::new(AtomicU64::new(0));
                    let counter = steps.clone();
                    c.progress_handler(
                        1,
                        Some(move || {
                            counter.fetch_add(1, Ordering::Relaxed);
                            false
                        }),
                    );
                    let mut row = response();
                    let result = enrich_under(&store, &mut row);
                    c.progress_handler(0, None::<fn() -> bool>);
                    result?;
                    assert_eq!(
                        serde_json::to_value(row.under)?,
                        json!([{"agent":"agent/node.lead","reason":"review"}])
                    );
                    Ok(steps.load(Ordering::Relaxed))
                })
                .unwrap();
            eprintln!("work response-under unrelated={size} vm_steps={cost}");
            assert!(
                cost < 200,
                "keyed hierarchy exceeds its 200-instruction CI budget: {cost}"
            );
            costs.push(cost);
        }
        assert!(
            costs[1] <= costs[0] + 16,
            "hierarchy cost grew with unrelated desired rows: {costs:?}"
        );
        let legacy_cost = store
            .read_snapshot(|_| {
                let c = store.readers.get();
                let steps = Arc::new(AtomicU64::new(0));
                let counter = steps.clone();
                c.progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                );
                let result = store.desired_subjects();
                c.progress_handler(0, None::<fn() -> bool>);
                assert_eq!(result?.len(), 4097);
                Ok(steps.load(Ordering::Relaxed))
            })
            .unwrap();
        eprintln!("work previous full-desired enrichment vm_steps={legacy_cost}");
        assert!(
            legacy_cost > 200,
            "the previous full desired read must fail the new budget"
        );
    }

    #[test]
    fn hierarchy_preserves_selection_fallbacks_and_ignores_unrelated_bad_bodies() {
        let store = Store::open_memory("node").unwrap();
        let anchor = store
            .append_claim(&ClaimInput {
                subject: "resource/fixture".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), json!("custom.test.latency"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        store.connection.batched(|tx| -> anyhow::Result<()> {
            for (subject,kind,body) in [
                ("agent/node.worker","agent",r#"{"children":[{"name":"under","arguments":["worker-lead"]}]}"#),
                ("agent/claimant","agent",r#"{"children":[{"name":"under","arguments":["claimant-lead"]}]}"#),
                ("agent/available","agent",r#"{"children":[{"name":"under","arguments":["available-lead"]}]}"#),
                ("resource/non-agent","resource",r#"{"children":[{"name":"under","arguments":["wrong"]}]}"#)
            ] {
                tx.execute("INSERT INTO desired(subject,kind,revision,claim_id,body) VALUES (?1,?2,'fixture',?3,?4)",
                    rusqlite::params![subject,kind,anchor.id,body])?;
            }
            Ok(())
        }).unwrap().unwrap();
        store
            .readers
            .request_read(|| -> anyhow::Result<()> {
                let connection = store.readers.get();
                // Native declaration-edge triggers parse body during INSERT. Corrupt only a
                // connection-local read fixture, keeping the native table and its guards intact.
                connection.execute_batch(
                    "PRAGMA query_only=OFF;
            CREATE TEMP TABLE desired AS SELECT * FROM main.desired;",
                )?;
                connection.execute(
                    "INSERT INTO temp.desired(subject,kind,revision,claim_id,body)
            VALUES ('agent/unrelated','agent','fixture',?1,'malformed')",
                    [&anchor.id],
                )?;
                connection.execute_batch("PRAGMA query_only=ON;")?;
                assert!(
                    connection.query_row("PRAGMA query_only", [], |row| row.get::<_, bool>(0))?
                );
                assert!(!connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM main.desired
            WHERE subject='agent/unrelated')",
                    [],
                    |row| row.get::<_, bool>(0)
                )?);
                assert_eq!(
                    connection.query_row(
                        "SELECT body FROM temp.desired
            WHERE subject='agent/unrelated'",
                        [],
                        |row| row.get::<_, String>(0)
                    )?,
                    "malformed"
                );
                let full = store.desired_subjects()?;
                assert_eq!(
                    full.iter()
                        .find(|subject| subject.subject == "agent/unrelated")
                        .expect("the full desired read must encounter the malformed fixture")
                        .desired,
                    Value::Null,
                    "malformed JSON retains the existing desired-reader null fallback"
                );
                for (claimant, assigned, available, expected) in [
                    (
                        Some("agent/claimant"),
                        Some("agent/node.worker"),
                        vec![],
                        Some("agent/claimant-lead"),
                    ),
                    (
                        None,
                        Some("agent/node.worker"),
                        vec![],
                        Some("agent/worker-lead"),
                    ),
                    (
                        None,
                        None,
                        vec!["agent/available"],
                        Some("agent/available-lead"),
                    ),
                    (
                        None,
                        None,
                        vec!["agent/available", "agent/node.worker"],
                        None,
                    ),
                    (None, Some("agent/missing"), vec![], None),
                    (None, Some("resource/non-agent"), vec![], None),
                ] {
                    let mut row = response();
                    row.claimant = claimant.map(str::to_owned);
                    row.assigned_to = assigned.map(str::to_owned);
                    row.available_to = available.into_iter().map(str::to_owned).collect();
                    enrich_under(&store, &mut row).unwrap();
                    assert_eq!(row.under.first().map(|u| u.agent.as_str()), expected);
                }
                let mut selected_bad = response();
                selected_bad.assigned_to = Some("agent/unrelated".into());
                enrich_under(&store, &mut selected_bad)?;
                assert!(
                    selected_bad.under.is_empty(),
                    "a selected malformed declaration must not invent a hierarchy"
                );
                Ok(())
            })
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn renewal_acknowledges_committed_lease_and_preserves_incarnation_and_retry_fences() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let source = "version 2\nmission \"ack-latency\" state=\"ready\" { goal \"Hold work.\"; step \"work\" { assigned-to \"agent/node.worker\" } }";
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let plan = state
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &plan.subject_tokens, "ack-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "ack-latency".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "ack-run".into(),
            })
            .unwrap();
        let subject = &run.steps[0].subject;
        state.store.set_step_state(subject, "ready", None).unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!("current")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let request = |key: &str, incarnation: &str| WorkRequest {
            actor: Some("agent/node.worker".into()),
            incarnation: Some(incarnation.into()),
            summary: None,
            reason: None,
            evidence: vec![],
            idempotency_key: key.into(),
        };
        state
            .store
            .work_action(subject, "claim", &request("ack-claim", "current"))
            .unwrap();
        let renewed = work_action_response(
            state.clone(),
            "renew".into(),
            subject.clone(),
            request("ack-renew", "current"),
            None,
            None,
        )
        .await
        .unwrap()
        .0;
        let committed = state.store.step_run(subject).unwrap().unwrap();
        assert_eq!(
            renewed.claim_expires_at_unix_ms,
            committed.claim_expires_at_unix_ms
        );
        assert_eq!(renewed.claim_incarnation, committed.claim_incarnation);
        assert_eq!(committed.claimant.as_deref(), Some("agent/node.worker"));
        assert!(
            work_action_response(
                state.clone(),
                "renew".into(),
                subject.clone(),
                request("ack-stale", "old"),
                None,
                None
            )
            .await
            .is_err()
        );
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("stopped")),
                    ("incarnation_id".into(), json!("current")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let replay = work_action_response(
            state.clone(),
            "renew".into(),
            subject.clone(),
            request("ack-renew", "current"),
            None,
            None,
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            replay.claim_expires_at_unix_ms,
            renewed.claim_expires_at_unix_ms
        );
        assert!(
            work_action_response(
                state,
                "renew".into(),
                subject.clone(),
                request("ack-after-stop", "current"),
                None,
                None
            )
            .await
            .is_err()
        );
    }
}
