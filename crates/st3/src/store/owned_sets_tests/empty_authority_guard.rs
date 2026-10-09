//! Current statement proof controls; synthetic corruption is explicitly reader-only.
use super::*;

#[test]
fn empty_effect_authority_cost_is_one_current_statement_per_invocation() {
    let store = Store::open_memory("amber").unwrap();
    let source = format!(
        "version 2\n{}",
        (0..64)
            .map(|index| format!(
                "agent \"remote/{index}\" {{ host \"cobalt\"; command \"true\" }}\n"
            ))
            .collect::<String>()
    );
    direct(
        &store,
        &parse_intent(&source, "amber").unwrap(),
        "remote-roster",
    )
    .unwrap();
    let desired = store.desired_subjects().unwrap();
    store.owned_desired_guard(&desired[0]).unwrap();
    for roster in [&desired[..1], &desired[..8], &desired[..64]] {
        let before = smallclaims::sqlite::work::total();
        for declaration in roster {
            let (result, reads) =
                smallclaims::touched::record(|| store.owned_desired_guard(declaration));
            result.unwrap();
            assert!(reads.contains("kind:owned-set.revised"));
            assert!(reads.contains(&declaration.subject));
        }
        let spent = smallclaims::sqlite::work::total() - before;
        assert_eq!(spent.statements, roster.len() as u64, "{spent:?}");
        assert_eq!(spent.fullscan_steps, 0, "{spent:?}");
        assert!(spent.vm_steps <= roster.len() as u64 * 100, "{spent:?}");
        println!("empty-authority roster={} cost={spent:?}", roster.len());
    }
    assert_pass_candidates_match_effect_guards(&store, &desired);
}

#[test]
fn empty_effect_authority_cost_does_not_grow_with_subject_history() {
    let store = Store::open_memory("amber").unwrap();
    direct(&store, &bundle("unowned", false), "legacy").unwrap();
    let captured = store.desired_subjects().unwrap().remove(0);
    let mut inserted = 0;
    let mut prior_vm = None;
    for history in [100, 10_000] {
        {
            // Reader-cost fixture only: these rows model retained runtime history, not
            // admission, canonical ordering, replication, or an owned-set receipt.
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            let batch = format!("reader-history-{history}");
            tx.execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                 VALUES(?1,'amber',?2,?1,'1')",
                params![batch, history],
            )
            .unwrap();
            {
                let mut insert = tx
                    .prepare(
                        "INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
                         VALUES(?1,?2,?3,'harness.usage','amber',NULL,?4,'[]','1')",
                    )
                    .unwrap();
                for index in inserted..history {
                    for subject in [captured.subject.as_str(), "agent/unrelated-history"] {
                        insert
                            .execute(params![
                                format!("reader-history-{subject}-{index}"),
                                batch,
                                subject,
                                json!({"fields": {"input_tokens": index}}).to_string()
                            ])
                            .unwrap();
                    }
                }
            }
            tx.commit().unwrap();
        }
        inserted = history;
        store.owned_desired_guard(&captured).unwrap();
        let before = smallclaims::sqlite::work::total();
        let (result, reads) = smallclaims::touched::record(|| store.owned_desired_guard(&captured));
        result.unwrap();
        let spent = smallclaims::sqlite::work::total() - before;
        assert!(reads.contains("kind:owned-set.revised"));
        assert!(reads.contains(&captured.subject));
        assert_eq!(spent.statements, 1, "history={history} {spent:?}");
        assert_eq!(spent.fullscan_steps, 0, "history={history} {spent:?}");
        assert!(spent.vm_steps <= 100, "history={history} {spent:?}");
        if let Some(prior) = prior_vm {
            assert!(spent.vm_steps <= prior + 10, "history={history} {spent:?}");
        }
        prior_vm = Some(spent.vm_steps);
        println!("empty-authority history-per-subject={history} cost={spent:?}");
    }
}

#[test]
fn first_receipt_after_empty_proof_fences_the_captured_old_member() {
    let store = Store::open_memory("amber").unwrap();
    direct(&store, &bundle("unowned", false), "legacy").unwrap();
    let captured = store.desired_subjects().unwrap().remove(0);
    store.owned_desired_guard(&captured).unwrap();
    let input = bundle("owned", false);
    let mut opts = options(&store, 10);
    opts.adopt.insert(captured.subject.clone());
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(&input, &opts, "first-receipt", "person/operator")
        .unwrap();
    assert_eq!(
        store.owned_desired_guard(&captured).unwrap_err().code,
        "stale-set-member"
    );
    let fresh = store.desired_subjects().unwrap().remove(0);
    let before = smallclaims::sqlite::work::total();
    store.owned_desired_guard(&fresh).unwrap();
    let spent = smallclaims::sqlite::work::total() - before;
    assert!(spent.statements > 1 && spent.statements <= 20, "{spent:?}");
    println!("nonempty-authority one-member fallback cost={spent:?}");
    assert_pass_candidates_match_effect_guards(&store, &[captured, fresh]);
}

#[test]
fn staged_only_proof_uses_any_claim_and_the_exact_non_null_marker() {
    for marker in [
        json!(""),
        json!(false),
        json!(0),
        json!({}),
        json!([]),
        Value::Null,
    ] {
        let store = Store::open_memory("amber").unwrap();
        direct(&store, &bundle("unowned", false), "legacy").unwrap();
        let captured = store.desired_subjects().unwrap().remove(0);
        store.owned_desired_guard(&captured).unwrap();
        {
            // Reader-contract fixture, not admission of an arbitrary claim kind/marker.
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            let id = "reader-staged-fixture";
            tx.execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                 VALUES(?1,'amber',1,?1,'1')",
                [id],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
                 VALUES(?1,?1,?2,'fixture.staged','amber','person/operator',?3,'[]','1')",
                params![id, captured.subject, json!({"owned_set": marker}).to_string()],
            ).unwrap();
            tx.commit().unwrap();
        }
        assert!(store.owned_sets().unwrap().is_empty());
        if marker.is_null() {
            store.owned_desired_guard(&captured).unwrap();
        } else {
            assert_eq!(
                store.owned_desired_guard(&captured).unwrap_err().code,
                "owned-set-pending"
            );
        }
    }
}

#[test]
fn empty_proof_errors_and_pinned_reads_do_not_authorize_effects() {
    let store = Store::open_memory("amber").unwrap();
    let declaration = bundle("unowned", false)
        .subjects
        .into_values()
        .next()
        .unwrap();
    store.owned_desired_guard(&declaration).unwrap();
    #[cfg(debug_assertions)]
    store
        .read_snapshot(|_| -> Result<()> {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    store.owned_desired_guard(&declaration)
                }))
                .is_err(),
                "an effect probe cannot reuse an enclosing read snapshot"
            );
            Ok(())
        })
        .unwrap();
    store.owned_desired_guard(&declaration).unwrap();
    // Isolated empty-store SQL failure injection; no admission/replication claim is made.
    store
        .connection
        .write()
        .execute_batch("DROP TABLE claims")
        .unwrap();
    let (failed, reads) = smallclaims::touched::record(|| store.owned_desired_guard(&declaration));
    assert_eq!(failed.unwrap_err().code, "internal");
    assert!(reads.contains("kind:owned-set.revised"));
    assert!(reads.contains(&declaration.subject));
}
