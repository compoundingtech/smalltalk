use super::owned_sets::{Options, Source};
use super::*;
use crate::parse_intent;

fn bundle(command: &str, meadow: bool) -> NormalizedIntent {
    parse_intent(
        &format!(
            "version 2\nagent \"garden/orchard\" {{ command {command:?} }}\n{}",
            if meadow {
                "agent \"garden/meadow\" { command \"true\" }"
            } else {
                ""
            }
        ),
        "amber",
    )
    .unwrap()
}
fn options(store: &Store, sequence: u64) -> Options {
    let current = store
        .owned_sets()
        .unwrap()
        .into_iter()
        .find(|s| s.id == "owned-set/garden");
    Options {
        set: "garden".into(),
        source: Source {
            repository: "acme/garden".into(),
            r#ref: "refs/heads/main".into(),
            sha: format!("{sequence:040x}"),
            sequence,
        },
        expected_set: current.map_or("absent".into(), |s| s.revision),
        rollout: None,
        adopt: Default::default(),
        allow_empty: false,
        confirm_retire: None,
        expected_subjects: Default::default(),
    }
}
fn apply(store: &Store, input: &NormalizedIntent, sequence: u64) -> ApplyResponse {
    let mut opts = options(store, sequence);
    opts.expected_subjects = store
        .owned_set_preview(input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(input, &opts, &format!("set-{sequence}"), "person/operator")
        .unwrap()
}
fn direct(store: &Store, input: &NormalizedIntent, key: &str) -> Result<ApplyResponse, St3Error> {
    let preview = store
        .mission(
            input,
            IntentInput {
                kdl: "".into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply_as(input, &preview.subject_tokens, key, Some("person/operator"))
}
fn share(from: &Store, to: &Store) {
    to.import_replication(&from.origin, &from.export_replication(0).unwrap())
        .unwrap();
}

fn assert_pass_candidates_match_effect_guards(store: &Store, desired: &[DesiredSubject]) {
    let (expected, effect_reads) = smallclaims::touched::record(|| {
        desired
            .iter()
            .filter(|subject| store.owned_desired_guard(subject).is_ok())
            .map(|subject| subject.subject.clone())
            .collect::<BTreeSet<_>>()
    });
    let (actual, pass_reads) =
        smallclaims::touched::record(|| store.owned_desired_subjects(desired).unwrap());
    assert_eq!(actual, expected);
    assert!(effect_reads.is_subset(&pass_reads));
}

#[test]
fn empty_ownership_pass_cost_does_not_grow_with_the_roster_or_unrelated_history() {
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
    assert_eq!(desired.len(), 64);
    assert!(store.owned_sets().unwrap().is_empty());
    let ask = |desired: &[DesiredSubject]| {
        let before = STATEMENTS_RUN.with(std::cell::Cell::get);
        let candidates = store.owned_desired_subjects(desired).unwrap();
        let spent = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
        (candidates, spent)
    };
    // Warm pooled readers before counting; this measures executed SQL, not wall time.
    ask(&desired);
    let (_, single_cost) = ask(&desired[..1]);
    let (candidates, roster_cost) = ask(&desired);
    assert_eq!(candidates.len(), desired.len());
    assert_eq!(roster_cost, single_cost);
    let before = STATEMENTS_RUN.with(std::cell::Cell::get);
    for _ in 0..3 {
        for subject in &desired {
            store.owned_desired_guard(subject).unwrap();
        }
    }
    let legacy_cost = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
    assert!(
        legacy_cost > 8 * roster_cost,
        "legacy={legacy_cost}, pass={roster_cost}"
    );
    for index in 0..512 {
        store
            .append_claim(&ClaimInput {
                subject: "agent/unrelated-history".into(),
                kind: "harness.usage".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("driver".into(), json!("codex")),
                    ("semantics".into(), json!("session_cumulative")),
                    ("incarnation_id".into(), json!("unrelated-incarnation")),
                    ("input_tokens".into(), json!(index)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let (after, history_cost) = ask(&desired);
    assert_eq!(after, candidates);
    assert_eq!(history_cost, roster_cost);
}

#[test]
fn pass_candidates_preserve_snapshot_authority_but_do_not_authorize_later_effects() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("amber.sqlite3"), "amber").unwrap();
    apply(&store, &bundle("first", true), 10);
    let old = store.desired_subjects().unwrap();
    assert_pass_candidates_match_effect_guards(&store, &old);
    let candidates = store.owned_desired_subjects(&old).unwrap();
    store
        .read_snapshot(|_| {
            assert_eq!(store.owned_desired_subjects(&old).unwrap(), candidates);
            std::thread::scope(|scope| {
                scope.spawn(|| apply(&store, &bundle("second", true), 20));
            });
            assert_eq!(store.owned_desired_subjects(&old).unwrap(), candidates);
            Ok(())
        })
        .unwrap();
    // A saved pass result is deliberately insufficient to authorize a later effect.
    assert!(candidates.contains("agent/garden/orchard"));
    let orchard = old
        .iter()
        .find(|subject| subject.subject == "agent/garden/orchard")
        .unwrap();
    assert!(store.owned_desired_guard(orchard).is_err());
    assert_pass_candidates_match_effect_guards(&store, &old);
    assert_pass_candidates_match_effect_guards(&store, &store.desired_subjects().unwrap());
}

#[test]
fn missing_staged_subject_index_does_not_fence_valid_candidates() {
    let store = Store::open_memory("amber").unwrap();
    direct(&store, &bundle("unowned", false), "initial").unwrap();
    let desired = store.desired_subjects().unwrap();
    store.owned_desired_guard(&desired[0]).unwrap();
    let before = store.owned_desired_subjects(&desired).unwrap();
    store
        .connection
        .write()
        .execute_batch("DROP INDEX claims_owned_set_subject_index")
        .unwrap();
    assert_eq!(store.owned_desired_subjects(&desired).unwrap(), before);
    assert_pass_candidates_match_effect_guards(&store, &desired);
}

#[test]
fn conflicting_owners_fence_the_same_candidates_as_individual_guards() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("owned", false), 10);
    let desired = store.desired_subjects().unwrap();
    let receipt = store.owned_sets().unwrap().remove(0).receipt;
    store
        .append_claim(&ClaimInput {
            subject: "owned-set/other".into(),
            kind: "owned-set.revised".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"revision": canonical_hash(&receipt).unwrap(), "body": receipt}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert_eq!(
        store.owned_desired_guard(&desired[0]).unwrap_err().code,
        "owned-set-conflict"
    );
    assert!(store.owned_desired_subjects(&desired).unwrap().is_empty());
    assert_pass_candidates_match_effect_guards(&store, &desired);
}

#[test]
fn nonempty_ownership_batches_reduce_statements_without_claiming_bounded_history_cost() {
    let store = Store::open_memory("amber").unwrap();
    for sequence in [10, 20, 30, 40] {
        apply(&store, &bundle(&format!("echo {sequence}"), true), sequence);
    }
    let desired = store.desired_subjects().unwrap();
    assert_eq!(store.owned_set_history("garden").unwrap().len(), 4);
    let before = STATEMENTS_RUN.with(std::cell::Cell::get);
    for _ in 0..3 {
        for subject in &desired {
            store.owned_desired_guard(subject).unwrap();
        }
    }
    let legacy_cost = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
    let before = STATEMENTS_RUN.with(std::cell::Cell::get);
    let candidates = store.owned_desired_subjects(&desired).unwrap();
    let batch_cost = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
    assert_eq!(candidates.len(), desired.len());
    assert!(
        batch_cost < legacy_cost,
        "batch={batch_cost}, legacy={legacy_cost}"
    );
    assert_pass_candidates_match_effect_guards(&store, &desired);
    // Parsing/hashing these receipts is still work even if the query count stays the same.
    apply(&store, &bundle("new authority", true), 50);
    assert!(
        !store
            .owned_desired_subjects(&desired)
            .unwrap()
            .contains("agent/garden/orchard")
    );
    assert_pass_candidates_match_effect_guards(&store, &desired);
    assert_pass_candidates_match_effect_guards(&store, &store.desired_subjects().unwrap());
}

#[test]
fn staged_ownership_is_invisible_after_rollback_and_fences_after_commit() {
    let store = Store::open_memory("amber").unwrap();
    direct(&store, &bundle("unowned", false), "initial").unwrap();
    let desired = store.desired_subjects().unwrap();
    let initial = store.owned_desired_subjects(&desired).unwrap();
    let mut staged = serde_json::to_value(&desired[0]).unwrap();
    staged["owned_set"] = json!("owned-set/garden");
    let index = store.index().unwrap();
    for commit in [false, true] {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        append_claim_tx(
            &transaction,
            "amber",
            &desired[0].subject,
            "intent.desired",
            Some("person/operator"),
            &staged,
            &[],
            None,
        )
        .unwrap();
        if commit {
            transaction.commit().unwrap();
        } else {
            transaction.rollback().unwrap();
        }
        drop(writer);
        if commit {
            assert!(store.owned_desired_subjects(&desired).unwrap().is_empty());
            assert_eq!(
                store.owned_desired_guard(&desired[0]).unwrap_err().code,
                "owned-set-pending"
            );
        } else {
            assert_eq!(store.index().unwrap(), index);
            assert_eq!(store.owned_desired_subjects(&desired).unwrap(), initial);
            store.owned_desired_guard(&desired[0]).unwrap();
        }
        assert_pass_candidates_match_effect_guards(&store, &desired);
    }
}

#[test]
fn receipt_read_errors_do_not_turn_into_eligible_candidates() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("owned", false), 10);
    let unowned = parse_intent(
        "version 2\nagent \"unmanaged\" { command \"true\" }",
        "amber",
    )
    .unwrap();
    direct(&store, &unowned, "unmanaged").unwrap();
    let desired = store.desired_subjects().unwrap();
    assert_eq!(store.owned_desired_subjects(&desired).unwrap().len(), 2);
    let receipt = store.owned_sets().unwrap().remove(0);
    // Inject a receipt decoding failure after declarations were captured, in this test only.
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms='invalid' WHERE id=?1",
            [&receipt.claim],
        )
        .unwrap();
    assert!(store.owned_desired_subjects(&desired).is_err());
    for declaration in desired {
        assert!(store.owned_desired_guard(&declaration).is_err());
    }
}

#[test]
fn repaired_receipt_eligibility_is_rechecked_between_passes() {
    let source = Store::open_memory("amber").unwrap();
    let target = Store::open_memory("cobalt").unwrap();
    apply(&source, &bundle("first", false), 10);
    let first = source.owned_sets().unwrap().remove(0);
    let old = source.desired_subjects().unwrap();
    apply(&source, &bundle("second", false), 20);
    let second = source.owned_sets().unwrap().remove(0);
    let new = source.desired_subjects().unwrap();
    // Repair addresses envelope records, which the legacy claim-only transport does not create.
    super::tests::receive_and_project(
        &target,
        &source.origin,
        &super::tests::exchange_from(&source, &target.replication_inventory().unwrap()),
    );
    assert!(target.owned_desired_subjects(&old).unwrap().is_empty());
    assert_pass_candidates_match_effect_guards(&target, &new);
    target.owned_desired_guard(&new[0]).unwrap();
    let record = target
        .replica_records(false)
        .unwrap()
        .into_iter()
        .find(|record| record.claim_id.as_deref() == Some(second.claim.as_str()))
        .unwrap();
    // Model the existing version-skew repair path: the receiver now rejects this receipt.
    target
        .connection
        .write()
        .execute(
            "UPDATE replica_records SET state='invalid' WHERE record_ref=?1",
            [&record.record_ref],
        )
        .unwrap();
    target
        .repair_replica_record(
            &record.record_ref,
            &first.claim,
            "receiver rejects this receipt after an upgrade",
            "person/operator",
            "repair-receipt",
        )
        .unwrap();
    assert_eq!(target.owned_sets().unwrap()[0].claim, first.claim);
    assert_eq!(target.owned_desired_guard(&new[0]).unwrap_err().code, "stale-set-member");
    target.owned_desired_guard(&old[0]).unwrap();
    assert!(target.owned_desired_subjects(&new).unwrap().is_empty());
    assert!(
        target
            .owned_desired_subjects(&old)
            .unwrap()
            .contains(&old[0].subject)
    );
    assert_pass_candidates_match_effect_guards(&target, &old);
    assert_pass_candidates_match_effect_guards(&target, &new);
}

#[test]
fn receipt_reuse_keeps_historical_bounds_and_ends_with_the_read_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let amber = Store::open(&directory.path().join("amber.sqlite3"), "amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    let subject = "agent/garden/orchard";
    apply(&amber, &bundle("first", false), 10);
    let first_index = amber.index().unwrap();
    let first = amber.selected_desired_token(subject).unwrap().unwrap();
    apply(&amber, &bundle("second", false), 11);
    let second = amber.selected_desired_token(subject).unwrap().unwrap();
    apply(&cobalt, &bundle("other-store", false), 20);

    amber
        .read_snapshot(|index| {
            amber.with_owned_set_snapshot_reads(|| {
                let connection = amber.readers.get();
                // Current and historical receipt reads must never borrow each other's results.
                for _ in 0..2 {
                    assert_eq!(amber.owned_sets().unwrap()[0].receipt.source.sequence, 11);
                    assert_eq!(
                        owned_sets::desired_at(&connection, subject, first_index)?
                            .unwrap()
                            .claim_id,
                        first
                    );
                    assert_eq!(
                        owned_sets::desired_at(&connection, subject, index)?
                            .unwrap()
                            .claim_id,
                        second
                    );
                    assert_eq!(cobalt.owned_sets().unwrap()[0].receipt.source.sequence, 20);
                }
                // A new publication on another thread cannot tear the pinned read.
                std::thread::scope(|scope| {
                    scope.spawn(|| apply(&amber, &bundle("third", false), 12));
                });
                assert_eq!(amber.owned_sets().unwrap()[0].receipt.source.sequence, 11);
                // A failed nested scope restores the enclosing read's receipt scope.
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        amber.with_owned_set_snapshot_reads(|| panic!("interrupted read"));
                    }))
                    .is_err()
                );
                assert_eq!(amber.owned_sets().unwrap()[0].receipt.source.sequence, 11);
                Ok(())
            })
        })
        .unwrap();
    // Reusing a pooled connection on the next request must see the new graph authority.
    amber
        .read_snapshot(|_| {
            amber.with_owned_set_snapshot_reads(|| {
                assert_eq!(amber.owned_sets().unwrap()[0].receipt.source.sequence, 12);
                Ok(())
            })
        })
        .unwrap();
    assert_eq!(amber.owned_sets().unwrap()[0].receipt.source.sequence, 12);
}

#[test]
fn one_shot_set_retirement_is_fenced_to_the_member_and_its_runtime_host() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    let source = "version 2\nagent \"garden/orchard\" { command \"sleep 100\"; one-shot }";
    let declared = parse_intent(source, "amber").unwrap();
    apply(&amber, &declared, 10);
    share(&amber, &cobalt);
    let subject = "agent/garden/orchard";
    let token = amber.selected_desired_token(subject).unwrap().unwrap();
    let expected = BTreeMap::from([(subject.to_owned(), vec![token.clone()])]);
    let stop = parse_intent("version 2\nstop \"agent/garden/orchard\"", "amber").unwrap();
    for (store, actor, key) in [
        (&amber, "person/operator", "person-stop"),
        (&cobalt, "daemon/runtime", "remote-stop"),
    ] {
        let index = store.index().unwrap();
        assert_eq!(
            store
                .apply_as(&stop, &expected, key, Some(actor))
                .unwrap_err()
                .code,
            "set-managed-subject"
        );
        assert_eq!(store.index().unwrap(), index);
    }
    amber
        .apply_as(&stop, &expected, "one-shot-retire", Some("daemon/runtime"))
        .unwrap();
    assert_eq!(
        amber.selected_desired_kind(subject).unwrap().as_deref(),
        Some("stop")
    );
    assert_eq!(amber.owned_sets().unwrap()[0].receipt.source.sequence, 10);
    let stopped = amber
        .desired_subjects_named(&[subject.into()])
        .unwrap()
        .remove(0);
    assert!(amber.owned_desired_guard(&stopped).is_ok());
    assert!(
        amber
            .owned_desired_subjects(std::slice::from_ref(&stopped))
            .unwrap()
            .contains(&stopped.subject)
    );
    assert_eq!(
        amber
            .declaration_ended_by_stop(subject)
            .unwrap()
            .unwrap()
            .declaration,
        declared.subjects[subject]
    );

    // Replication and historical reads select the same stop through the exact source member.
    share(&amber, &cobalt);
    assert_eq!(
        cobalt.selected_desired_kind(subject).unwrap().as_deref(),
        Some("stop")
    );
    assert_eq!(
        amber.selected_desired_token(subject).unwrap(),
        cobalt.selected_desired_token(subject).unwrap()
    );
    let index = cobalt.index().unwrap();
    let connection = cobalt.readers.get();
    assert_eq!(
        owned_sets::desired_at(&connection, subject, index)
            .unwrap()
            .unwrap()
            .kind,
        "stop"
    );
    drop(connection);

    // A new declaration through the owning source re-arms the seat. A delayed old exit cannot
    // replace it, locally or when its stop later reaches another replica.
    let replacement = parse_intent(&source.replace("sleep 100", "sleep 200"), "amber").unwrap();
    apply(&amber, &replacement, 20);
    let current = amber.selected_desired_token(subject).unwrap();
    assert_eq!(
        amber
            .apply_as(
                &stop,
                &expected,
                "stale-one-shot-exit",
                Some("daemon/runtime")
            )
            .unwrap_err()
            .code,
        "stale-subject"
    );
    assert_eq!(amber.selected_desired_token(subject).unwrap(), current);
    share(&amber, &cobalt);
    assert_eq!(
        cobalt.selected_desired_kind(subject).unwrap().as_deref(),
        Some("agent")
    );
}

#[test]
fn daemon_cannot_retire_an_unmarked_set_member_as_a_one_shot() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("sleep 100", false), 10);
    let subject = "agent/garden/orchard";
    let stop = parse_intent("version 2\nstop \"agent/garden/orchard\"", "amber").unwrap();
    let expected = BTreeMap::from([(
        subject.into(),
        vec![store.selected_desired_token(subject).unwrap().unwrap()],
    )]);
    assert_eq!(
        store
            .apply_as(&stop, &expected, "unmarked-stop", Some("daemon/runtime"))
            .unwrap_err()
            .code,
        "set-managed-subject"
    );
    assert_eq!(
        store.selected_desired_kind(subject).unwrap().as_deref(),
        Some("agent")
    );
}

#[test]
fn a_staged_owned_set_member_check_seeks_tagged_claims() {
    let store = Store::open_memory("amber").unwrap();
    let unmanaged = parse_intent(
        "version 2\nagent \"garden/unmanaged\" { command \"true\" }",
        "amber",
    )
    .unwrap();
    direct(&store, &unmanaged, "unmanaged").unwrap();
    apply(&store, &bundle("true", false), 10);
    let connection = store.readers.get();
    let query = "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND json_extract(body,'$.owned_set') IS NOT NULL)";
    for (subject, expected) in [
        ("agent/garden/orchard", true),
        ("agent/garden/unmanaged", false),
        ("agent/garden/absent", false),
    ] {
        let staged: bool = connection
            .query_row(query, [subject], |row| row.get(0))
            .unwrap();
        assert_eq!(staged, expected, "{subject}");
    }
    let plan = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
        .unwrap()
        .query_map(["agent/garden/unmanaged"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    assert!(
        plan.contains("claims_owned_set_subject_index (subject=?)"),
        "{plan}"
    );
}

#[test]
fn stale_render_is_refused_even_with_fresh_set_and_member_heads() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("new", false), 30);
    let input = bundle("old", false);
    let mut opts = options(&store, 20);
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    let before = store.index().unwrap();
    let error = store
        .apply_owned_set(&input, &opts, "old", "person/operator")
        .unwrap_err();
    assert_eq!(error.code, "owned-set-refused");
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 30);
}

#[test]
fn partition_heal_selects_highest_source_and_blocks_stale_member_writes() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    let ivory = Store::open_memory("ivory").unwrap();
    apply(&amber, &bundle("initial", true), 10);
    share(&amber, &cobalt);
    share(&amber, &ivory);
    apply(&amber, &bundle("new", true), 30);
    apply(&cobalt, &bundle("old", true), 20);
    share(&amber, &ivory);
    share(&cobalt, &ivory);
    share(&cobalt, &amber);
    share(&amber, &cobalt);
    let expected = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    assert!(expected.is_some());
    for store in [&amber, &cobalt, &ivory] {
        assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 30);
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            expected
        );
        let error = direct(store, &bundle("stale", true), "direct-stale").unwrap_err();
        assert_eq!(error.code, "set-managed-subject");
        store
            .connection
            .batched(replay_graph_from_nothing_tx)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            expected
        );
    }
}

#[test]
fn pruning_requires_exact_preview_confirmation_and_retains_ownership() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("true", true), 10);
    let input = bundle("true", false);
    let mut opts = options(&store, 20);
    let preview = store.owned_set_preview(&input, &opts).unwrap();
    assert!(preview.mass_retirement);
    opts.expected_subjects = preview.expected_subjects.clone();
    let before = store.index().unwrap();
    assert_eq!(
        store
            .apply_owned_set(&input, &opts, "retire", "person/operator")
            .unwrap_err()
            .code,
        "mass-retirement-refused"
    );
    assert_eq!(store.index().unwrap(), before);
    opts.confirm_retire = Some(preview.digest);
    store
        .apply_owned_set(&input, &opts, "retire", "person/operator")
        .unwrap();
    let stopped = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|s| s.subject == "agent/garden/meadow")
        .unwrap();
    assert_eq!(stopped.kind, "stop");
    assert!(
        store
            .owned_desired_subjects(std::slice::from_ref(&stopped))
            .unwrap()
            .contains(&stopped.subject)
    );
    assert!(
        store.owned_sets().unwrap()[0]
            .receipt
            .retired
            .contains_key("agent/garden/meadow")
    );
    assert_eq!(
        direct(&store, &bundle("resurrect", true), "resurrect")
            .unwrap_err()
            .code,
        "set-managed-subject"
    );
    assert!(
        store
            .claims_for("agent/garden/meadow", Some("intent.desired"))
            .unwrap()
            .len()
            >= 2
    );
}

#[test]
fn confirmation_is_bound_to_content_and_source() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("initial", true), 10);
    let input = bundle("new", false);
    let mut opts = options(&store, 20);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    opts.confirm_retire = Some(p.digest);
    let different = bundle("different", false);
    opts.expected_subjects = store
        .owned_set_preview(&different, &opts)
        .unwrap()
        .expected_subjects;
    assert_eq!(
        store
            .apply_owned_set(&different, &opts, "different", "person/operator")
            .unwrap_err()
            .code,
        "mass-retirement-refused"
    );
}

#[test]
fn explicit_adoption_keeps_unrelated_declarations_and_atomic_failure_changes_nothing() {
    let store = Store::open_memory("amber").unwrap();
    let input = bundle("initial", true);
    direct(&store, &input, "manual").unwrap();
    let input = bundle("initial", false);
    let mut opts = options(&store, 10);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    assert!(p.blockers.iter().any(|b| b.contains("adoption")));
    opts.adopt.insert("agent/garden/orchard".into());
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(&input, &opts, "adopt", "person/operator")
        .unwrap();
    assert!(
        store.owned_sets().unwrap()[0]
            .receipt
            .adoptions
            .contains_key("agent/garden/orchard")
    );
    let meadow = store.selected_desired_token("agent/garden/meadow").unwrap();
    let mut invalid = bundle("new", true);
    invalid
        .subjects
        .get_mut("agent/garden/meadow")
        .unwrap()
        .kind = "unsupported".into();
    let before = store.index().unwrap();
    assert_eq!(
        store
            .apply_owned_set(&invalid, &options(&store, 20), "invalid", "person/operator")
            .unwrap_err()
            .code,
        "unsupported-set-member"
    );
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(
        store.selected_desired_token("agent/garden/meadow").unwrap(),
        meadow
    );
}

#[test]
fn same_source_retry_is_noop_and_new_source_does_not_relaunch_unchanged_members() {
    let store = Store::open_memory("amber").unwrap();
    let input = bundle("initial", true);
    apply(&store, &input, 10);
    let token = store
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    let before = store.index().unwrap();
    let opts = options(&store, 10);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    assert!(p.noop);
    assert!(
        !store
            .apply_owned_set(&input, &opts, "repeat", "person/operator")
            .unwrap()
            .changed
    );
    assert_eq!(store.index().unwrap(), before);
    apply(&store, &input, 20);
    assert_eq!(
        store
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
    assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 20);
}

#[test]
fn empty_set_needs_both_explicit_empty_and_preview_bound_retirement() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("initial", true), 10);
    let input = crate::graph::parse_owned_set_intent("version 2", "amber").unwrap();
    let mut opts = options(&store, 20);
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    assert_eq!(
        store
            .apply_owned_set(&input, &opts, "empty", "person/operator")
            .unwrap_err()
            .code,
        "empty-owned-set"
    );
    opts.allow_empty = true;
    let p = store.owned_set_preview(&input, &opts).unwrap();
    opts.expected_subjects = p.expected_subjects;
    opts.confirm_retire = Some(p.digest);
    store
        .apply_owned_set(&input, &opts, "empty", "person/operator")
        .unwrap();
    assert!(store.owned_sets().unwrap()[0].receipt.members.is_empty());
}

#[test]
fn mission_omission_retains_active_runs_and_blocks_new_runs_even_on_old_pin() {
    let store = Store::open_memory("amber").unwrap();
    let kdl = "version 2\nmission \"harvest\" state=\"ready\" { goal \"Harvest\"; step \"work\" { assigned-to \"agent/garden/orchard\" } }\nagent \"garden/orchard\" { command \"true\" }";
    let input = parse_intent(kdl, "amber").unwrap();
    apply(&store, &input, 10);
    let revision = input.missions["harvest"].revision.clone();
    let request = crate::model::MissionRunRequest {
        mission: "harvest".into(),
        revision: Some(revision.clone()),
        workspace: "/tmp".into(),
        requester: Some("person/operator".into()),
        mode: Some("run".into()),
        inputs: Default::default(),
        idempotency_key: "active".into(),
    };
    let run = store.create_mission_run(&request).unwrap();
    let input = bundle("true", false);
    let mut opts = options(&store, 20);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    opts.expected_subjects = p.expected_subjects;
    opts.confirm_retire = Some(p.digest);
    store
        .apply_owned_set(&input, &opts, "prune-mission", "person/operator")
        .unwrap();
    assert_eq!(
        store.mission_spec("harvest", None).unwrap().unwrap().state,
        MissionState::Retired
    );
    assert_eq!(
        store
            .mission_spec("harvest", Some(&revision))
            .unwrap()
            .unwrap()
            .state,
        MissionState::Ready
    );
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().status,
        "running"
    );
    let mut request = request;
    request.idempotency_key = "new".into();
    assert_eq!(
        store.create_mission_run(&request).unwrap_err().code,
        "mission-retired"
    );
}

fn signed_fleet() -> Vec<Store> {
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let keys = (0..3)
        .map(|_| Arc::new(crate::fleet::MemberKey::generate().unwrap().0))
        .collect::<Vec<_>>();
    let stores = ["amber", "cobalt", "ivory"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let store = Store::open_memory(*name).unwrap();
            store.bind_fleet(fleet).unwrap();
            store.pin_fleet_anchor(keys[0].public()).unwrap();
            store.set_member_key(Some(keys[i].clone())).unwrap();
            store
        })
        .collect::<Vec<_>>();
    for (i, store) in stores.iter().enumerate() {
        let mut fields = json!({"fleet_id":fleet,"member_key":keys[i].public(),"via":if i==0{"anchor"}else{"invite"},"mode":"listening"});
        if i != 0 {
            fields["sponsor"] = json!("host/amber");
        }
        stores[0]
            .append_claim(&ClaimInput {
                subject: format!("host/{}", store.origin),
                kind: "fleet.member-admitted".into(),
                actor: None,
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: format!("daemon/{}", store.origin),
                kind: "daemon.started".into(),
                actor: None,
                fields: serde_json::from_value(
                    json!({"status":"running","features":{"owned_sets":1}}),
                )
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    for from in &stores {
        for to in &stores {
            if from.origin != to.origin {
                signed_share(from, to);
            }
        }
    }
    stores
}

#[test]
fn rollout_policy_preserves_legacy_hashes_and_requires_every_active_daemon() {
    let stores = signed_fleet();
    let input = bundle("unchanged", false);
    apply(&stores[0], &input, 10);
    let legacy = serde_json::to_value(&stores[0].owned_sets().unwrap()[0].receipt).unwrap();
    assert!(legacy.get("rollout").is_none());
    let restored: owned_sets::Revision = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(
        canonical_hash(&restored).unwrap(),
        canonical_hash(&legacy).unwrap()
    );
    let mut opts = options(&stores[0], 20);
    opts.rollout = Some(crate::rollout::Policy::when_idle(1_800_000, false));
    let blocked = stores[0].owned_set_preview(&input, &opts).unwrap();
    assert_eq!(
        blocked
            .blockers
            .iter()
            .filter(|reason| reason.contains("seat-rollout support"))
            .count(),
        3
    );
    for from in &stores {
        from.append_claim(&ClaimInput {
            subject: format!("daemon/{}", from.origin),
            kind: "daemon.started".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"status":"running","features":{"owned_sets":1,"seat_rollout":1}}),
            )
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
        for to in &stores {
            signed_share(from, to);
        }
    }
    let ready = stores[0].owned_set_preview(&input, &opts).unwrap();
    assert!(ready.blockers.is_empty(), "{:?}", ready.blockers);
    opts.expected_subjects = ready.expected_subjects;
    stores[0]
        .apply_owned_set(&input, &opts, "rollout-policy", "person/operator")
        .unwrap();
    let same_source = options(&stores[0], 20);
    let preview = stores[0].owned_set_preview(&input, &same_source).unwrap();
    assert!(
        !preview.noop,
        "changing policy is not an equal-source retry"
    );
    assert!(
        preview
            .blockers
            .iter()
            .any(|b| b.contains("source sequence"))
    );
}

#[test]
fn rollout_preview_refuses_host_harness_and_authored_session_transitions() {
    let store = Store::open_memory("amber").unwrap();
    let native = |host: &str, harness: &str, args: &str| {
        crate::graph::parse_owned_set_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{ host {host:?}; workspace \".\"; harness {harness:?} {{ {args} }} }}"),"amber").unwrap()
    };
    apply(&store, &native("amber", "claude", "model \"first\";"), 10);
    for (host, harness, args) in [
        ("cobalt", "claude", "model \"second\";"),
        ("amber", "codex", "model \"second\";"),
        ("amber", "claude", "account \"cloud\"; model \"second\";"),
        ("amber", "claude", "args \"--resume\" \"authored-session\";"),
    ] {
        let mut opts = options(&store, 20);
        opts.rollout = Some(crate::rollout::Policy::when_idle(1_800_000, false));
        let preview = store
            .owned_set_preview(&native(host, harness, args), &opts)
            .unwrap();
        assert!(!preview.blockers.is_empty(), "{host} {harness} {args}");
        assert!(
            store
                .apply_owned_set(
                    &native(host, harness, args),
                    &opts,
                    "unsupported",
                    "person/operator"
                )
                .is_err()
        );
        assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 10);
    }
}
fn signed_share(from: &Store, to: &Store) {
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let exchange = from
        .export_replication_exchange_answering(
            fleet,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(&from.origin, fleet, &exchange)
        .unwrap();
    let admission = to.validate_replication_backlog().unwrap();
    assert_eq!(admission.invalid, 0);
    assert_eq!(admission.unknown, 0);
    to.project_replication_backlog().unwrap();
}

#[test]
fn signed_replication_heal_replay_and_checkpoint_keep_source_winner() {
    let stores = signed_fleet();
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    apply(amber, &bundle("initial", true), 10);
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    apply(amber, &bundle("new", true), 30);
    apply(cobalt, &bundle("old", true), 20);
    // The older render arrives last, after all of the newer one's records were admitted.
    signed_share(amber, ivory);
    signed_share(cobalt, ivory);
    signed_share(cobalt, amber);
    signed_share(amber, cobalt);
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    assert!(token.is_some());
    for store in &stores {
        assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 30);
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            token
        );
        store
            .connection
            .batched(replay_graph_from_nothing_tx)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            token
        );
        assert!(
            store.status(Some("agent/garden/orchard")).unwrap().subjects[0]
                .conflicts
                .is_empty()
        );
        let scratch = tempfile::tempdir().unwrap();
        let (_, _, proof) = store
            .plan_checkpoint_through(now_ms() + 1_000, None, scratch.path())
            .unwrap();
        assert!(proof.passed, "{proof:?}");
        let claims = store
            .claims_for("owned-set/garden", Some("owned-set.revised"))
            .unwrap();
        assert_eq!(claims.len(), 3);
        for c in claims {
            assert!(checkpoint_rules::slot_of(&c).is_none());
        }
        for c in store
            .claims_for("agent/garden/orchard", Some("intent.desired"))
            .unwrap()
        {
            assert!(checkpoint_rules::slot_of(&c).is_none());
        }
    }
}

#[test]
fn a_peer_without_set_support_blocks_activation_without_blocking_unmanaged_publication() {
    let stores = signed_fleet();
    let amber = &stores[0];
    let cobalt = &stores[1];
    cobalt
        .append_claim(&ClaimInput {
            subject: "daemon/cobalt".into(),
            kind: "daemon.started".into(),
            actor: None,
            fields: serde_json::from_value(json!({"status":"running","version":"old"})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    signed_share(cobalt, amber);
    let input = bundle("initial", false);
    let opts = options(amber, 10);
    let p = amber.owned_set_preview(&input, &opts).unwrap();
    assert!(p.blockers.iter().any(|b| b.contains("host/cobalt")));
    assert_eq!(
        amber
            .apply_owned_set(&input, &opts, "unsupported", "person/operator")
            .unwrap_err()
            .code,
        "owned-set-refused"
    );
    direct(amber, &input, "manual").unwrap();
}

#[test]
fn stale_independent_claim_written_before_adoption_cannot_override_set_on_heal() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    direct(&amber, &bundle("initial", false), "initial").unwrap();
    share(&amber, &cobalt);
    let input = bundle("managed", false);
    let mut opts = options(&amber, 10);
    opts.adopt.insert("agent/garden/orchard".into());
    opts.expected_subjects = amber
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    amber
        .apply_owned_set(&input, &opts, "adopt", "person/operator")
        .unwrap();
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    direct(&cobalt, &bundle("stale", false), "stale").unwrap();
    share(&cobalt, &amber);
    assert_eq!(
        amber
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
    share(&amber, &cobalt);
    assert_eq!(
        cobalt
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
}

#[test]
fn incomplete_highest_set_holds_member_effects_until_dependencies_arrive() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    apply(&amber, &bundle("initial", false), 10);
    let old = amber.owned_sets().unwrap()[0].clone();
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    direct(&cobalt, &bundle("new", false), "future-member").unwrap();
    let future = cobalt
        .claims_for("agent/garden/orchard", Some("intent.desired"))
        .unwrap()
        .pop()
        .unwrap();
    let mut receipt = old.receipt.clone();
    receipt.previous = Some(format!("{}@{}", old.id, old.revision));
    receipt.source = options(&amber, 30).source;
    let desired: DesiredSubject = serde_json::from_value(future.body.clone()).unwrap();
    receipt
        .members
        .get_mut("agent/garden/orchard")
        .unwrap()
        .claim = future.id.clone();
    receipt
        .members
        .get_mut("agent/garden/orchard")
        .unwrap()
        .revision = desired_revision(&desired);
    amber
        .append_claim(&ClaimInput {
            subject: old.id,
            kind: "owned-set.revised".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"revision":canonical_hash(&receipt).unwrap(),"body":receipt}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(!amber.owned_sets().unwrap()[0].blockers.is_empty());
    let pending = amber.desired_subjects().unwrap();
    assert!(amber.owned_desired_subjects(&pending).unwrap().is_empty());
    assert_pass_candidates_match_effect_guards(&amber, &pending);
    assert_eq!(
        amber
            .owned_member_guard("agent/garden/orchard")
            .unwrap_err()
            .code,
        "owned-set-pending"
    );
    assert_eq!(
        amber
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
    share(&cobalt, &amber);
    assert!(amber.owned_sets().unwrap()[0].blockers.is_empty());
    assert_pass_candidates_match_effect_guards(&amber, &amber.desired_subjects().unwrap());
    assert_eq!(
        amber
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        Some(future.id)
    );
}

#[test]
fn reused_idempotency_key_with_changed_input_is_refused() {
    let store = Store::open_memory("amber").unwrap();
    let input = bundle("initial", false);
    let mut opts = options(&store, 10);
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(&input, &opts, "same-key", "person/operator")
        .unwrap();
    assert_eq!(
        store
            .apply_owned_set(
                &bundle("different", false),
                &opts,
                "same-key",
                "person/operator"
            )
            .unwrap_err()
            .code,
        "idempotency-mismatch"
    );
}

#[test]
fn a_render_prepared_from_a_superseded_set_cannot_overwrite_selected_files() {
    let store = Store::open_memory("amber").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = |content: &str| {
        parse_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{\n workspace {:?}\n command \"true\"\n render {{ file \"configuration.txt\" {content:?} }}\n}}\n",
        workspace.path().display().to_string()), "amber").unwrap()
    };
    let old = source("old");
    apply(&store, &old, 10);
    let stale = &old.subjects["agent/garden/orchard"];
    let candidates = store
        .owned_desired_subjects(std::slice::from_ref(stale))
        .unwrap();
    assert!(candidates.contains(&stale.subject));
    let new = source("new");
    apply(&store, &new, 20);
    let subject = "agent/garden/orchard";
    let selected = &new.subjects[subject];
    assert!(crate::render::apply_all(&store, &[selected], "amber")[subject].is_ok());
    assert!(
        store
            .owned_desired_subjects(std::slice::from_ref(stale))
            .unwrap()
            .is_empty()
    );
    assert!(crate::render::apply_all(&store, &[stale], "amber")[subject].is_err());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("configuration.txt")).unwrap(),
        "new"
    );
}

#[test]
fn a_member_added_only_on_a_losing_partition_retires_through_the_winning_set() {
    let stores = signed_fleet();
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    apply(amber, &bundle("initial", false), 10);
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    apply(cobalt, &bundle("initial", true), 20);
    apply(amber, &bundle("newer", false), 30);
    signed_share(cobalt, ivory);
    signed_share(amber, ivory);
    signed_share(cobalt, amber);
    signed_share(amber, cobalt);
    for store in &stores {
        let stopped = store
            .desired_subject_with_writer("agent/garden/meadow")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(stopped.kind, "stop");
        assert!(store.owned_desired_guard(&stopped).is_ok());
        assert!(
            store
                .owned_desired_subjects(std::slice::from_ref(&stopped))
                .unwrap()
                .contains(&stopped.subject)
        );
        assert_eq!(
            direct(store, &bundle("revive", true), "bypass")
                .unwrap_err()
                .code,
            "set-managed-subject"
        );
        store
            .connection
            .batched(replay_graph_from_nothing_tx)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .desired_subject_with_writer("agent/garden/meadow")
                .unwrap()
                .unwrap()
                .0
                .kind,
            "stop"
        );
        let scratch = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_checkpoint_through(now_ms() + 1_000, None, scratch.path())
                .unwrap()
                .2
                .passed
        );
    }
    // A subsequent apply makes the learned retirement an explicit immutable stop reference.
    let input = bundle("newer", false);
    let mut opts = options(amber, 40);
    let preview = amber.owned_set_preview(&input, &opts).unwrap();
    opts.expected_subjects = preview.expected_subjects;
    opts.confirm_retire = Some(preview.digest);
    amber
        .apply_owned_set(&input, &opts, "learned-retirement", "person/operator")
        .unwrap();
    let view = amber.owned_sets().unwrap().remove(0);
    assert!(view.blockers.is_empty(), "{:?}", view.blockers);
    let retired = &view.receipt.retired["agent/garden/meadow"];
    assert_eq!(
        amber.claim_by_id(&retired.claim).unwrap().unwrap().kind,
        "intent.desired"
    );
}

#[test]
fn equal_source_content_conflicts_hold_effects_until_a_higher_source_resolves_them() {
    let stores = signed_fleet();
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    apply(amber, &bundle("initial", false), 10);
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    apply(amber, &bundle("one", false), 20);
    apply(cobalt, &bundle("two", false), 20);
    signed_share(amber, ivory);
    signed_share(cobalt, ivory);
    assert!(ivory.owned_member_guard("agent/garden/orchard").is_err());
    assert!(!ivory.owned_sets().unwrap()[0].blockers.is_empty());
    let conflicted = ivory.desired_subjects().unwrap();
    assert!(
        ivory
            .owned_desired_subjects(&conflicted)
            .unwrap()
            .is_empty()
    );
    assert_pass_candidates_match_effect_guards(ivory, &conflicted);
    apply(ivory, &bundle("settled", false), 30);
    signed_share(ivory, amber);
    signed_share(ivory, cobalt);
    for store in &stores {
        let view = store.owned_sets().unwrap().remove(0);
        assert_eq!(view.receipt.source.sequence, 30);
        assert!(view.blockers.is_empty());
        assert!(store.owned_member_guard("agent/garden/orchard").is_ok());
        assert_pass_candidates_match_effect_guards(store, &store.desired_subjects().unwrap());
    }
}

#[test]
fn rollout_signed_partition_heal_keeps_the_winning_owner_operation_and_status() {
    let stores = signed_fleet();
    for from in &stores {
        from.append_claim(&ClaimInput {
            subject: format!("daemon/{}", from.origin),
            kind: "daemon.started".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"status":"running","features":{"owned_sets":1,"seat_rollout":1}}),
            )
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
        for to in &stores {
            signed_share(from, to);
        }
    }
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    let native = |model: &str| {
        crate::graph::parse_owned_set_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{ host \"amber\"; workspace \".\"; harness \"claude\" {{ model {model:?}; }} }}"), "amber").unwrap()
    };
    apply(amber, &native("initial"), 10);
    let old = amber
        .desired_subjects_named(&["agent/garden/orchard".into()])
        .unwrap()
        .remove(0)
        .member
        .unwrap();
    amber.append_claim(&ClaimInput {
        subject: "agent/garden/orchard".into(), kind: "runtime.observed".into(), actor: Some("person/operator".into()),
        fields: serde_json::from_value(json!({"status":"running","host":"amber","runtime_id":old.runtime_id,"incarnation_id":"original-one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    let policy = crate::rollout::Policy::when_idle(1_800_000, false);
    for (store, sequence, model) in [(amber, 30, "winner"), (cobalt, 20, "stale")] {
        let mut opts = options(store, sequence);
        opts.rollout = Some(policy.clone());
        let input = native(model);
        let preview = store.owned_set_preview(&input, &opts).unwrap();
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        opts.expected_subjects = preview.expected_subjects;
        store
            .apply_owned_set(
                &input,
                &opts,
                &format!("partition-{sequence}"),
                "person/operator",
            )
            .unwrap();
    }
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap()
        .unwrap();
    let request = amber
        .request_rollout(
            "agent/garden/orchard",
            &token,
            &old,
            "original-one",
            "person/operator",
            &policy,
            "winner-operation",
        )
        .unwrap();
    let operation = amber.rollout("agent/garden/orchard").unwrap().unwrap();
    crate::rollout::phase(
        amber,
        "agent/garden/orchard",
        &operation,
        "held",
        Some("busy at deadline"),
        &["claimed-work".into()],
    )
    .unwrap();
    signed_share(cobalt, ivory);
    signed_share(amber, ivory);
    signed_share(cobalt, amber);
    signed_share(amber, cobalt);
    for store in &stores {
        let selected = store
            .rollout_selection("agent/garden/orchard")
            .unwrap()
            .unwrap();
        let operation = store.rollout("agent/garden/orchard").unwrap().unwrap();
        assert_eq!(selected.source.sequence, 30);
        assert_eq!(operation.id, request.id);
        assert_eq!(operation.phase, "held");
        assert_eq!(operation.old_incarnation, "original-one");
        assert_eq!(
            operation.deadline_unix_ms,
            operation.requested_at_unix_ms + 1_800_000
        );
        assert_eq!(operation.blocking, vec!["claimed-work"]);
    }
}

#[test]
fn manual_rollout_policy_requires_each_active_daemon_and_is_in_the_receipt_digest() {
    let stores = signed_fleet();
    let source = "version 2\nagent \"garden/orchard\" { rollout \"manual\"; harness \"claude\" { model \"example-model\"; } }";
    let input = crate::graph::parse_owned_set_intent(source, "amber").unwrap();
    let opts = options(&stores[0], 1);
    let preview = stores[0].owned_set_preview(&input, &opts).unwrap();
    assert_eq!(
        preview
            .blockers
            .iter()
            .filter(|b| b.contains("manual seat-rollout support"))
            .count(),
        3
    );
    for from in &stores {
        from.append_claim(&ClaimInput {
            subject: format!("daemon/{}", from.origin), kind:"daemon.started".into(), actor:None,
            fields:serde_json::from_value(json!({"status":"running","features":{"owned_sets":1,"seat_rollout":1,"seat_rollout_manual":1}})).unwrap(),
            evidence:vec![],expected_subject:None,idempotency_key:None,
        }).unwrap();
        for to in &stores {
            if from.origin != to.origin {
                signed_share(from, to);
            }
        }
    }
    let automatic =
        crate::graph::parse_owned_set_intent(&source.replace("rollout \"manual\";", ""), "amber")
            .unwrap();
    let automatic_preview = stores[0].owned_set_preview(&automatic, &opts).unwrap();
    let manual_preview = stores[0].owned_set_preview(&input, &opts).unwrap();
    assert!(
        manual_preview.blockers.is_empty(),
        "{:?}",
        manual_preview.blockers
    );
    assert_ne!(manual_preview.digest, automatic_preview.digest);
    apply(&stores[0], &input, 1);
    let receipt = stores[0].owned_sets().unwrap().remove(0).receipt;
    assert_ne!(
        receipt.bundle_digest,
        canonical_hash(&(&automatic.subjects, &automatic.missions)).unwrap()
    );
    assert!(crate::rollout::manual(
        &stores[0]
            .desired_subject_with_writer("agent/garden/orchard")
            .unwrap()
            .unwrap()
            .0
    ));
    let unsupported = crate::graph::parse_owned_set_intent(
        "version 2\nagent \"garden/orchard\" { rollout \"manual\"; command \"true\"; }",
        "amber",
    )
    .unwrap();
    let preview = stores[0]
        .owned_set_preview(&unsupported, &options(&stores[0], 2))
        .unwrap();
    assert!(
        preview
            .blockers
            .iter()
            .any(|b| b.contains("typed native harness")),
        "{:?}",
        preview.blockers
    );
}

#[test]
fn pass_staged_subject_selection_uses_the_partial_index() {
    let store = Store::open_memory("amber").unwrap();
    let connection = store.readers.get();
    let plans = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            super::owned_sets::STAGED_SUBJECTS_QUERY
        ))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plans
            .iter()
            .any(|plan| plan.contains("claims_owned_set_subject_index")),
        "{plans:?}"
    );
}

#[test]
fn the_agent_card_fold_reads_set_owned_declarations_as_the_reduction_does() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("true", true), 10);
    let unmanaged = parse_intent("version 2\nagent \"unmanaged\" { command \"true\" }", "amber")
        .unwrap();
    direct(&store, &unmanaged, "unmanaged").unwrap();
    for subject in ["agent/garden/orchard", "agent/garden/meadow"] {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: serde_json::from_value(json!({"status": "running",
                    "runtime_id": subject, "incarnation_id": "garden-1"}))
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    // The set retires meadow, then a staged ownership change of unmanaged rolls back.
    let retiring = bundle("false", false);
    let mut opts = options(&store, 11);
    let preview = store.owned_set_preview(&retiring, &opts).unwrap();
    opts.expected_subjects = preview.expected_subjects;
    opts.confirm_retire = Some(preview.digest);
    store.apply_owned_set(&retiring, &opts, "set-11", "person/operator").unwrap();
    let desired = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject.ends_with("unmanaged"))
        .unwrap();
    let mut staged = serde_json::to_value(&desired).unwrap();
    staged["owned_set"] = json!("owned-set/garden");
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        append_claim_tx(&transaction, "amber", &desired.subject, "intent.desired",
            Some("person/operator"), &staged, &[], None).unwrap();
        transaction.rollback().unwrap();
    }
    let indexes = (0..=store.index().unwrap()).collect::<Vec<_>>();
    super::card_fold_tests::assert_card_fold_parity(&store, &indexes);
}
