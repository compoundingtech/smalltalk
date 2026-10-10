use super::*;

#[test]
fn agents_window_partial_projection_supplies_its_queue_deadline() {
    let store = Store::open_memory("node").unwrap();
    let entry = |index, history, covered, deadline| runtime::AgentResourcesEntry {
        index, local: 0, history, covered, valid_until_unix_ms: deadline,
        publication_revision: 0,
        publication_items: None,
        items: Arc::new(Vec::new()), published_at_unix_ms: 0,
    };
    let Ok(mut cache) = store.smalltalk.agent_resources_cache.lock() else {
        panic!("fixture cache was poisoned before deadline setup");
    };
    cache.push_back(entry(0, false, None, Some(100)));
    cache.push_back(entry(0, false, Some(BTreeSet::from(["agent/node.one".into()])), Some(200)));
    // Newer history rows and another cut must not replace this window's deadline.
    cache.push_back(entry(0, true, None, Some(300)));
    cache.push_back(entry(1, false, None, Some(400)));
    drop(cache);
    assert_eq!(store.agent_roster_valid_until(0), Some(200));
    assert_eq!(store.agent_roster_valid_until(1), Some(400));
    assert_eq!(store.agent_roster_valid_until(2), None);
    let Ok(mut cache) = store.smalltalk.agent_resources_cache.lock() else {
        panic!("fixture cache was poisoned before clearing the lease deadline");
    };
    cache.push_back(entry(0, false, Some(BTreeSet::new()), None));
    drop(cache);
    assert_eq!(store.agent_roster_valid_until(0), None,
        "a current projection with no live leases must not inherit an older expiry");
}
