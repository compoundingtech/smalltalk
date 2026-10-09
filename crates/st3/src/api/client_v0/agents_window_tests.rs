use super::*;

fn append_runtime(store: &Store, subject: &str, status: &str) {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: "runtime.observed".into(), actor: Some(subject.into()),
        fields: serde_json::from_value(json!({"status":status, "runtime_id":subject,
            "incarnation_id":"window-fixture"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
}

fn fixture(root: &Path) -> AppState {
    let state = tests::test_state_named(root, "node");
    // Display order deliberately disagrees with subject order, including name ties.
    let source = format!("version 2\n{}", (0..137).map(|n| {
        let name = if n % 13 == 0 { "Tied".into() } else { format!("Label {:03}", 136 - n) };
        format!("agent \"seat-{n:03}\" {{ command \"true\"; name \"{name}\" }}\n")
    }).collect::<String>());
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let plan = state.store.mission(&intent, crate::model::IntentInput {
        kdl: source, source_name: None,
    }).unwrap();
    state.store.apply(&intent, &plan.subject_tokens, "window-fixture").unwrap();
    append_runtime(&state.store, "agent/00-history", "stopped");
    append_runtime(&state.store, "agent/01-runtime", "running");
    state
}

fn request(limit: usize, status: Option<&str>) -> CollectionSubscribe {
    serde_json::from_value(json!({"kind":"subscribe", "id":"agents", "collection":"agents",
        "limit":limit, "status":status})).unwrap()
}

fn frame(snapshot: &ClientSnapshot, items: &[Value], has_more: bool) -> Value {
    json!({"kind":"snapshot", "id":"agents", "collection":"agents", "snapshot":snapshot,
        "order":items.iter().map(|item| item["id"].clone()).collect::<Vec<_>>(),
        "items":items, "has_more":has_more})
}

fn old_window(store: &Store, snapshot: &ClientSnapshot, limit: usize, status: Option<&str>) -> (Vec<Value>, bool) {
    let mut items = client_agent_resources_cached(store, false, snapshot.store_index).unwrap();
    if status.is_none() { items.truncate(limit + 1); }
    overlay_agent_resources(store, &mut items, &snapshot.created_at).unwrap();
    if let Some(status) = status { items.retain(|item| item["state"].as_str() == Some(status)); }
    let has_more = items.len() > limit;
    items.truncate(limit);
    (items, has_more)
}

#[tokio::test]
async fn agents_window_first_snapshot_matches_full_json_and_display_order() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let session = ClientSession::local(None).unwrap();
    for limit in [1, 5, 50, 100, 200] {
        state.store.forget_current_views();
        let before = state.store.agent_resources_refolded_cards_for_test();
        let (snapshot, items, has_more) = collection_items(&state, &session, &request(limit, None),
            Arc::new(tokio::sync::Semaphore::new(1)).acquire_owned().await.unwrap()).await.unwrap();
        assert_eq!(state.store.agent_resources_refolded_cards_for_test() - before,
            limit.min(138), "fold only visible cards, not the has_more witness");
        let oracle = Store::open(&root.path().join("graph.db"), "node").unwrap();
        let (expected, expected_more) = oracle.read_snapshot(|_| Ok(old_window(&oracle, &snapshot, limit, None))).unwrap();
        assert_eq!(frame(&snapshot, &items, has_more), frame(&snapshot, &expected, expected_more));
        // Negative control: limiting by subject order would return different rows.
        if limit == 5 {
            let mut wrong = client_agent_resources_cached(&oracle, false, snapshot.store_index).unwrap();
            wrong.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            wrong.truncate(limit);
            assert_ne!(items.iter().map(|item| &item["id"]).collect::<Vec<_>>(),
                wrong.iter().map(|item| &item["id"]).collect::<Vec<_>>());
        }
    }
}

#[tokio::test]
async fn agents_window_status_filters_still_consider_every_live_card() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let session = ClientSession::local(None).unwrap();
    for status in ["running", "desired", "stopped", "failed"] {
        state.store.forget_current_views();
        let (snapshot, items, has_more) = collection_items(&state, &session, &request(5, Some(status)),
            Arc::new(tokio::sync::Semaphore::new(1)).acquire_owned().await.unwrap()).await.unwrap();
        let oracle = Store::open(&root.path().join("graph.db"), "node").unwrap();
        let (expected, expected_more) = oracle.read_snapshot(|_| Ok(old_window(&oracle, &snapshot, 5, Some(status)))).unwrap();
        assert_eq!(frame(&snapshot, &items, has_more), frame(&snapshot, &expected, expected_more));
    }
}

#[tokio::test]
async fn agents_window_reorders_after_new_membership_without_reusing_old_prefix() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let session = ClientSession::local(None).unwrap();
    let windows = collection_windows::Windows::attach(&state.store);
    let query = request(2, None);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let (before, old_items, old_more) = collection_items_with_windows(&state, &session, &query,
        slots.clone().acquire_owned().await.unwrap(), windows.clone()).await.unwrap();
    let (_, warm_items, warm_more) = collection_items_with_windows(&state, &session, &query,
        slots.clone().acquire_owned().await.unwrap(), windows.clone()).await.unwrap();
    assert_eq!(warm_items, old_items);
    assert_eq!(warm_more, old_more);
    append_runtime(&state.store, "agent/000-inserted", "running");
    let (after, items, has_more) = collection_items_with_windows(&state, &session, &query,
        slots.acquire_owned().await.unwrap(), windows).await.unwrap();
    assert!(after.store_index > before.store_index);
    assert_eq!(items[0]["id"], "agent/000-inserted");
    assert_ne!(items, old_items);
    let oracle = Store::open(&root.path().join("graph.db"), "node").unwrap();
    let (expected, expected_more) = oracle.read_snapshot(|_| Ok(old_window(&oracle, &after, 2, None))).unwrap();
    assert_eq!(frame(&after, &items, has_more), frame(&after, &expected, expected_more));
}

#[test]
fn agents_window_cold_vm_work_is_below_the_full_card_path() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let index = state.store.index().unwrap();
    let selected = Store::open(&root.path().join("graph.db"), "node").unwrap();
    let old = Store::open(&root.path().join("graph.db"), "node").unwrap();
    let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    let (selected_cards, selected_more) = selected.read_snapshot(|_| client_agent_window_cards(&selected, index, 6)).unwrap();
    let selected_work = scope.finish();
    assert!(selected_more);
    let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    let mut old_cards = old.read_snapshot(|_| client_agent_resources_cached(&old, false, index)).unwrap();
    let old_work = scope.finish();
    old_cards.truncate(6);
    assert_eq!(selected_cards, old_cards);
    assert_eq!(selected.agent_resources_refolded_cards_for_test(), 6);
    assert!(selected_work.vm_steps < old_work.vm_steps,
        "selected={selected_work:?}, full={old_work:?}");
}

fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied structure when it returns success.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
    let usage = unsafe { usage.assume_init() };
    let ms = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

#[derive(Clone, Copy)]
struct Sample { wall: f64, cpu: f64, statements: u64, vm: u64 }

fn phase<T>(run: impl FnOnce() -> T) -> (T, Sample) {
    let before = cpu_ms();
    let started = Instant::now();
    let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    let value = run();
    let work = scope.finish();
    (value, Sample { wall: started.elapsed().as_secs_f64() * 1000.0,
        cpu: cpu_ms() - before, statements: work.statements, vm: work.vm_steps })
}

fn percentiles(mut values: Vec<f64>) -> (f64, f64) {
    values.sort_by(f64::total_cmp);
    (values[values.len() / 2], values[(values.len() * 95).div_ceil(100) - 1])
}

#[test]
#[ignore = "requires invented ST_COLLECTION_FIXTURE; paired n=7 cold/warm benchmark, run alone"]
fn agents_window_fixture_cold_and_warm_phase_costs() {
    let path = std::path::PathBuf::from(std::env::var_os("ST_COLLECTION_FIXTURE").unwrap());
    let mut samples = BTreeMap::<(bool, &str, &str), Vec<Sample>>::new();
    for round in 0..7 {
        // Alternate order. Both paths use fresh Store instances and the same invented file.
        for selected in if round % 2 == 0 { [false, true] } else { [true, false] } {
            let store = Store::open(&path, "bench-host").unwrap();
            assert_eq!(store.index().unwrap(), 222_212);
            for temperature in ["cold", "warm"] {
                store.read_snapshot(|index| {
                    let (cards, resource) = phase(|| if selected {
                        client_agent_window_cards(&store, index, 100)
                    } else {
                        client_agent_resources_cached(&store, false, index).map(|mut cards| {
                            cards.truncate(101);
                            let has_more = cards.len() > 100;
                            (cards, has_more)
                        })
                    });
                    let (mut cards, has_more) = cards?;
                    let (overlay_result, overlay) = phase(|| overlay_agent_resources(&store, &mut cards, "cut"));
                    overlay_result?;
                    cards.truncate(100);
                    let (_, serialization) = phase(|| serde_json::to_vec(&json!({
                        "kind":"snapshot", "collection":"agents", "order":cards.iter().map(|c| &c["id"]).collect::<Vec<_>>(),
                        "items":cards, "has_more":has_more,
                    })).unwrap());
                    for (name, sample) in [("graph-derived cards", resource), ("live overlay", overlay),
                        ("serialization", serialization)] {
                        samples.entry((selected, temperature, name)).or_default().push(sample);
                    }
                    Ok(())
                }).unwrap();
            }
        }
    }
    for ((selected, temperature, name), values) in samples {
        let (wall_median, wall_p95) = percentiles(values.iter().map(|v| v.wall).collect());
        let (cpu_median, cpu_p95) = percentiles(values.iter().map(|v| v.cpu).collect());
        let (statements_median, statements_p95) = percentiles(values.iter().map(|v| v.statements as f64).collect());
        let (vm_median, vm_p95) = percentiles(values.iter().map(|v| v.vm as f64).collect());
        println!("path={} cache={temperature} phase={name} n=7 wall_median_ms={wall_median:.3} wall_p95_ms={wall_p95:.3} cpu_median_ms={cpu_median:.3} cpu_p95_ms={cpu_p95:.3} statements_median={statements_median:.0} statements_p95={statements_p95:.0} vm_median={vm_median:.0} vm_p95={vm_p95:.0}",
            if selected { "selected" } else { "full" });
    }
}
