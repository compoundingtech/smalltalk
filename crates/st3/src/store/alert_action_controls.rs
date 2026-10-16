//! Invented real-store controls; no live person policy or harness is touched.
use super::*;
use crate::model::{DelegationPolicyRequest, DelegationProof};

const SEAT: &str = "agent/alder.worker";
const ACTOR: &str = "agent/alder.helper";

#[test]
fn history_candidate_limits_and_wide_cell_refusal_do_not_write() {
    let (store, denial, proof) = denial_fixture(&["close-item"]);
    store
        .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap();
    for number in 0..40 {
        append(
            &store,
            SEAT,
            "harness.diagnostic",
            SEAT,
            json!({"code":"history-fixture","reason":format!("candidate {number}"),"severity":"warning"}),
            &format!("history-candidate-{number}"),
        );
    }
    let before = store.index().unwrap();
    let rows = store
        .read_snapshot(|cut| store.attention_history_candidates(cut, i64::MAX as u64, 200, None))
        .unwrap();
    assert_eq!(
        rows.len(),
        33,
        "32 candidates and one lookahead, before filtering"
    );
    assert_eq!(store.index().unwrap(), before);
    assert!(
        rows.iter()
            .all(|row| row.resolution.is_none() && !row.omitted),
        "known noise never consumes a resource decode or omission"
    );
    let noise = store
        .claims_for(SEAT, Some("harness.diagnostic"))
        .unwrap()
        .into_iter()
        .rev()
        .filter(|row| row.body["fields"]["code"] == "history-fixture")
        .take(3)
        .collect::<Vec<_>>();
    let large_noise =
        json!({"fields":{"code":"history-fixture", "reason":"n".repeat(900_000)}}).to_string();
    store
        .connection
        .batched(|tx| -> Result<()> {
            for row in &noise {
                tx.execute(
                    "UPDATE claims SET body=?1 WHERE id=?2",
                    params![large_noise, row.id],
                )?;
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
    let rows = store
        .read_snapshot(|cut| store.attention_history_candidates(cut, i64::MAX as u64, 32, None))
        .unwrap();
    assert!(
        rows.iter()
            .all(|row| !row.omitted && row.resolution.is_none()),
        "three known noise bodies exceed 2MiB but do not use decode budget"
    );
    store
        .connection
        .batched(|tx| -> Result<()> {
            for row in &noise {
                tx.execute(
                    "UPDATE claims SET body=?1 WHERE id=?2",
                    params![serde_json::to_string(&row.body)?, row.id],
                )?;
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
    let latest = store
        .latest_claim(SEAT, Some("harness.diagnostic"))
        .unwrap()
        .unwrap();
    // Authored wide-cell fixture, not a valid signed replacement: the read must
    // omit it before retrieving/decoding its unbounded body; keep the index and warning.
    let wide = json!({"fields":{"reason":"x".repeat(1_048_577)}}).to_string();
    store
        .connection
        .batched(|tx| -> Result<()> {
            tx.execute(
                "UPDATE claims SET body=?1 WHERE id=?2",
                params![wide, latest.id],
            )?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    let before = store.index().unwrap();
    let rows = store
        .read_snapshot(|cut| store.attention_history_candidates(cut, i64::MAX as u64, 1, None))
        .unwrap();
    assert!(rows[0].omitted);
    assert!(rows[0].resolution.is_none());
    assert_eq!(rows[0].store_index, latest.store_index);
    assert!(
        !rows[1].omitted && rows[1].resolution.is_none(),
        "a known noise row neither decodes nor reports a missing resource"
    );
    let older = store
        .read_snapshot(|cut| store.attention_history_candidates(cut, rows[0].store_index, 1, None))
        .unwrap();
    assert!(older.iter().all(|row| row.store_index < latest.store_index));
    assert_eq!(store.index().unwrap(), before);
    store
        .connection
        .batched(|tx| -> Result<()> {
            tx.execute(
                "UPDATE claims SET body=?1 WHERE id=?2",
                params![serde_json::to_string(&latest.body)?, latest.id],
            )?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .read_snapshot(|cut| store.attention_history_candidates(cut, i64::MAX as u64, 1, None))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn history_query_plan_bounds_each_kind_scan_before_sorting_a_large_fixture() {
    fn episode_seek(detail: &str) -> bool {
        matches!(detail,
            "SEARCH e USING INDEX sqlite_autoindex_claims_1 (id=?) LEFT-JOIN"
            | "SEARCH e USING INDEX sqlite_autoindex_claims_1 (id=? AND rowid<?) LEFT-JOIN"
            | "SEARCH e USING INDEX sqlite_autoindex_claims_1 (id=? AND store_index<?) LEFT-JOIN")
    }
    // The existing cut has been in the episode join since e1236bec. SQLite can
    // expose that extra range on the same index; scans and non-equality seeks
    // must still fail this assertion rather than being accepted by a substring.
    for refused in [
        "SCAN claims",
        "SCAN e",
        "SEARCH e USING INDEX another_index (id=?) LEFT-JOIN",
        "SEARCH e USING INDEX sqlite_autoindex_claims_1 (rowid<?) LEFT-JOIN",
        "SEARCH e USING INDEX sqlite_autoindex_claims_1 (id>?) LEFT-JOIN",
    ] {
        assert!(!episode_seek(refused), "unexpected accepted plan: {refused}");
    }
    let (store, denial, _) = denial_fixture(&["close-item"]);
    // Authored claims-table fixture only, not signed/native evidence. One transaction
    // creates a busy kind without turning the control into 10,000 source actions.
    store.connection.batched(|tx| -> Result<()> {
        tx.execute(r#"WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000)
            INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
            SELECT 'history-plan-'||i,?1,?2,'harness.diagnostic','alder',?2,
            '{"fields":{"code":"history-fixture"}}','[]','1' FROM n"#,
            params![denial.batch_id, SEAT])?;
        Ok(())
    }).unwrap().unwrap();
    store
        .read_snapshot(|cut| {
            let plan = attention_history::candidate_query_plan(&store, cut)?;
            assert_eq!(
                plan.iter()
                    .filter(|(_, _, detail)| detail
                        .contains("SEARCH claims USING COVERING INDEX claims_kind_index"))
                    .count(),
                2,
                "{plan:?}"
            );
            assert!(
                plan.iter()
                    .all(|(_, _, detail)| !detail.starts_with("SCAN claims")),
                "{plan:?}"
            );
            // A temporary sort of the <=66 materialized candidates is allowed. A
            // sort below either kind producer would sort an unbounded kind and fails.
            for (_, parent, detail) in &plan {
                if !detail.contains("TEMP B-TREE") {
                    continue;
                }
                let mut parent = *parent;
                while let Some((_, next, ancestor)) = plan.iter().find(|(id, _, _)| *id == parent) {
                    assert!(
                        !matches!(
                            ancestor.as_str(),
                            "MATERIALIZE withdrawals" | "MATERIALIZE answers"
                        ),
                        "full-kind sort: {plan:?}"
                    );
                    if *next == parent {
                        break;
                    }
                    parent = *next;
                }
            }
            assert!(
                plan.iter()
                    .any(|(_, _, detail)| detail.contains("SEARCH c USING INTEGER PRIMARY KEY")),
                "{plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|(_, _, detail)| episode_seek(detail)),
                "{plan:?}"
            );
            let rows = store.attention_history_candidates(cut, i64::MAX as u64, 32, None)?;
            assert_eq!(rows.len(), 33);
            assert!(rows.iter().all(|row| !row.omitted));
            let expected: usize = store.readers.get().query_row(
                "SELECT count(*) FROM claims WHERE kind IN ('harness.diagnostic','work.person-cancelled') AND store_index<=?1",
                [cut], |row| row.get(0))?;
            let mut before = i64::MAX as u64;
            let mut scanned = 0usize;
            let mut pages = 0usize;
            loop {
                let mut rows = store.attention_history_candidates(cut, before, 32, None)?;
                let more = rows.len() > 32;
                rows.truncate(32);
                assert!(rows.iter().all(|row| row.store_index < before && !row.omitted));
                scanned += rows.len();
                pages += 1;
                before = rows.last().map_or(before, |row| row.store_index);
                if !more { break }
                assert!(pages <= expected.div_ceil(32));
            }
            assert_eq!(scanned, expected);
            assert_eq!(pages, expected.div_ceil(32));
            Ok(())
        })
        .unwrap();
}

#[test]
fn mission_owned_person_walk_matches_the_public_run_requester() {
    let (store, origin, _) = person_work::tests::fixture();
    let run_id = origin
        .run
        .strip_prefix("mission-run/")
        .unwrap_or(&origin.run);
    for owner_run in [run_id.to_owned(), format!("mission-run/{run_id}")] {
        for status in ["running", "blocked", "completed", "failed", "cancelled"] {
            store
                .connection
                .batched(|tx| -> Result<()> {
                    tx.execute(
                        "UPDATE desired SET owner_run=?1 WHERE subject='agent/alder.asker'",
                        [&owner_run],
                    )?;
                    tx.execute(
                        "UPDATE mission_runs SET status=?1 WHERE id=?2",
                        params![status, run_id],
                    )?;
                    Ok(())
                })
                .unwrap()
                .unwrap();
            let public = store.mission_run(&owner_run).unwrap().unwrap().requester;
            assert_eq!(public, "person/avery");
            assert_eq!(
                store.agent_person("agent/alder.asker").unwrap(),
                Some(public.clone())
            );
            assert_eq!(
                store
                    .connection
                    .batched(|tx| agent_person_at(tx, "agent/alder.asker"))
                    .unwrap()
                    .unwrap(),
                Some(public)
            );
        }
    }
    // mission_run's inner join cannot return a header without its current
    // generation; the ownership walk must take its original writer fallback.
    store.connection.batched(|tx| -> Result<()> {
        tx.execute("UPDATE mission_runs SET current_generation_id='missing-fixture-generation' WHERE id=?1", [run_id])?;
        Ok(())
    }).unwrap().unwrap();
    assert!(store.mission_run(run_id).unwrap().is_none());
    assert_eq!(store.agent_person("agent/alder.asker").unwrap(), None);
    assert_eq!(
        store
            .connection
            .batched(|tx| agent_person_at(tx, "agent/alder.asker"))
            .unwrap()
            .unwrap(),
        None
    );
}

#[test]
fn an_old_registry_holds_the_delegated_answer_but_admits_its_known_notice() {
    use super::fleet_admission_tests::{admit, key, node, sync};
    let anchor = key();
    let peer_key = key();
    let current = node("alder", Some(&anchor), Some(&anchor));
    admit(&current, "alder", &anchor, "anchor", None);
    admit(&current, "older", &peer_key, "invite", None);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("older.sqlite3");
    let older = Store::open(&path, "older").unwrap();
    // The same authored fleet used by fleet_admission_tests::node. A real reopen
    // below changes this fixture's build registry, never its write-once runtime.
    older.bind_fleet("7c9e6679-7425-40de-944b-e07fc1f90ae7").unwrap();
    older.pin_fleet_anchor(anchor.public()).unwrap();
    older.set_member_key(Some(peer_key.clone())).unwrap();
    let mut old_registry = st3_schema::registry().clone();
    let diagnostic = old_registry.claims.get_mut("harness.diagnostic").unwrap();
    diagnostic.fields.remove("acted_for").unwrap();
    diagnostic.fields.remove("delegation").unwrap();
    older.set_claim_registry(old_registry);
    let (denial, proof) = seed_denial(&current, &["close-item"]);
    sync(&current, &older);
    let answer = current
        .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap();
    let notice = current.messages(Some(SEAT), true).unwrap().remove(0);
    let notice_claim = current
        .latest_claim(&notice.subject, Some("message.sent"))
        .unwrap()
        .unwrap();
    let admission = sync(&current, &older);
    assert_eq!(admission.unknown, 1);
    assert_eq!(admission.invalid, 0);
    assert!(older.claim_by_id(&answer.id).unwrap().is_none());
    assert_eq!(older.prompt_answer(&denial.id).unwrap().answer, None);
    assert!(older.claim_by_id(&notice_claim.id).unwrap().is_some());
    assert_eq!(older.messages(Some(SEAT), true).unwrap().len(), 1);
    let held = older.replica_records(true).unwrap();
    assert!(held.iter().any(
        |record| record.claim_id.as_deref() == Some(answer.id.as_str())
            && record.error_code.as_deref() == Some("unknown-claim-field")
    ));
    // A new runtime on the SAME durable store uses the current registry. The
    // write-once old runtime is dropped; no reset or rewritten claim is involved.
    drop(older);
    let upgraded = Store::open(&path, "older").unwrap();
    upgraded.set_member_key(Some(peer_key)).unwrap();
    upgraded.validate_replication_backlog().unwrap();
    upgraded.apply_replication_repairs().unwrap();
    upgraded.project_replication_backlog().unwrap();
    assert_eq!(
        upgraded.claim_by_id(&answer.id).unwrap().unwrap().body,
        answer.body
    );
    assert_eq!(
        upgraded.prompt_answer(&denial.id).unwrap().answer.as_deref(),
        Some("dismiss")
    );
    assert_eq!(upgraded.messages(Some(SEAT), true).unwrap().len(), 1);
}

fn append(
    store: &Store,
    subject: &str,
    kind: &str,
    actor: &str,
    fields: Value,
    key: &str,
) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some(actor.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })
        .unwrap()
}

fn denial_fixture(actions: &[&str]) -> (Store, ClaimRecord, DelegationProof) {
    let store = Store::open_memory("alder").unwrap();
    let (denial, proof) = seed_denial(&store, actions);
    (store, denial, proof)
}

pub(crate) fn seed_denial(store: &Store, actions: &[&str]) -> (ClaimRecord, DelegationProof) {
    let source = "version 2\nagent \"worker\" { workspace \"/tmp\"; harness \"claude\" {} }\n";
    let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
    let preview = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "declared",
            Some("person/avery"),
        )
        .unwrap();
    append(
        store,
        SEAT,
        "runtime.observed",
        SEAT,
        json!({"status":"running","incarnation_id":"one"}),
        "runtime",
    );
    let denial = append(
        store,
        SEAT,
        "harness.diagnostic",
        SEAT,
        json!({"code":"permission-denied","severity":"warning","status":"Bash","driver":"claude",
            "reason":"invented denied call","session_id":"session-one"}),
        "denial",
    );
    let instruction = append(
        store,
        "message/instruction",
        "message.sent",
        "person/avery",
        json!({"from":"person/avery","to":ACTOR,"content":"Dismiss that denied call.","status":"sent"}),
        "instruction",
    );
    let policy = store
        .set_delegation_policy(&DelegationPolicyRequest {
            person: "person/avery".into(),
            actor: "person/avery".into(),
            actions: actions.iter().map(|s| (*s).into()).collect(),
            evidence: vec![instruction.id],
            idempotency_key: "policy".into(),
        })
        .unwrap();
    let proof = DelegationProof {
        person: "person/avery".into(),
        policy: policy.id,
        message: "message/instruction".into(),
        quote: "Dismiss that denied call.".into(),
        episode: denial.id.clone(),
    };
    (denial, proof)
}

#[test]
fn granted_denial_dismissal_records_agent_person_and_closes_the_source_once() {
    let (store, denial, proof) = denial_fixture(&["close-item"]);
    assert!(
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .iter()
            .any(|item| item.episode == denial.id)
    );
    let result = store
        .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap();
    assert_eq!(result.actor.as_deref(), Some(ACTOR));
    assert_eq!(result.body["fields"]["acted_for"], proof.person);
    assert_eq!(result.body["fields"]["delegation"], json!(proof));
    let expected_input = ClaimInput {
        subject: SEAT.into(), kind: "harness.diagnostic".into(), actor: Some(ACTOR.into()),
        fields: serde_json::from_value(result.body["fields"].clone()).unwrap(),
        evidence: serde_json::from_value(result.body["evidence"].clone()).unwrap(),
        expected_subject: None,
        idempotency_key: Some(format!("native-prompt-answer:{}", denial.id)),
    };
    let (operation, digest) = smallclaims::store::claim_operation(&expected_input).unwrap().unwrap();
    assert_eq!(result.body["_operation"], json!({"id":operation,"request_digest":digest}));
    assert_eq!(
        store.prompt_answer(&denial.id).unwrap().answer.as_deref(),
        Some("dismiss")
    );
    let committed_index = store.index().unwrap();
    for _ in 0..2 {
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.episode != denial.id)
        );
        assert_eq!(
            store
                .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
                .unwrap()
                .id,
            result.id
        );
        assert_eq!(store.index().unwrap(), committed_index);
    }
    assert_eq!(store.answer_native_prompt(SEAT, &denial.id, "approve-once", "person/avery")
        .unwrap_err().code, "stale-fence");
    assert_eq!(store.respond_native_prompt(SEAT, &denial.id, "dismiss", "agent/alder.stranger", Some(&proof))
        .unwrap_err().code, "stale-fence");
    let mut changed_proof = proof.clone();
    changed_proof.quote = "a different instruction".into();
    assert_eq!(store.respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&changed_proof))
        .unwrap_err().code, "stale-fence");
    assert_eq!(store.index().unwrap(), committed_index);
    // A different, declared seat of the same person reaches the retry identity
    // fence; this does not turn an undeclared-seat ownership refusal into a pass.
    let source = "version 2\nagent \"worker\" { workspace \"/tmp\"; harness \"claude\" {} }\nagent \"peer\" { workspace \"/tmp\"; harness \"claude\" {} }\n";
    let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
    let preview = store.mission(&intent, crate::model::IntentInput { kdl:source.into(), source_name:None }).unwrap();
    store.apply_as(&intent, &preview.subject_tokens, "declared-peer", Some("person/avery")).unwrap();
    assert_eq!(store.agent_person("agent/alder.peer").unwrap().as_deref(), Some("person/avery"));
    let before_wrong_seat = store.index().unwrap();
    assert_eq!(store.respond_native_prompt("agent/alder.peer", &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap_err().code, "stale-fence");
    assert_eq!(store.index().unwrap(), before_wrong_seat);
    assert_eq!(store.claims_for(SEAT, Some("harness.diagnostic")).unwrap().iter()
        .filter(|claim| claim.body["fields"]["code"] == "native-prompt-answered").count(), 1);
    let notices = store.messages(Some(SEAT), true).unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].content.contains("Do not run it"));
    assert!(notices[0].content.contains(ACTOR));
    let notice = store.latest_claim(&notices[0].subject, Some("message.sent")).unwrap().unwrap();
    assert_eq!(notice.body["_operation"]["id"], smallclaims::store::opaque_cache_key(
        &format!("st3-denial-answer:{}", denial.id)));
}

#[test]
fn dismissal_refuses_missing_wrong_revoked_and_forged_delegation_without_source_effects() {
    for case in 0..8 {
        let (store, denial, mut proof) = denial_fixture(if case == 0 {
            &["answer-ask"]
        } else {
            &["close-item"]
        });
        let actor = if case == 1 {
            "agent/alder.stranger"
        } else {
            ACTOR
        };
        match case {
            2 => proof.person = "person/other".into(),
            3 => proof.policy = "missing-policy".into(),
            4 => proof.quote = "Not in the person's message".into(),
            5 => proof.episode = "missing-episode".into(),
            6 => {
                store
                    .set_delegation_policy(&DelegationPolicyRequest {
                        person: proof.person.clone(),
                        actor: proof.person.clone(),
                        actions: vec![],
                        evidence: vec![proof.policy.clone()],
                        idempotency_key: "revoke".into(),
                    })
                    .unwrap();
            }
            _ => {}
        }
        assert!(
            store
                .respond_native_prompt(
                    SEAT,
                    &proof.episode,
                    "dismiss",
                    actor,
                    (case != 7).then_some(&proof)
                )
                .is_err(),
            "case {case}"
        );
        assert_eq!(
            store.prompt_answer(&denial.id).unwrap().answer,
            None,
            "case {case}"
        );
        assert!(
            store.messages(Some(SEAT), true).unwrap().is_empty(),
            "case {case}"
        );
    }
}

#[test]
fn replacing_a_policy_with_the_same_actions_fences_the_old_proof() {
    let (store, denial, mut proof) = denial_fixture(&["close-item"]);
    let old_policy = proof.policy.clone();
    let replacement = store
        .set_delegation_policy(&DelegationPolicyRequest {
            person: proof.person.clone(),
            actor: proof.person.clone(),
            actions: vec!["close-item".into()],
            evidence: vec![old_policy.clone()],
            idempotency_key: "replace-with-same-actions".into(),
        })
        .unwrap();
    assert_ne!(replacement.id, old_policy);
    assert_eq!(
        store
            .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
            .unwrap_err()
            .code,
        "delegation-refused"
    );
    assert_eq!(store.prompt_answer(&denial.id).unwrap().answer, None);
    assert!(store.messages(Some(SEAT), true).unwrap().is_empty());
    proof.policy = replacement.id;
    let recorded = store
        .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap();
    assert_eq!(recorded.body["fields"]["delegation"], json!(proof));
    assert_eq!(store.messages(Some(SEAT), true).unwrap().len(), 1);
}

#[test]
fn close_item_never_authorizes_allow_kind_or_an_unproved_generic_harness_answer() {
    let (store, denial, proof) = denial_fixture(&["close-item"]);
    for answer in ["allow", "deny", "approve", "approve-once", "approve-kind"] {
        assert_eq!(
            store
                .respond_native_prompt(SEAT, &denial.id, answer, ACTOR, Some(&proof))
                .unwrap_err()
                .code,
            "delegation-refused"
        );
    }
    let input = ClaimInput {
        subject: SEAT.into(),
        kind: "harness.diagnostic".into(),
        actor: Some(ACTOR.into()),
        fields: serde_json::from_value(
            json!({"code":"native-prompt-answered","status":"dismiss","driver":"claude"}),
        )
        .unwrap(),
        evidence: vec![denial.id],
        expected_subject: None,
        idempotency_key: Some("unproved-answer".into()),
    };
    assert_eq!(
        store.append_claim(&input).unwrap_err().code,
        "delegation-refused"
    );
}

#[test]
fn a_denial_cleared_after_validation_is_refused_again_inside_the_writer() {
    for state in ["working", "ended"] {
        let (store, denial, proof) = denial_fixture(&["close-item"]);
        let mut fields = BTreeMap::from([
            ("code".into(), json!("native-prompt-answered")),
            ("status".into(), json!("dismiss")),
            ("driver".into(), json!("claude")),
        ]);
        delegation::add_fields(&mut fields, &proof);
        let mut evidence = vec![denial.id.clone()];
        store.delegation_evidence(&mut evidence, &proof).unwrap();
        // Establish a valid prepared action before the source transition. Reusing
        // these exact fields after it must not rely on that earlier validation.
        store
            .connection
            .batched(|tx| {
                delegation::validate_write(
                    tx,
                    SEAT,
                    "harness.diagnostic",
                    Some(ACTOR),
                    &fields,
                    &evidence,
                )
            })
            .unwrap()
            .unwrap();
        if state == "working" {
            // Match the source predicate's strict millisecond boundary, not a
            // performance oracle. The ended branch has no clock precondition.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let moved = append(
            &store,
            SEAT,
            "harness.observed",
            SEAT,
            json!({"state":state,"driver":"claude","incarnation_id":"one"}),
            "moved-on",
        );
        if state == "working" {
            assert!(
                moved.body["fields"]["observed_since_ms"].as_u64().unwrap() as u128
                    > denial.accepted_at_unix_ms
            );
        }
        let before = store.index().unwrap();
        let body = json!({"fields":fields,"evidence":evidence});
        let error = store
            .connection
            .batched(|tx| {
                append_receipt_claim_tx(
                    tx,
                    &store.origin,
                    SEAT,
                    "harness.diagnostic",
                    Some(ACTOR),
                    &body,
                    std::slice::from_ref(&denial.id),
                    None,
                    &format!("native-prompt-answer:{}", denial.id),
                )
            })
            .unwrap()
            .unwrap_err();
        assert_eq!(claim_append_error(error).code, "stale-fence", "{state}");
        assert_eq!(store.index().unwrap(), before);
        assert_eq!(store.prompt_answer(&denial.id).unwrap().answer, None);
        assert!(store.messages(Some(SEAT), true).unwrap().is_empty());
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .iter()
                .all(|item| item.episode != denial.id)
        );
        // The public adapter's preflight agrees with the writer's current fence.
        assert_eq!(
            store
                .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
                .unwrap_err()
                .code,
            "stale-fence"
        );
    }
    let (store, denial, proof) = denial_fixture(&["close-item"]);
    store
        .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap();
}

#[test]
fn delegated_dismissal_fences_a_restarted_runtime_and_changed_person() {
    for changed_person in [false, true] {
        let (store, denial, proof) = denial_fixture(&["close-item"]);
        if changed_person {
            let source =
                "version 2\nagent \"worker\" { workspace \"/tmp/other\"; harness \"claude\" {} }\n";
            let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
            let preview = store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            store
                .apply_as(
                    &intent,
                    &preview.subject_tokens,
                    "changed-person",
                    Some("person/other"),
                )
                .unwrap();
        } else {
            append(
                &store,
                SEAT,
                "runtime.observed",
                SEAT,
                json!({"status":"running","incarnation_id":"two"}),
                "restarted",
            );
        }
        assert!(
            store
                .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
                .is_err()
        );
        assert_eq!(store.prompt_answer(&denial.id).unwrap().answer, None);
    }
}

#[test]
fn a_failed_denial_notice_rolls_back_the_delegated_close_and_operation() {
    let (store, denial, proof) = denial_fixture(&["close-item"]);
    store.connection.batched(|tx| -> Result<()> {
        tx.execute_batch("CREATE TEMP TRIGGER refuse_denial_notice BEFORE INSERT ON claims WHEN NEW.kind='message.sent' BEGIN SELECT RAISE(ABORT,'authored notice failure'); END")?;
        Ok(())
    }).unwrap().unwrap();
    assert!(
        store
            .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
            .is_err()
    );
    assert_eq!(store.prompt_answer(&denial.id).unwrap().answer, None);
    store
        .connection
        .batched(|tx| -> Result<()> {
            tx.execute_batch("DROP TRIGGER refuse_denial_notice")?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    store
        .respond_native_prompt(SEAT, &denial.id, "dismiss", ACTOR, Some(&proof))
        .unwrap();
}

#[test]
fn sender_withdraws_its_update_without_releasing_its_held_step_and_retains_history() {
    let (store, origin, mut input) = person_work::tests::fixture();
    input.step = None;
    input.incarnation = None;
    input.request = Some(json!({"version":1,"type":"update","about":origin.run}));
    let update = store.ask_person(&input).unwrap();
    let episode = store
        .claims_for(&update.subject, Some("work.person-asked"))
        .unwrap()[0]
        .id
        .clone();
    let before = store.step_run(&origin.subject).unwrap().unwrap();
    let mut request = crate::model::PersonStepResponse {
        delegation: None,
        subject: update.subject.clone(),
        actor: input.actor.clone(),
        summary: "The old update is withdrawn".into(),
        evidence: vec![],
        episode: Some(episode),
        answer: None,
        idempotency_key: "withdraw-own-update".into(),
    };
    request.actor = "agent/alder.stranger".into();
    assert_eq!(
        store.withdraw_person_update(&request).unwrap_err().code,
        "forbidden"
    );
    request.actor = input.actor;
    let ended = store.withdraw_person_update(&request).unwrap();
    assert_eq!(ended.status, "cancelled");
    assert_eq!(
        store.withdraw_person_update(&request).unwrap().status,
        "cancelled"
    );
    let after = store.step_run(&origin.subject).unwrap().unwrap();
    assert_eq!(
        (after.status, after.claimant, after.claim_expires_at_unix_ms),
        (
            before.status,
            before.claimant,
            before.claim_expires_at_unix_ms
        )
    );
    assert!(
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .iter()
            .all(|item| item.subject != update.subject)
    );
    let history = store.claims_for(&update.subject, None).unwrap();
    assert!(
        history
            .iter()
            .any(|claim| claim.kind == "work.person-asked")
    );
    assert_eq!(
        history
            .iter()
            .filter(|claim| claim.kind == "work.person-cancelled")
            .count(),
        1
    );
}

#[test]
fn concurrent_delegated_dismissals_have_one_source_answer_and_one_notice() {
    let (store, denial, proof) = denial_fixture(&["close-item"]);
    let store = std::sync::Arc::new(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let (store, barrier, proof, episode) = (
                store.clone(),
                barrier.clone(),
                proof.clone(),
                denial.id.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                store.respond_native_prompt(SEAT, &episode, "dismiss", ACTOR, Some(&proof))
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert!(results.iter().any(|result| result.is_ok()));
    let answers = store
        .claims_for(SEAT, Some("harness.diagnostic"))
        .unwrap()
        .into_iter()
        .filter(|claim| claim.body["fields"]["code"] == "native-prompt-answered")
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 1);
    for result in results.into_iter().flatten() {
        assert_eq!(result.id, answers[0].id);
    }
    assert_eq!(store.messages(Some(SEAT), true).unwrap().len(), 1);
}

#[test]
fn a_person_read_racing_sender_withdrawal_has_one_terminal_source_outcome() {
    let (store, origin, mut input) = person_work::tests::fixture();
    input.step = None;
    input.incarnation = None;
    input.request = Some(json!({"version":1,"type":"update","about":origin.run}));
    let update = store.ask_person(&input).unwrap();
    let episode = store
        .claims_for(&update.subject, Some("work.person-asked"))
        .unwrap()[0]
        .id
        .clone();
    let request = crate::model::PersonStepResponse {
        delegation: None,
        subject: update.subject.clone(),
        actor: input.actor,
        summary: "Withdrawn".into(),
        evidence: vec![],
        episode: Some(episode),
        answer: None,
        idempotency_key: "racing-withdraw".into(),
    };
    let store = std::sync::Arc::new(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let reader = {
        let (store, barrier, mut request) = (store.clone(), barrier.clone(), request.clone());
        std::thread::spawn(move || {
            request.actor = "person/avery".into();
            request.summary = "Read".into();
            request.answer = Some(crate::person_request::AnswerInput {
                id: Some("read".into()),
                text: None,
            });
            request.idempotency_key = "racing-read".into();
            barrier.wait();
            store.finish_person_step(&request, false)
        })
    };
    barrier.wait();
    let withdrawn = store.withdraw_person_update(&request);
    let read = reader.join().unwrap();
    assert_ne!(withdrawn.is_ok(), read.is_ok());
    let history = store.claims_for(&update.subject, None).unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|c| matches!(
                c.kind.as_str(),
                "work.person-done" | "work.person-cancelled"
            ))
            .count(),
        1
    );
    assert_eq!(
        store.step_run(&origin.subject).unwrap().unwrap().status,
        "claimed"
    );
}

#[test]
fn update_only_withdrawal_cannot_cancel_an_attached_question() {
    let (store, origin, input) = person_work::tests::fixture();
    let ask = store.ask_person(&input).unwrap();
    let mut request = crate::model::PersonStepResponse {
        delegation: None,
        subject: ask.subject.clone(),
        actor: input.actor,
        summary: "Wrong route".into(),
        evidence: vec![],
        episode: None,
        answer: None,
        idempotency_key: "wrong-kind-withdraw".into(),
    };
    let index = store.index().unwrap();
    assert_eq!(
        store.withdraw_person_update(&request).unwrap_err().code,
        "invalid-update-withdrawal"
    );
    // The existing source-kind refusal precedes ownership validation. The
    // separate update control requires forbidden for a stranger's actual update.
    request.actor = "agent/node.intruder".into();
    assert_eq!(
        store.withdraw_person_update(&request).unwrap_err().code,
        "invalid-update-withdrawal"
    );
    assert_eq!(store.index().unwrap(), index);
    assert_eq!(
        store.step_run(&ask.subject).unwrap().unwrap().status,
        "ready"
    );
    assert_eq!(
        store.step_run(&origin.subject).unwrap().unwrap().status,
        "waiting-person"
    );
}
