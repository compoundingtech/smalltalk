use super::*;

const SUBJECT: &str = "agent/node.fresh-proof";

fn unrelated(store: &Store) {
    store
        .append_claim(&ClaimInput {
            subject: "custom/fresh-proof/marker".into(),
            kind: "custom.test.marker".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn fixture(history: bool) -> (Store, u64, Arc<Vec<Value>>, u128) {
    let store = Store::open_memory("node").unwrap();
    let _wake = store.start_agent_roster_refresher().unwrap();
    unrelated(&store);
    let cut = store.index().unwrap();
    // Private immutable cache rows isolate proof guards; real card production is covered by
    // api::tests::roster_fresh. This is not a legal lease/expiry or admission-history fixture.
    store
        .read_snapshot(|index| {
            store
                .cached_agent_resources_for(index, history, None, |_| {
                    Ok(vec![json!({"id":SUBJECT,"name":"fresh-proof"})])
                })
                .map(drop)
        })
        .unwrap();
    let (_, cards, published_at) = store.published_agent_roster(cut, history).unwrap();
    (store, cut, cards, published_at)
}

#[test]
fn exact_served_proof_rejects_other_identity_mode_bounds_and_eviction() {
    for history in [false, true] {
        let (store, cut, cards, published_at) = fixture(history);
        unrelated(&store);
        let through = store.index().unwrap();
        assert!(cut < through);
        let writes = smallclaims::touched::writes();
        assert!(
            store
                .served_agent_roster_unchanged_through(through, history, cut, published_at, &cards)
                .unwrap()
        );
        assert!(
            store
                .published_agent_roster_unchanged_through(through, history)
                .unwrap()
        );
        let other = Arc::new((*cards).clone());
        assert!(
            !store
                .served_agent_roster_unchanged_through(through, history, cut, published_at, &other)
                .unwrap()
        );
        assert!(
            !store
                .served_agent_roster_unchanged_through(through, !history, cut, published_at, &cards)
                .unwrap()
        );
        assert!(
            !store
                .served_agent_roster_unchanged_through(
                    through,
                    history,
                    cut,
                    published_at + 1,
                    &cards
                )
                .unwrap()
        );
        assert!(
            !store
                .served_agent_roster_unchanged_through(cut - 1, history, cut, published_at, &cards)
                .unwrap()
        );
        assert!(
            !store
                .served_agent_roster_unchanged_through(
                    through + 1,
                    history,
                    cut,
                    published_at,
                    &cards
                )
                .unwrap()
        );
        assert_eq!(smallclaims::touched::writes(), writes);
        let replacement = {
            let mut cache = store.smalltalk.agent_resources_cache.lock().unwrap();
            let mut replacement = cache.back().unwrap().clone();
            replacement.items = Arc::new((*cards).clone());
            replacement.published_at_unix_ms += 1;
            cache.clear();
            replacement
        };
        assert!(
            !store
                .served_agent_roster_unchanged_through(through, history, cut, published_at, &cards)
                .unwrap()
        );
        store
            .smalltalk
            .agent_resources_cache
            .lock()
            .unwrap()
            .push_back(replacement);
        assert!(
            store
                .published_agent_roster_unchanged_through(through, history)
                .unwrap()
        );
        assert!(
            !store
                .served_agent_roster_unchanged_through(through, history, cut, published_at, &cards)
                .unwrap(),
            "a valid replacement cannot certify the evicted served image"
        );
    }
}

#[test]
fn exact_served_proof_rejects_expiry_partial_coverage_and_pending_faults() {
    for history in [false, true] {
        let (store, cut, cards, published_at) = fixture(history);
        assert!(
            store
                .served_agent_roster_unchanged_through(cut, history, cut, published_at, &cards)
                .unwrap()
        );
        {
            let mut cache = store.smalltalk.agent_resources_cache.lock().unwrap();
            cache.back_mut().unwrap().valid_until_unix_ms = Some(now_ms());
        }
        assert!(
            !store
                .served_agent_roster_unchanged_through(cut, history, cut, published_at, &cards)
                .unwrap()
        );
        assert!(
            !store
                .published_agent_roster_unchanged_through(cut, history)
                .unwrap()
        );
        {
            let mut cache = store.smalltalk.agent_resources_cache.lock().unwrap();
            cache.back_mut().unwrap().valid_until_unix_ms = None;
            cache.back_mut().unwrap().covered = Some(BTreeSet::from([SUBJECT.into()]));
        }
        assert!(
            !store
                .served_agent_roster_unchanged_through(cut, history, cut, published_at, &cards)
                .unwrap()
        );
        store
            .smalltalk
            .agent_resources_cache
            .lock()
            .unwrap()
            .back_mut()
            .unwrap()
            .covered = None;
        store
            .smalltalk
            .agent_roster_fault_retries
            .lock()
            .unwrap()
            .pending[usize::from(history)]
        .insert(SUBJECT.into());
        assert!(
            !store
                .served_agent_roster_unchanged_through(cut, history, cut, published_at, &cards)
                .unwrap()
        );
        assert!(
            !store
                .published_agent_roster_unchanged_through(cut, history)
                .unwrap()
        );
    }
}

#[test]
fn exact_served_proof_rejects_a_local_timeline_change_at_the_same_graph_cut() {
    for history in [false, true] {
        let (store, cut, cards, published_at) = fixture(history);
        assert!(
            store
                .served_agent_roster_unchanged_through(cut, history, cut, published_at, &cards)
                .unwrap()
        );
        store.append_local_observations_for_test(&[ClaimInput {
            subject: SUBJECT.into(),
            kind: "harness.timeline".into(),
            actor: Some(SUBJECT.into()),
            fields: serde_json::from_value(json!({
                "operation":"append", "entry_id":"proof-local", "source_id":"fixture/proof-local",
                "sequence":1, "revision":1, "role":"assistant", "entry_type":"content",
                "final":true, "driver":"codex", "incarnation_id":"proof-one",
                "observed_at_unix_ms":1_900_000_000_000_u64, "body":{"text":"local activity"}
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }]);
        assert_eq!(store.index().unwrap(), cut);
        assert!(
            !store
                .served_agent_roster_unchanged_through(cut, history, cut, published_at, &cards)
                .unwrap()
        );
        assert!(
            !store
                .published_agent_roster_unchanged_through(cut, history)
                .unwrap()
        );
    }
}
