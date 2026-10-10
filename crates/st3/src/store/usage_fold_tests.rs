//! A kept usage fold reads only the usage claims after it, and always answers as a fresh fold.
use super::*;

fn usage(store: &Store, subject: &str, mut fields: Value) {
    fields["driver"] = json!("codex");
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: "harness.usage".into(), actor: Some(subject.into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
}

/// The usage claim for step `n` of a fixed mix of every semantics, incarnation and model.
fn mixed(n: u64) -> Value {
    let incarnation = format!("i{}", n % 3);
    match n % 5 {
        0 => json!({"semantics":"response", "incarnation_id": incarnation, "total_tokens": 10 + n,
            "input_tokens": n, "output_tokens": 3, "cached_tokens": 1, "cost": 0.001 * n as f64,
            "currency": "USD"}),
        1 => json!({"semantics":"response_rollup", "incarnation_id": incarnation,
            "model": format!("m{}", n % 2), "total_tokens": 100 + n, "input_tokens": 50,
            "output_tokens": 50 + n, "cached_tokens": 7}),
        2 => json!({"semantics":"session_cumulative", "incarnation_id": incarnation,
            "total_tokens": 1000 + (n * 7) % 300, "input_tokens": 600, "output_tokens": 400,
            "cost": 0.5}),
        3 => json!({"semantics":"context_occupancy", "incarnation_id": incarnation,
            "context_used_tokens": n, "context_window_tokens": 200000, "model": "m0"}),
        _ => json!({"semantics":"response", "incarnation_id": incarnation, "total_tokens": n}),
    }
}

#[test]
fn kept_usage_folds_answer_as_fresh_folds_through_ties_late_claims_and_older_cuts() {
    let store = Store::open_memory("alder").unwrap();
    let subjects = ["agent/usage-a", "agent/usage-b", "agent/usage-c"].map(str::to_owned);
    let fresh = |subject: &str, at: Option<u64>| store.usage_summary_at(subject, None, at).unwrap();
    let mut clock = 1_000_u128;
    let mut cuts = Vec::new();
    for n in 0..240_u64 {
        // Mostly later; sometimes the same millisecond, and sometimes earlier, as a claim
        // replicated from a member whose clock was behind is.
        clock = match n % 11 {
            3 | 7 => clock,
            5 => clock.saturating_sub(40),
            _ => clock + 1 + u128::from(n % 4),
        };
        store.set_write_clock_at(clock).unwrap();
        usage(&store, &subjects[(n % 3) as usize], mixed(n));
        let kept = store.usage_summaries_at(&subjects, None).unwrap();
        for subject in &subjects {
            assert_eq!(kept.get(subject), fresh(subject, None).as_ref(), "{subject} after claim {n}");
        }
        cuts.push(store.index().unwrap());
        if n % 17 == 0 && n > 0 {
            // A read at an older cut never answers from, or replaces, a newer kept fold.
            let older = cuts[(n / 2) as usize];
            let at = store.usage_summaries_at(&subjects, Some(older)).unwrap();
            for subject in &subjects {
                assert_eq!(at.get(subject), fresh(subject, Some(older)).as_ref(), "{subject} at {older}");
            }
        }
    }
    store.forget_current_views();
    let refolded = store.usage_summaries_at(&subjects, None).unwrap();
    for subject in &subjects {
        assert_eq!(refolded.get(subject), fresh(subject, None).as_ref());
    }
}

#[test]
fn a_kept_usage_fold_reads_only_the_claims_after_it() {
    let store = Store::open_memory("alder").unwrap();
    let subject = "agent/usage-history".to_owned();
    let mut clock = 1_000_u128;
    let mut steps = |history: u64| {
        for n in 0..history {
            clock += 1;
            store.set_write_clock_at(clock).unwrap();
            usage(&store, &subject, mixed(n));
        }
        store.forget_current_views();
        let cold = smallclaims::sqlite::work::SqliteWorkScope::start();
        store.usage_summaries_at(std::slice::from_ref(&subject), None).unwrap();
        let cold = cold.finish();
        clock += 1;
        store.set_write_clock_at(clock).unwrap();
        usage(&store, &subject, mixed(history));
        let warm = smallclaims::sqlite::work::SqliteWorkScope::start();
        let summary = store.usage_summaries_at(std::slice::from_ref(&subject), None).unwrap();
        let warm = warm.finish();
        assert_eq!(summary.get(&subject), store.usage_summary_at(&subject, None, None).unwrap().as_ref());
        (cold.vm_steps, warm.vm_steps, warm.statements)
    };
    let (small_cold, small_warm, small_statements) = steps(20);
    let (large_cold, large_warm, large_statements) = steps(600);
    assert!(large_cold > small_cold * 10, "a cold fold reads the history: {small_cold} vs {large_cold}");
    println!("usage fold VM steps: 20 claims cold={small_cold} warm={small_warm} ({small_statements} statements); \
        600 claims cold={large_cold} warm={large_warm} ({large_statements} statements)");
    assert!(large_warm <= small_warm * 2,
        "a warm fold must not grow with usage history: {small_warm} vs {large_warm} VM steps");
}

#[test]
fn kept_numeric_usage_reads_replaced_context_at_the_same_graph_cut() {
    let store = Store::open_memory("alder").unwrap();
    let subject = "agent/context-cache".to_owned();
    let subjects = std::slice::from_ref(&subject);
    store.set_write_clock_at(1_000).unwrap();
    usage(&store, &subject, json!({"semantics":"session_cumulative", "incarnation_id":"one", "total_tokens":100}));
    usage(&store, &subject, json!({"semantics":"context_occupancy", "incarnation_id":"one", "context_used_tokens":10}));
    let cut = store.index().unwrap();
    let first = store.usage_summaries_at(subjects, None).unwrap();
    assert_eq!(first[&subject].context.as_ref().unwrap().used_tokens, Some(10));
    usage(&store, &subject, json!({"semantics":"context_occupancy", "incarnation_id":"two", "context_used_tokens":20}));
    assert_eq!(store.index().unwrap(), cut);
    let replaced = store.usage_summaries_at(subjects, None).unwrap();
    assert_eq!(replaced[&subject].context.as_ref().unwrap().used_tokens, Some(20));
    assert_eq!(replaced.get(&subject), store.usage_summary_at(&subject, None, None).unwrap().as_ref());
    assert_eq!(replaced[&subject].total_tokens, 100);

    // The register's wall timestamp is newer than this durable claim's acceptance time.
    // It must never advance the cached numeric fold's canonical timestamp frontier.
    store.set_write_clock_at(2_000).unwrap();
    usage(&store, &subject, json!({"semantics":"session_cumulative", "incarnation_id":"one", "total_tokens":150}));
    let resumed = store.usage_summaries_at(subjects, None).unwrap();
    assert_eq!(resumed[&subject].total_tokens, 150);
    assert_eq!(resumed[&subject].context.as_ref().unwrap().used_tokens, Some(20));
    assert_eq!(resumed.get(&subject), store.usage_summary_at(&subject, None, None).unwrap().as_ref());
}

#[test]
fn a_context_only_register_is_visible_without_a_durable_usage_fold() {
    let store = Store::open_memory("alder").unwrap();
    let subject = "agent/context-only".to_owned();
    let subjects = std::slice::from_ref(&subject);
    assert!(store.usage_summaries_at(subjects, None).unwrap().is_empty());
    let cut = store.index().unwrap();
    usage(&store, &subject, json!({"semantics":"context_occupancy", "incarnation_id":"one", "context_used_tokens":0}));
    assert_eq!(store.index().unwrap(), cut);
    let summary = store.usage_summaries_at(subjects, None).unwrap();
    assert_eq!(summary[&subject].total_tokens, 0);
    assert_eq!(summary[&subject].context.as_ref().unwrap().used_tokens, Some(0));
    assert_eq!(summary.get(&subject), store.usage_summary_at(&subject, None, None).unwrap().as_ref());
}

#[test]
fn current_context_overlays_keep_the_fleet_statement_budget_across_batches() {
    let store = Store::open_memory("alder").unwrap();
    let subjects = (0..501).map(|n| format!("agent/context-{n:04}")).collect::<Vec<_>>();
    for (n, subject) in subjects.iter().enumerate() {
        usage(&store, subject, json!({"semantics":"context_occupancy", "incarnation_id":"one", "context_used_tokens":n}));
    }
    let cut = store.index().unwrap();
    let work = smallclaims::sqlite::work::SqliteWorkScope::start();
    let summaries = store.usage_summaries_at(&subjects, Some(cut)).unwrap();
    let work = work.finish();
    for (n, subject) in subjects.iter().enumerate() {
        assert_eq!(summaries[subject].context.as_ref().unwrap().used_tokens, Some(n as u64));
        assert_eq!(summaries[subject].total_tokens, 0);
    }
    assert_eq!(summaries.len(), subjects.len());
    println!("501 current contexts: {} statements, {} VM steps", work.statements, work.vm_steps);
    // This part of a cold roster must fit within its unchanged whole-roster allowance.
    assert!(work.statements as f64 <= 0.33 * subjects.len() as f64, "{work:?}");
    assert_eq!(store.index().unwrap(), cut);
}
