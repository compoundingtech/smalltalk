//! A one-shot reviewer repair belongs to the existing request, not its published gate spec.
//! Readers fold immutable reassignment claims; the request's operation and episode stay intact.
use super::*;
use crate::model::PersonRenameRequest;

/// SQL for the reviewer assigned to this exact request. An answer uses the assignment at its
/// canonical moment, so later repairs cannot erase an answer already made by its reviewer.
/// Unary `+` removes the request ID's TEXT affinity so its JSON-expression index is seekable;
/// both IDs remain strings, as in the operation-conflict query.
pub(super) fn reviewer_sql(request: &str, answer: Option<&str>) -> String {
    let before_answer = answer
        .map(|answer| format!(" AND {}", canonical::after_sql(answer, "reassignment")))
        .unwrap_or_default();
    let accepted = person_rename::accepted_reassignment_sql(
        "reassignment",
        "previous_reviewer",
        "reviewer",
    );
    format!(
        "COALESCE((SELECT json_extract(reassignment.body, '$.fields.reviewer')
           FROM claims reassignment INDEXED BY claims_human_gate_reassignment_request_index
           WHERE reassignment.subject={request}.subject
             AND reassignment.kind='gate.reviewer-reassigned'
             AND json_extract(reassignment.body, '$.fields.request')=+{request}.id
             AND json_extract(reassignment.body, '$.fields.owner')=json_extract({request}.body, '$.fields.owner')
             AND {accepted}
             {before_answer}
           ORDER BY {} LIMIT 1), json_extract({request}.body, '$.fields.reviewer'))",
        canonical::order_sql("reassignment", true),
    )
}

pub(super) fn reviewer_tx(connection: &Connection, request: &ClaimRecord) -> Result<String> {
    smallclaims::touched::note_read(|| request.subject.clone());
    connection
        .query_row(
            &format!(
                "SELECT {} FROM claims request WHERE request.id=?1 AND request.kind='gate.requested'",
                reviewer_sql("request", None),
            ),
            [&request.id],
            |row| row.get(0),
        )
        .context("the human review request does not name a reviewer")
}

/// Restrict inbox hydration to the requested effective reviewer and unresolved, newest
/// episodes. Candidate routing includes repairs received before their immutable request.
/// Supersession uses the original reviewer, never the repaired reviewer: neither answering
/// nor moving the newest request may revive a superseded copy in OLD's inbox.
pub(super) fn pending_requests_sql(filtered: bool) -> String {
    let candidates = if filtered {
        "SELECT id FROM claims INDEXED BY claims_human_gate_request_index
         WHERE kind='gate.requested' AND json_extract(body, '$.fields.reviewer') IS NOT NULL
           AND json_extract(body, '$.fields.reviewer')=?1
         UNION
         SELECT json_extract(body, '$.fields.request')
         FROM claims INDEXED BY claims_human_gate_reassignment_reviewer_index
         WHERE kind='gate.reviewer-reassigned' AND json_extract(body, '$.fields.reviewer')=?1"
    } else {
        // A global attention read must start from live owners, not every historical request.
        // These are the owner forms accepted by human_review_currency; that function still
        // decides revision, generation, attempt and root-run currency after hydration.
        // Loop descendants include the legacy item and round/candidate owner forms.
        // CROSS JOIN keeps the small owner set outside the indexed claim seeks. Unary +
        // removes TEXT affinity from owner IDs, as it does for exact request IDs above.
        // Keep the parameter shape identical to the reviewer-filtered query.
        "WITH open_steps(subject) AS MATERIALIZED (
             SELECT step_runs.subject
             FROM mission_runs INDEXED BY mission_runs_open_index
             CROSS JOIN step_runs INDEXED BY step_runs_run_index
               ON step_runs.run_id=mission_runs.id
             WHERE mission_runs.status NOT IN ('completed','failed','cancelled')
               AND step_runs.status NOT IN ('completed','failed','cancelled')
         ), owners(owner) AS (
             SELECT 'mission-run/' || id FROM mission_runs INDEXED BY mission_runs_open_index
             WHERE status NOT IN ('completed','failed','cancelled')
             UNION ALL SELECT subject FROM open_steps
             UNION ALL SELECT 'loop-run/' || substr(subject,10) FROM open_steps
         )
         SELECT request.id
         FROM owners
         CROSS JOIN claims request INDEXED BY claims_human_gate_episode_index
           ON json_extract(request.body, '$.fields.owner')=+owners.owner
         WHERE request.kind='gate.requested'
           AND json_extract(request.body, '$.fields.reviewer') IS NOT NULL
           AND ?1 IS NULL
         UNION
         SELECT request.id
         FROM open_steps
         CROSS JOIN claims request INDEXED BY claims_human_gate_episode_index
           ON json_extract(request.body, '$.fields.owner')>='loop-run/' || substr(open_steps.subject,10) || '/'
          AND json_extract(request.body, '$.fields.owner')<'loop-run/' || substr(open_steps.subject,10) || '0'
         WHERE request.kind='gate.requested'
           AND json_extract(request.body, '$.fields.reviewer') IS NOT NULL
           AND ?1 IS NULL"
    };
    let episode = |alias: &str, request: &str| {
        ["owner", "reviewer", "attempt", "mission_revision", "step_definition"]
            .map(|field| {
                format!(
                    "json_extract({alias}.body, '$.fields.{field}')=json_extract({request}.body, '$.fields.{field}')"
                )
            })
            .join(" AND ")
    };
    let effective_reviewer = reviewer_sql("request", None);
    // Finish answer/supersession filtering before looking up an episode's first request.
    // Otherwise SQLite may run that canonical sort for every historical candidate, including
    // answered episodes that cannot contribute a review or its waiting-since timestamp.
    // Keep candidates outside hydration too: an ordinary JOIN lets SQLite scan the
    // partial reviewer index and probe an automatic candidates index for every historical
    // request, despite the bounded owner seeks inside candidates.
    canonical_sql(&format!(
        "WITH candidates(id) AS ({candidates}),
         unresolved AS MATERIALIZED (
             SELECT request.*
             FROM candidates
             CROSS JOIN claims request ON request.id=candidates.id
             WHERE request.kind='gate.requested'
               AND json_extract(request.body, '$.fields.reviewer') IS NOT NULL
               AND (?1 IS NULL OR {effective_reviewer}=?1)
               AND NOT EXISTS (
                   SELECT 1 FROM claims result INDEXED BY claims_human_gate_answer_request_index
                   WHERE {})
               AND NOT EXISTS (
                   SELECT 1 FROM claims newer INDEXED BY claims_human_gate_episode_index
                   WHERE newer.kind='gate.requested'
                     AND json_extract(newer.body, '$.fields.reviewer') IS NOT NULL
                     AND {} AND {})
         )
         SELECT pending.id, pending.store_index, pending.batch_id, pending.subject,
                pending.kind, pending.origin, pending.actor, pending.body,
                pending.predecessors, pending.accepted_at_unix_ms,
                first.accepted_at_unix_ms
         FROM unresolved pending
         JOIN claims first ON first.id=(
             SELECT earliest.id FROM claims earliest INDEXED BY claims_human_gate_episode_index
             WHERE earliest.kind='gate.requested'
               AND json_extract(earliest.body, '$.fields.reviewer') IS NOT NULL
               AND {}
             ORDER BY {} LIMIT 1)
         ORDER BY {}",
        reviewer_answer_sql("result"),
        episode("newer", "request"),
        canonical::after_sql("newer", "request"),
        episode("earliest", "pending"),
        canonical::order_sql("earliest", false),
        canonical::order_sql("first", false),
    ))
}

/// Append one reassignment per current, unanswered request under the rename transaction's
/// writer lock. The original request is both evidence and a causal predecessor; no gate is
/// re-asked, and no historical body or actor is rewritten.
pub(super) fn reassign_reviews_tx(
    tx: &Transaction<'_>,
    origin: &str,
    input: &PersonRenameRequest,
) -> Result<Vec<String>, St3Error> {
    let reviews = pending_human_reviews_tx(tx, Some(&input.old_person)).map_err(internal)?;
    let mut owners = Vec::with_capacity(reviews.len());
    for review in reviews {
        let request = claim_by_id_tx(tx, &review.request)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("missing-review-request", "the pending review request is missing"))?;
        let key = canonical_hash(&(
            "st3.person-review-reassignment.v1",
            &input.idempotency_key,
            &request.id,
        ))
        .map_err(internal)?;
        let claim_input = ClaimInput {
            subject: request.subject.clone(),
            kind: "gate.reviewer-reassigned".into(),
            actor: Some(input.actor.clone()),
            fields: BTreeMap::from([
                ("request".into(), json!(request.id)),
                ("owner".into(), json!(review.owner)),
                ("previous_reviewer".into(), json!(input.old_person)),
                ("reviewer".into(), json!(input.new_person)),
            ]),
            evidence: vec![request.id.clone()],
            expected_subject: None,
            idempotency_key: Some(format!("person-review-reassignment:{key}")),
        };
        let (operation_id, request_digest) = claim_operation(&claim_input)?
            .expect("a reviewer reassignment always has an idempotency key");
        if let Some((stored_digest, _, state)) = operation_tx(tx, &operation_id).map_err(internal)? {
            if state != "active" || stored_digest != request_digest {
                return Err(St3Error::new(
                    "idempotency-conflict",
                    "this review reassignment key already names a different repair",
                ));
            }
            continue;
        }
        checkpointed_operation_outcome(tx, &operation_id, &request_digest)?;
        let body = json!({
            "fields": claim_input.fields,
            "evidence": claim_input.evidence,
            "_operation": {"id": operation_id, "request_digest": request_digest},
        });
        let mut predecessors = latest_claim_id_tx(tx, &request.subject)
            .map_err(internal)?
            .into_iter()
            .collect::<Vec<_>>();
        if !predecessors.contains(&request.id) {
            predecessors.push(request.id);
        }
        let claim = append_claim_tx(
            tx,
            origin,
            &request.subject,
            &claim_input.kind,
            Some(&input.actor),
            &body,
            &predecessors,
            None,
        )
        .map_err(claim_append_error)?;
        register_operation_tx(tx, &claim).map_err(internal)?;
        owners.push(review.owner);
    }
    owners.sort();
    owners.dedup();
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "person/avery";
    const NEW: &str = "person/robin";
    const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

    fn fixture() -> (Store, MissionRunView) {
        let store = Store::open_memory("alder").unwrap();
        store.set_write_clock_at(now_ms() + 1_000).unwrap();
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
mission "review-repair" state="ready" {
  goal "Review the release.";
  step "pending" { goal "Review the release."; agentless }
  step "answered" { goal "Review an earlier change."; agentless }
  step "stale" { goal "Review current work only."; agentless }
}"#,
            "alder",
        )
        .unwrap();
        store.apply_internal(&intent, "review-repair-mission").unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "review-repair".into(),
                revision: None,
                workspace: ".".into(),
                requester: Some("person/operator".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "review-repair-run".into(),
            })
            .unwrap();
        (store, run)
    }

    fn request(
        store: &Store,
        run: &MissionRunView,
        path: &str,
        suffix: &str,
        attempt_offset: u32,
    ) -> ClaimRecord {
        let step = run.steps.iter().find(|step| step.step == path).unwrap();
        let operation = format!("gate-operation/review-repair/{suffix}");
        store
            .append_claim(&ClaimInput {
                subject: operation.clone(),
                kind: "gate.requested".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("owner".into(), json!(step.subject)),
                    ("reviewer".into(), json!(OLD)),
                    ("mode".into(), json!("approve")),
                    ("question".into(), json!("Approve this change?")),
                    ("review_targets".into(), json!([])),
                    ("decisions".into(), json!(["approved", "rejected"])),
                    ("operation".into(), json!(operation)),
                    ("mission_revision".into(), json!(run.revision)),
                    ("step_definition".into(), json!(step.definition_hash)),
                    ("attempt".into(), json!(step.attempt + attempt_offset)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("review-repair-request:{suffix}")),
            })
            .unwrap()
    }

    fn answer(store: &Store, request: &ClaimRecord, actor: &str, key: &str) -> ClaimRecord {
        store
            .append_claim(&ClaimInput {
                subject: request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some(actor.into()),
                fields: BTreeMap::from([
                    ("request".into(), json!(request.id)),
                    ("verdict".into(), json!("pass")),
                    ("decision".into(), json!("approved")),
                ]),
                evidence: vec![request.id.clone()],
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap()
    }

    fn repair(store: &Store, old: &str, new: &str, key: &str) -> Vec<String> {
        store
            .connection
            .batched(|tx| {
                reassign_reviews_tx(
                    tx,
                    &store.origin,
                    &PersonRenameRequest {
                        old_person: old.into(),
                        new_person: new.into(),
                        actor: "person/operator".into(),
                        idempotency_key: key.into(),
                    },
                )
            })
            .unwrap()
            .unwrap()
    }

    fn receive(source: &Store, target: &Store) {
        let exchange = source
            .export_replication_exchange_answering(
                FLEET,
                &target.replication_inventory().unwrap(),
                &[],
            )
            .unwrap();
        target
            .receive_replication_exchange(&exchange.peer, FLEET, &exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.project_replication_backlog().unwrap();
    }

    #[test]
    fn a_pending_review_moves_without_reasking_or_reopening_old_copies() {
        let (store, run) = fixture();
        let first = request(&store, &run, "pending", "first", 0);
        let current = request(&store, &run, "pending", "current", 0);
        let answered = request(&store, &run, "answered", "answered", 0);
        let prior_answer = answer(&store, &answered, OLD, "answered-before-repair");
        let stale = request(&store, &run, "stale", "stale", 1);
        // The fixture pins claim time ahead of the wall clock; use that same snapshot time.
        let as_of = current.accepted_at_unix_ms;
        let before = store.attention_snapshot(Some(OLD), as_of).unwrap();
        let before = before.iter().find(|item| item.kind == "human-gate").unwrap();
        assert_eq!(before.episode, current.id);

        assert_eq!(
            repair(&store, OLD, NEW, "rename-old-new"),
            vec![current.body["fields"]["owner"].as_str().unwrap().to_owned()],
        );
        assert!(store.pending_human_reviews(Some(OLD)).unwrap().is_empty());
        assert!(store.attention_snapshot(Some(OLD), as_of).unwrap().is_empty());
        let reviews = store.pending_human_reviews(Some(NEW)).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].request, current.id);
        assert_eq!(reviews[0].operation, current.subject);
        assert_eq!(reviews[0].requested_at_unix_ms, first.accepted_at_unix_ms);
        let after = store.attention_snapshot(Some(NEW), as_of).unwrap();
        let after = after.iter().find(|item| item.kind == "human-gate").unwrap();
        assert_eq!(after.episode, before.episode);
        assert_eq!(after.subject, before.subject);
        assert_eq!(after.person, NEW);
        assert_eq!(store.human_review_reviewer(&current).unwrap(), NEW);
        assert!(store.human_gate_requests().unwrap().iter().any(|(id, _, reviewer)| {
            id == &current.id && reviewer == NEW
        }));
        assert_eq!(
            store.human_review_refusal(&reviews[0].owner).unwrap(),
            format!("its review waits on {NEW}"),
        );
        assert_eq!(store.human_review_reviewer(&answered).unwrap(), OLD);
        assert_eq!(store.human_review_reviewer(&stale).unwrap(), OLD);
        assert_eq!(
            store.human_review_answer(&answered.id).unwrap().unwrap().id,
            prior_answer.id,
        );
        for original in [&first, &current, &answered, &stale] {
            let stored = store.claim_by_id(&original.id).unwrap().unwrap();
            assert_eq!(stored.body, original.body);
            assert_eq!(stored.actor, original.actor);
        }
        let repairs = store
            .claims_for(&current.subject, None)
            .unwrap()
            .into_iter()
            .filter(|claim| claim.kind == "gate.reviewer-reassigned")
            .collect::<Vec<_>>();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].actor.as_deref(), Some("person/operator"));
        assert_eq!(repairs[0].body["fields"]["request"], current.id);
        assert!(repairs[0].predecessors.contains(&current.id));
        assert_eq!(repairs[0].body["evidence"], json!([current.id]));
        assert!(repair(&store, OLD, NEW, "rename-old-new").is_empty());
        assert!(repair(&store, OLD, NEW, "another-rename-old-new").is_empty());

        answer(&store, &current, OLD, "wrong-reviewer-after-repair");
        assert!(store.human_review_answer(&current.id).unwrap().is_none());
        assert_eq!(store.pending_human_reviews(Some(NEW)).unwrap()[0].request, current.id);
        let verdict = answer(&store, &current, NEW, "new-reviewer-answer");
        assert_eq!(
            store.human_review_answer(&current.id).unwrap().unwrap().id,
            verdict.id,
        );
        assert!(store.pending_human_reviews(None).unwrap().is_empty());
        assert!(store.attention_snapshot(Some(OLD), as_of).unwrap().is_empty());
        assert!(store.attention_snapshot(Some(NEW), as_of).unwrap().is_empty());
        assert_eq!(
            store.human_review_refusal(&reviews[0].owner).unwrap(),
            format!("it was already answered: approved by {NEW}"),
        );
        assert!(repair(&store, NEW, "person/blair", "answered-is-immutable").is_empty());
        assert_eq!(
            store.claims_for(&current.subject, None).unwrap().iter()
                .filter(|claim| claim.kind == "gate.reviewer-reassigned").count(),
            1,
        );
    }

    #[test]
    fn canonical_latest_reassignment_survives_replication_replay_and_reopen() {
        let (source, run) = fixture();
        let request = request(&source, &run, "pending", "replicated", 0);
        // All writes share one timestamp; replica sequence and position break the ties.
        repair(&source, OLD, NEW, "first-rename");
        repair(&source, NEW, "person/blair", "second-rename");
        assert_eq!(source.human_review_reviewer(&request).unwrap(), "person/blair");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reviews.sqlite3");
        let target = Store::open(&path, "birch").unwrap();
        target.project_replication_backlog().unwrap();
        receive(&source, &target);
        for store in [&source, &target] {
            assert!(store.pending_human_reviews(Some(OLD)).unwrap().is_empty());
            assert!(store.pending_human_reviews(Some(NEW)).unwrap().is_empty());
            assert_eq!(
                store.pending_human_reviews(Some("person/blair")).unwrap()[0].request,
                request.id,
            );
            assert_eq!(store.human_review_reviewer(&request).unwrap(), "person/blair");
            store.replay_replication_graph().unwrap();
            assert_eq!(store.human_review_reviewer(&request).unwrap(), "person/blair");
        }
        answer(&source, &request, NEW, "superseded-reviewer-verdict");
        assert!(source.human_review_answer(&request.id).unwrap().is_none());
        let verdict = answer(&source, &request, "person/blair", "third-reviewer-verdict");
        receive(&source, &target);
        target.replay_replication_graph().unwrap();
        drop(target);
        let reopened = Store::open(&path, "birch").unwrap();
        assert!(reopened.pending_human_reviews(None).unwrap().is_empty());
        assert_eq!(
            reopened.human_review_answer(&request.id).unwrap().unwrap().id,
            verdict.id,
        );
        assert_eq!(reopened.claim_by_id(&request.id).unwrap().unwrap().body, request.body);
        assert_eq!(reopened.claim_by_id(&request.id).unwrap().unwrap().actor, request.actor);
        assert!(repair(&reopened, OLD, NEW, "first-rename").is_empty());
    }

    #[test]
    fn a_replica_answer_before_reassignment_remains_an_immutable_answer() {
        let (source, run) = fixture();
        let request = request(&source, &run, "pending", "concurrent", 0);
        let target = Store::open_memory("birch").unwrap();
        target.project_replication_backlog().unwrap();
        receive(&source, &target);
        target.set_write_clock_at(request.accepted_at_unix_ms + 1).unwrap();
        let prior_answer = answer(&target, &request, OLD, "earlier-replica-answer");
        source.set_write_clock_at(request.accepted_at_unix_ms + 2).unwrap();
        repair(&source, OLD, NEW, "later-replica-rename");
        receive(&source, &target);
        receive(&target, &source);
        for store in [&source, &target] {
            assert_eq!(store.human_review_reviewer(&request).unwrap(), NEW);
            assert_eq!(
                store.human_review_answer(&request.id).unwrap().unwrap().id,
                prior_answer.id,
            );
            assert!(store.pending_human_reviews(None).unwrap().is_empty());
            store.replay_replication_graph().unwrap();
            assert_eq!(
                store.human_review_answer(&request.id).unwrap().unwrap().actor.as_deref(),
                Some(OLD),
            );
        }
    }

    #[test]
    fn indexed_candidates_exclude_answered_and_unrelated_history_before_hydration() {
        let (store, run) = fixture();
        let pending = request(&store, &run, "pending", "indexed-pending", 0);
        // Separate owners keep this a history/routing test, not a supersession shortcut.
        for index in 0..512 {
            for answered in [false, true] {
                let mut fields: BTreeMap<String, Value> =
                    serde_json::from_value(pending.body["fields"].clone()).unwrap();
                fields.insert("owner".into(), json!(format!("step-run/history/{answered}/{index}")));
                fields.insert(
                    "reviewer".into(),
                    json!(if answered { OLD } else { "person/intruder" }),
                );
                let historical = store.append_claim(&ClaimInput {
                    subject: format!("gate-operation/history/{answered}/{index}"),
                    kind: "gate.requested".into(),
                    actor: Some("daemon/runtime".into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                }).unwrap();
                if answered {
                    answer(&store, &historical, OLD, &format!("history-answer-{index}"));
                }
            }
        }
        let selected = |reviewer: &str| {
            let connection = store.readers.get();
            let mut statement = connection.prepare(&pending_requests_sql(true)).unwrap();
            statement.query_map([reviewer], |row| row.get::<_, String>(0)).unwrap()
                .collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(selected(OLD), vec![pending.id.clone()]);
        assert!(selected(NEW).is_empty());
        repair(&store, OLD, NEW, "indexed-rename");
        assert!(selected(OLD).is_empty());
        assert_eq!(selected(NEW), vec![pending.id.clone()]);
        assert_eq!(store.pending_human_reviews(Some(NEW)).unwrap()[0].request, pending.id);

        let connection = store.readers.get();
        let plan = connection.prepare(&format!("EXPLAIN QUERY PLAN {}", pending_requests_sql(true)))
            .unwrap().query_map([NEW], |row| row.get::<_, String>(3)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        for index in [
            "claims_human_gate_request_index",
            "claims_human_gate_reassignment_reviewer_index",
            "claims_human_gate_reassignment_request_index",
            "claims_human_gate_answer_request_index",
            "claims_human_gate_episode_index",
        ] {
            assert!(plan.iter().any(|step| step.contains("SEARCH") && step.contains(index)),
                "{index}: {plan:?}");
        }
        assert!(!plan.iter().any(|step|
            step.starts_with("SCAN request") || step.starts_with("SCAN result")
                || step.starts_with("SCAN reassignment") || step.starts_with("SCAN newer")),
            "{plan:?}");
    }

    #[test]
    fn answered_episodes_are_filtered_before_first_request_sorting() {
        let (store, run) = fixture();
        let pending = request(&store, &run, "pending", "sort-pending", 0);
        let measure = || {
            let connection = store.readers.get();
            let mut statement = connection.prepare(&pending_requests_sql(false)).unwrap();
            let ids = statement.query_map([None::<&str>], |row| row.get::<_, String>(0))
                .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(ids, vec![pending.id.clone()]);
            statement.get_status(rusqlite::StatementStatus::Sort)
        };
        let before = measure();
        for index in 0..64 {
            let historical = request(
                &store, &run, "answered", &format!("sort-history-{index}"), index,
            );
            answer(&store, &historical, OLD, &format!("sort-answer-{index}"));
        }
        let after = measure();
        // Each answer may sort its exact-request reassignment lookup, but an answered
        // episode must not also sort its earliest request merely to be discarded.
        assert!(after <= before + 64, "answered history added episode sorts: {before} -> {after}");

        let connection = store.readers.get();
        let plan = connection.prepare(&format!("EXPLAIN QUERY PLAN {}", pending_requests_sql(false)))
            .unwrap().query_map([None::<&str>], |row| row.get::<_, String>(3)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert!(plan.iter().any(|step| step == "MATERIALIZE unresolved"), "{plan:?}");
    }

    #[test]
    fn unfiltered_reviews_cover_owner_shapes_and_deduplicate_overlapping_paths() {
        let (store, completed) = fixture();
        let template = request(&store, &completed, "pending", "closed-template", 0);
        let append = |owner: &str, definition: &str, attempt: u32, suffix: &str| {
            let operation = format!("gate-operation/review-owner-shapes/{suffix}");
            let mut fields: BTreeMap<String, Value> =
                serde_json::from_value(template.body["fields"].clone()).unwrap();
            fields.insert("owner".into(), json!(owner));
            fields.insert("operation".into(), json!(operation));
            fields.insert("step_definition".into(), json!(definition));
            fields.insert("attempt".into(), json!(attempt));
            store.append_claim(&ClaimInput {
                subject: operation,
                kind: "gate.requested".into(),
                actor: Some("daemon/runtime".into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("review-owner-shapes:{suffix}")),
            }).unwrap()
        };
        let completed_step = &completed.steps[0];
        let completed_loop_owner = format!(
            "loop-run/{}", completed_step.subject.strip_prefix("step-run/").unwrap(),
        );
        let closed_requests = [
            append(&completed.subject, &completed.revision, 1, "closed-mission"),
            append(
                &completed_step.subject, &completed_step.definition_hash,
                completed_step.attempt, "closed-step",
            ),
            append(
                &completed_loop_owner, &completed_step.definition_hash,
                completed_step.attempt, "closed-loop",
            ),
        ];
        store.set_mission_run_state(
            &completed.subject, "completed", "terminal", Some("owner-shape coverage"),
        ).unwrap();
        // Finish the historical run first: this mission permits only one active run.
        let run = store.create_mission_run(&MissionRunRequest {
            mission: "review-repair".into(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "review-owner-shapes-live".into(),
        }).unwrap();
        let step_request = request(&store, &run, "pending", "owner-step", 0);
        let parent = run.steps.iter().find(|step| step.step == "pending").unwrap();
        let nested = run.steps.iter().find(|step| step.step == "answered").unwrap();
        let nested_subject = format!("{}/nested", parent.subject);
        // The nested loop owner is reachable both by its exact-owner seek and by the
        // parent's descendant range. Keep both projected steps live to exercise UNION.
        store.connection.write().execute(
            "UPDATE step_runs SET subject=?1,step_path='pending/nested' WHERE subject=?2",
            params![nested_subject, nested.subject],
        ).unwrap();
        let mission_request = append(&run.subject, &run.revision, 1, "mission");
        let loop_step = run.steps.iter().find(|step| step.step == "stale").unwrap();
        let loop_owner = format!(
            "loop-run/{}/round/1/candidate/1",
            loop_step.subject.strip_prefix("step-run/").unwrap(),
        );
        let loop_request = append(
            &loop_owner, &loop_step.definition_hash, loop_step.attempt, "loop-descendant",
        );
        let overlap_owner = format!(
            "loop-run/{}", nested_subject.strip_prefix("step-run/").unwrap(),
        );
        let overlap_request = append(
            &overlap_owner, &nested.definition_hash, nested.attempt, "overlap",
        );
        let matching_owners: u64 = store.readers.get().query_row(
            "SELECT COUNT(*) FROM step_runs
             WHERE ?1='loop-run/' || substr(subject,10)
                OR (?1>='loop-run/' || substr(subject,10) || '/'
                    AND ?1<'loop-run/' || substr(subject,10) || '0')",
            [&overlap_owner], |row| row.get(0),
        ).unwrap();
        assert_eq!(matching_owners, 2, "the fixture must exercise overlapping owner seeks");


        let reviews = store.pending_human_reviews(None).unwrap();
        let expected = [
            step_request.id, mission_request.id, loop_request.id, overlap_request.id.clone(),
        ].into_iter().collect::<BTreeSet<_>>();
        let actual = reviews.iter().map(|review| review.request.clone()).collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(reviews.len(), expected.len(), "overlapping owners duplicated a review");
        assert_eq!(
            reviews.iter().filter(|review| review.request == overlap_request.id).count(), 1,
        );
        for closed in closed_requests {
            assert!(!actual.contains(&closed.id), "completed run leaked request {}", closed.id);
        }
    }

    #[test]
    fn unfiltered_candidates_do_not_scan_unrelated_review_history() {
        let (store, run) = fixture();
        let pending = request(&store, &run, "pending", "unfiltered-pending", 0);
        // Keep the episode-index boundary nonempty in both measurements. An empty
        // suffix lets SQLite's Next jump directly to Halt; one unrelated owner
        // exercises the terminating IdxGT before measuring growth by 1,024 rows.
        let mut baseline_fields: BTreeMap<String, Value> =
            serde_json::from_value(pending.body["fields"].clone()).unwrap();
        baseline_fields.insert("owner".into(), json!("step-run/unrelated/baseline"));
        baseline_fields.insert("operation".into(), json!("gate-operation/unfiltered-baseline"));
        let baseline = store.append_claim(&ClaimInput {
            subject: "gate-operation/unfiltered-baseline".into(),
            kind: "gate.requested".into(),
            actor: Some("daemon/runtime".into()),
            fields: baseline_fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();
        answer(&store, &baseline, OLD, "unfiltered-baseline-answer");
        let measure = || {
            let connection = store.readers.get();
            let mut statement = connection.prepare(&pending_requests_sql(false)).unwrap();
            let ids = statement.query_map([None::<&str>], |row| row.get::<_, String>(0))
                .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(ids, vec![pending.id.clone()]);
            (
                statement.get_status(rusqlite::StatementStatus::VmStep),
                statement.get_status(rusqlite::StatementStatus::FullscanStep),
            )
        };
        let before = measure();
        for index in 0..512 {
            for answered in [false, true] {
                let operation = format!("gate-operation/unfiltered-history/{answered}/{index}");
                let mut fields: BTreeMap<String, Value> =
                    serde_json::from_value(pending.body["fields"].clone()).unwrap();
                fields.insert("owner".into(), json!(format!("step-run/unrelated/{answered}/{index}")));
                fields.insert("operation".into(), json!(operation));
                let historical = store.append_claim(&ClaimInput {
                    subject: operation,
                    kind: "gate.requested".into(),
                    actor: Some("daemon/runtime".into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                }).unwrap();
                if answered {
                    answer(&store, &historical, OLD, &format!("unfiltered-history-answer-{index}"));
                }
            }
        }
        let after = measure();
        // Unrelated owners are not live work. Filtering them after a global request
        // scan can preserve the result and sort count while still growing linearly.
        assert!(after.0 <= before.0, "unrelated history added VM steps: {before:?} -> {after:?}");
        assert!(after.1 <= before.1, "unrelated history added full scans: {before:?} -> {after:?}");
    }

    #[test]
    fn indexed_routing_converges_when_reassignment_arrives_before_request() {
        let (source, run) = fixture();
        let target = Store::open_memory("birch").unwrap();
        target.project_replication_backlog().unwrap();
        receive(&source, &target);
        let request = request(&source, &run, "pending", "late-request", 0);
        target.set_write_clock_at(request.accepted_at_unix_ms + 1).unwrap();
        let reassignment = target.append_claim(&ClaimInput {
            subject: request.subject.clone(),
            kind: "gate.reviewer-reassigned".into(),
            actor: Some(OLD.into()),
            fields: BTreeMap::from([
                ("request".into(), json!(request.id)),
                ("owner".into(), request.body["fields"]["owner"].clone()),
                ("previous_reviewer".into(), json!(OLD)),
                ("reviewer".into(), json!(NEW)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();
        assert!(target.pending_human_reviews(Some(NEW)).unwrap().is_empty());
        receive(&source, &target);
        receive(&target, &source);
        for store in [&source, &target] {
            assert!(store.pending_human_reviews(Some(OLD)).unwrap().is_empty());
            assert_eq!(store.pending_human_reviews(Some(NEW)).unwrap()[0].request, request.id);
        }
        // The source's pinned clock has not advanced with the replicated repair. The answer
        // must follow that repair canonically to be an answer by the new reviewer.
        source.set_write_clock_at(reassignment.accepted_at_unix_ms + 1).unwrap();
        let verdict = answer(&source, &request, NEW, "late-request-answer");
        receive(&source, &target);
        for store in [&source, &target] {
            assert!(store.pending_human_reviews(None).unwrap().is_empty());
            assert_eq!(store.human_review_answer(&request.id).unwrap().unwrap().id, verdict.id);
        }
    }

    #[test]
    fn review_indexes_are_transactional_and_reinstalled_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review-indexes.sqlite3");
        let store = Store::open(&path, "alder").unwrap();
        let drop_indexes = "
            DROP INDEX claims_human_gate_reassignment_request_index;
            DROP INDEX claims_human_gate_reassignment_reviewer_index;
            DROP INDEX claims_human_gate_answer_request_index;
            DROP INDEX claims_human_gate_episode_index;";
        {
            let connection = store.connection.lock().unwrap();
            let tx = connection.unchecked_transaction().unwrap();
            tx.execute_batch(drop_indexes).unwrap();
            tx.rollback().unwrap();
            connection.prepare(&pending_requests_sql(true)).unwrap();
            // Simulate an existing store from before these additive local indexes.
            connection.execute_batch(drop_indexes).unwrap();
        }
        drop(store);
        let reopened = Store::open(&path, "alder").unwrap();
        assert!(reopened.pending_human_reviews(Some(NEW)).unwrap().is_empty());
        let connection = reopened.readers.get();
        let indexes: u64 = connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name IN (
                'claims_human_gate_reassignment_request_index',
                'claims_human_gate_reassignment_reviewer_index',
                'claims_human_gate_answer_request_index',
                'claims_human_gate_episode_index')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(indexes, 4);
    }
}
