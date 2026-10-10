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
fn kept_usage_folds_stay_within_their_weight() {
    let fold = |series: usize| {
        let mut spend = BTreeMap::new();
        spend.insert("i0".to_owned(), UsageSpend {
            rollups: (0..series).map(|n| (format!("series-{n}"), (1, 1, 0, 0, 0))).collect(),
            ..UsageSpend::default()
        });
        CachedUsageFold { through: 1, last_accepted: "1".into(),
            fold: UsageFold { spend, context: None, saw: true } }
    };
    let mut folds = UsageFolds::default();
    // One agent with too many rollup series is folded afresh on each read, never kept.
    folds.keep("agent/heavy".into(), fold(USAGE_FOLD_WEIGHT / 16 + 1));
    assert!(folds.get("agent/heavy").is_none());
    let each = fold(1000).fold.weight();
    let fit = USAGE_FOLD_WEIGHT / each;
    for n in 0..fit {
        folds.keep(format!("agent/{n}"), fold(1000));
    }
    assert_eq!(folds.weight, fit * each);
    // Replacing a kept fold re-counts its weight instead of adding to it.
    folds.keep("agent/0".into(), fold(1000));
    assert_eq!(folds.weight, fit * each);
    // Past the budget the kept folds start over.
    folds.keep("agent/one-more".into(), fold(1000));
    assert_eq!(folds.weight, each);
    assert!(folds.get("agent/one-more").is_some() && folds.get("agent/0").is_none());
}
