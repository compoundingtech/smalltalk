// Integration regressions for the revisioned agents publication. `api.rs` includes this file
// inside its `tests` module, so the roster fixtures of that module are in scope here.

type PublicationArc = Arc<crate::store::agents_publication::AgentsPublication>;

fn publication_current(store: &Store) -> PublicationArc {
    store.agents_publication().expect("a complete current roster must be published")
}

async fn publication_await_view(
    store: &Store, ready: impl Fn(&PublicationArc) -> bool,
) -> PublicationArc {
    let mut receiver = store.subscribe_agents_publications();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let latest = receiver.borrow_and_update().clone();
            if let Some(latest) = latest && ready(&latest) { return latest; }
            receiver.changed().await.expect("the shared publication owner must stay live");
        }
    }).await.expect("the shared owner must publish without a fresh reader")
}

fn publication_envelope(publication: &PublicationArc) -> Value {
    serde_json::from_slice(publication.encoded()).expect("the encoded publication must be JSON")
}

fn publication_epoch(publication: &PublicationArc) -> String {
    publication_envelope(publication)["publication"]["node_epoch"].as_str()
        .expect("the node epoch must serialize as a string").to_owned()
}

fn publication_row_ids(publication: &PublicationArc) -> Vec<String> {
    publication.rows().iter()
        .map(|row| row["id"].as_str().expect("every row has an id").to_owned())
        .collect()
}

fn publication_row<'a>(publication: &'a PublicationArc, id: &str) -> &'a Value {
    publication.rows().iter().find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("{id} must be in the publication"))
}

/// The encoded body is exactly the complete envelope of the metadata, rows and order.
fn assert_publication_envelope(publication: &PublicationArc) {
    let envelope = publication_envelope(publication);
    assert_eq!(envelope["publication"], serde_json::to_value(publication.metadata()).unwrap());
    assert_eq!(envelope["items"].as_array().unwrap().as_slice(), publication.rows());
    assert_eq!(envelope["order"], json!(publication.order()));
    assert_eq!(envelope["has_more"], false);
    assert_eq!(publication.order(), publication_row_ids(publication).as_slice());
}

/// The current membership and order that the legacy page refs show at the newest cut.
fn publication_expected_order(store: &Store) -> Vec<String> {
    store.read_snapshot(|index| Ok(client_agent_page_refs(store, false, index)?.iter()
        .filter_map(|reference| reference["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>())).unwrap()
}

/// A valid todo snapshot whose task text sets the size of the agent's card.
fn publication_todo(session: &str, contents: &[String], first_in_progress: bool) -> Value {
    let active = usize::from(first_in_progress && !contents.is_empty());
    let tasks = contents.iter().enumerate().map(|(task, content)| json!({
        "content": content,
        "status": if task == 0 && active == 1 { "in_progress" } else { "pending" },
    })).collect::<Vec<_>>();
    json!({
        "harness":"omp", "session_id":session, "incarnation_id":"one",
        "observed_at":"2026-10-03T09:00:00Z", "source_op":"update",
        "phases":[{"name":"Work","tasks":tasks}],
        "totals":{"pending":contents.len() - active,"in_progress":active,"completed":0,
            "blocked":0,"abandoned":0},
        "truncated":false,
    })
}

/// ASCII task text of exactly `len` bytes, so it needs no JSON escapes.
fn publication_task_text(agent: usize, task: usize, len: usize) -> String {
    const FILL: &str = "check the queued change, compare the expected output, record the result and continue ";
    let mut text = format!("Review step {task} of synthetic lane {agent}: ");
    while text.len() < len {
        text.push_str(FILL);
    }
    text.truncate(len);
    text
}

fn publication_task_texts(agent: usize, tasks: usize, total: usize) -> Vec<String> {
    (0..tasks).map(|task| publication_task_text(agent, task,
        (total / tasks + usize::from(task < total % tasks)).max(1))).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_publication_completes_a_roster_over_two_hundred_and_one_mebibyte() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    let count = 401;
    for agent in 0..count {
        let subject = format!("agent/publication-large-{agent:04}");
        roster_resilience_runtime(store, &subject, "old");
        roster_fixture_append(store, &subject, "harness.todo.observed", publication_todo(
            &format!("publication-large-{agent}"), &publication_task_texts(agent, 6, 2_880), false));
    }
    let mut receiver = store.subscribe_agents_publications();
    assert!(receiver.borrow_and_update().is_none(), "nothing is published before a complete roster");
    let seen = Arc::new(parking_lot::Mutex::new(Vec::<(u64, usize)>::new()));
    let tracker = tokio::spawn({
        let seen = Arc::clone(&seen);
        async move {
            while receiver.changed().await.is_ok() {
                let Some(publication) = receiver.borrow_and_update().clone() else { continue };
                seen.lock().push((publication.metadata().revision, publication.rows().len()));
            }
        }
    });
    // More cards are missing than one fold takes, so this refresh assembles the roster in chunks.
    refresh_agent_roster(store, false).unwrap();
    let first = publication_current(store);
    let cut = store.index().unwrap();
    assert_eq!(first.rows().len(), count);
    assert_eq!(first.order(), publication_expected_order(store).as_slice());
    assert_eq!(first.metadata().status_watermark.store_index, cut);
    assert!(first.encoded().len() > 1 << 20,
        "the fixture must exceed one mebibyte, not {} bytes", first.encoded().len());
    assert_publication_envelope(&first);
    assert!(first.rows().iter().all(|row| row["runtime_ids"] == json!(["runtime/old"])));

    // Every card changes: again more than one fold, so chunks follow and one revision results.
    for agent in 0..count {
        roster_resilience_runtime(store, &format!("agent/publication-large-{agent:04}"), "new");
    }
    refresh_agent_roster(store, false).unwrap();
    let second = publication_current(store);
    assert_eq!(second.metadata().revision, first.metadata().revision + 1,
        "chunks inside one refresh must not take revisions");
    assert_eq!(second.metadata().status_watermark.store_index, store.index().unwrap());
    assert_eq!(second.rows().len(), count);
    assert!(second.encoded().len() > 1 << 20);
    assert_publication_envelope(&second);
    assert!(second.rows().iter().all(|row| row["runtime_ids"] == json!(["runtime/new"])));
    assert!(first.rows().iter().all(|row| row["runtime_ids"] == json!(["runtime/old"])),
        "a pinned publication must not change after a newer one");

    tokio::time::sleep(Duration::from_millis(50)).await;
    tracker.abort();
    let seen = seen.lock().clone();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|(_, rows)| *rows == count), "a partial roster was published: {seen:?}");
    assert!(seen.windows(2).all(|pair| pair[0].0 < pair[1].0), "revisions must increase: {seen:?}");
    assert_eq!(seen.last().unwrap().0, second.metadata().revision);
}

#[test]
fn agents_publication_ignores_the_startup_head_and_history_rosters() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    let count = CLIENT_MAX_PAGE_ITEMS + 50;
    for agent in 0..count {
        roster_resilience_runtime(store, &format!("agent/publication-head-{agent:04}"), "old");
    }
    roster_resilience_ended(store);
    store.read_snapshot(|index| client_agent_roster_head(store, index)).unwrap();
    assert!(store.published_agent_roster_head(store.index().unwrap(), 1).is_some(),
        "the fixture must hold a published startup head");
    assert!(store.agents_publication().is_none(), "the startup head is not a complete roster");
    assert!(store.subscribe_agents_publications().borrow().is_none());

    refresh_agent_roster(store, true).unwrap();
    assert!(store.published_agent_roster(store.index().unwrap(), true).is_some());
    assert!(store.agents_publication().is_none(), "a history roster must not be published");

    refresh_agent_roster(store, false).unwrap();
    let current = publication_current(store);
    assert_eq!(current.rows().len(), count);
    assert!(!current.order().iter().any(|id| id == "agent/ended"));
    assert_eq!(current.order(), publication_expected_order(store).as_slice());
    assert_publication_envelope(&current);

    // An unchanged cut with an unchanged presentation keeps the same publication.
    refresh_agent_roster(store, false).unwrap();
    assert!(Arc::ptr_eq(&publication_current(store), &current));

    // A later history refresh does not replace the current publication.
    store.append_claim(&ClaimInput {
        subject: "agent/ended".into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(json!({"status":"stopped", "terminal":true,
            "runtime_id":"ended-again", "incarnation_id":"ended"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    refresh_agent_roster(store, true).unwrap();
    assert!(Arc::ptr_eq(&publication_current(store), &current));
}

#[test]
fn agents_publication_full_forget_clears_it_and_changes_the_epoch() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    roster_resilience_runtime(store, "agent/publication-forget", "old");
    refresh_agent_roster(store, false).unwrap();
    let before = publication_current(store);
    let receiver = store.subscribe_agents_publications();
    assert!(receiver.borrow().as_ref().is_some_and(|latest| Arc::ptr_eq(latest, &before)));

    store.forget_current_views();
    assert!(store.agents_publication().is_none(), "a full forget must clear the publication");
    assert!(receiver.borrow().is_none(), "a full forget must clear the watch value");

    refresh_agent_roster(store, false).unwrap();
    let after = publication_current(store);
    assert_ne!(publication_epoch(&after), publication_epoch(&before),
        "a publication after a full forget must have a new epoch");
    assert_eq!(after.order(), before.order());
    assert_eq!(after.metadata().status_watermark.store_index, store.index().unwrap());
    assert_publication_envelope(&after);
}

#[tokio::test]
async fn agents_publication_rows_stay_frozen_after_presence_and_clock_changes() {
    const SUBJECT: &str = "agent/node.publication-frozen";
    let root = tempfile::tempdir().unwrap();
    let app = state(root.path());
    let store = app.store.as_ref();
    // Date the fixture just inside the 90 s observation freshness, so real time makes it stale.
    let backdated = client_now_ms() - 88_000;
    {
        let _clock = RosterFixtureClock::at(backdated);
        store.set_write_clock_at(backdated).unwrap();
        let source = "version 2\nagent \"publication-frozen\" { command \"true\" }\n";
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let plan = store.mission(&intent, crate::model::IntentInput {
            kdl: source.into(), source_name: None,
        }).unwrap();
        store.apply(&intent, &plan.subject_tokens, "publication-frozen").unwrap();
        for (kind, fields) in [
            ("runtime.observed", json!({"status":"running", "runtime_id":"node.publication-frozen",
                "incarnation_id":"frozen-1"})),
            ("harness.observed", json!({"state":"idle", "driver":"codex",
                "incarnation_id":"frozen-1", "observed_at_ms":backdated as u64})),
        ] {
            store.append_claim(&ClaimInput {
                subject: SUBJECT.into(), kind: kind.into(), actor: Some(SUBJECT.into()),
                fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
                expected_subject: None, idempotency_key: None,
            }).unwrap();
        }
        store.set_write_clock_offset(0).unwrap();
    }
    delivery_presence::record(SUBJECT, r#"{"transport":"app-server","pid":4242}"#);
    let _demand = store.subscribe_agents_publications();
    start_agent_roster(&app);
    let first = publication_await_view(store,
        |publication| publication_row(publication, SUBJECT)["observation"] == "current").await;
    let cut = first.metadata().status_watermark.store_index;
    let frozen_rows = first.rows().to_vec();
    let frozen_body = first.encoded().to_vec();
    let frozen_at = first.metadata().materialized_at_ms;
    let row = publication_row(&first, SUBJECT);
    assert_eq!(row["observation"], "current", "{row}");
    assert_eq!(row["delivery"]["state"], "legacy", "the fixture must show delivery presence: {row}");

    // A presence change at the same graph cut publishes a new presentation.
    delivery_presence::record(SUBJECT,
        r#"{"transport":"app-server","pid":4242,"ready":false,"reason":"The channel stopped."}"#);
    let second = publication_await_view(store,
        |publication| publication_row(publication, SUBJECT)["delivery"]["state"] == "stale").await;
    assert_eq!(store.index().unwrap(), cut);
    assert_eq!(second.metadata().status_watermark.store_index, cut);
    assert!(second.metadata().revision > first.metadata().revision);
    assert_eq!(publication_row(&second, SUBJECT)["delivery"]["state"], "stale");
    assert_eq!(publication_row(&second, SUBJECT)["state"], "waiting");
    assert_publication_envelope(&second);

    // Real time passes the freshness bound: only a newer publication shows that.
    let stale_at = backdated + 90_500;
    let now = client_now_ms();
    if now < stale_at {
        tokio::time::sleep(Duration::from_millis((stale_at - now) as u64)).await;
    }
    let third = publication_await_view(store,
        |publication| publication_row(publication, SUBJECT)["observation"] == "stale").await;
    assert_eq!(third.metadata().status_watermark.store_index, cut);
    assert!(third.metadata().revision > second.metadata().revision);
    assert_eq!(publication_row(&third, SUBJECT)["observation"], "stale");
    assert_eq!(publication_row(&second, SUBJECT)["observation"], "current",
        "a published row must keep the clock it was presented with");
    assert!(third.metadata().materialized_at_ms >= frozen_at);

    assert_eq!(first.rows(), frozen_rows.as_slice(), "published rows must stay frozen");
    assert_eq!(&first.encoded()[..], frozen_body.as_slice(), "the published body must stay frozen");
    assert_eq!(first.metadata().materialized_at_ms, frozen_at);
    assert_eq!(publication_row(&first, SUBJECT)["delivery"]["state"], "legacy");
}

#[test]
fn agents_publication_follows_same_cut_local_activity_export_and_trim() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.store = Arc::new(roster_followup_store());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    let cut = store.index().unwrap();
    refresh_agent_roster(store, false).unwrap();
    let first = publication_current(store);
    assert_eq!(first.metadata().status_watermark.store_index, cut);

    store.append_local_observations_for_test(&[roster_local_timeline()]);
    assert_eq!(store.index().unwrap(), cut, "local activity must keep the graph cut");
    refresh_agent_roster(store, false).unwrap();
    let second = publication_current(store);
    assert_eq!(second.metadata().status_watermark.store_index, cut);
    assert!(second.metadata().status_watermark.local_frontier
        > first.metadata().status_watermark.local_frontier);
    assert!(second.metadata().revision > first.metadata().revision);
    assert_ne!(publication_row(&second, "agent/node.amber")["last_activity_at"],
        publication_row(&first, "agent/node.amber")["last_activity_at"]);
    assert_publication_envelope(&second);

    let mut later = roster_local_timeline();
    later.fields.insert("entry_id".into(), json!("later-local-activity"));
    later.fields.insert("sequence".into(), json!(2));
    store.append_local_observations_for_test(&[later]);
    refresh_agent_roster(store, false).unwrap();
    let third = publication_current(store);
    assert_eq!(third.metadata().status_watermark.store_index, cut);
    assert!(third.metadata().status_watermark.local_frontier
        > second.metadata().status_watermark.local_frontier);
    assert!(third.metadata().revision > second.metadata().revision);

    // Export the local observations, then trim all but the newest of each subject and kind.
    let tail = store.local_observations_tail(usize::MAX).unwrap();
    let exported = crate::store::local_observation_position(tail.last().unwrap())
        .expect("the tail holds local observations");
    store.set_otlp_export_cursor(exported).unwrap();
    assert!(store.trim_local_observations(u128::MAX, 1, 64).unwrap() > 0,
        "the fixture must trim an older local observation");
    assert_eq!(store.index().unwrap(), cut);
    refresh_agent_roster(store, false).unwrap();
    let fourth = publication_current(store);
    assert_eq!(fourth.metadata().status_watermark.store_index, cut);
    let same_source = fourth.metadata().status_watermark.local_frontier
        == third.metadata().status_watermark.local_frontier;
    if same_source && fourth.rows() == third.rows() {
        assert!(Arc::ptr_eq(&fourth, &third), "an identical cut and presentation must reuse the Arc");
    } else {
        assert!(fourth.metadata().revision > third.metadata().revision,
            "a changed watermark or presentation must take a new revision");
    }
    assert_publication_envelope(&fourth);
}

#[tokio::test]
async fn agents_publication_fresh_read_waits_for_the_written_publication() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    roster_resilience_runtime(store, "agent/publication-written", "old");
    refresh_agent_roster(store, false).unwrap();
    let before = publication_current(store);

    roster_resilience_runtime(store, "agent/publication-written", "new");
    let written = store.index().unwrap();
    let read = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery { fresh: true, ..ClientListQuery::default() }));
    tokio::pin!(read);
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut read).await.is_err(),
        "a fresh read must wait for the publication of its write");
    assert!(Arc::ptr_eq(&publication_current(store), &before));
    let reader = Arc::clone(store);
    let worker = tokio::task::spawn_blocking(move || {
        reader.answer_agent_roster_requests(|| refresh_agent_roster(&reader, false))
    });
    let (Extension(snapshot), Json(page)) = tokio::time::timeout(Duration::from_secs(2), read)
        .await.expect("the publication must wake the fresh reader").unwrap();
    worker.await.unwrap().unwrap();
    let after = publication_current(store);
    assert_eq!(after.metadata().status_watermark.store_index, written);
    assert_eq!(snapshot.store_index, written);
    assert!(after.metadata().revision > before.metadata().revision);
    assert_eq!(publication_row(&after, "agent/publication-written")["runtime_ids"],
        json!(["runtime/new"]));
    assert_eq!(publication_row(&before, "agent/publication-written")["runtime_ids"],
        json!(["runtime/old"]));
    let page_ids = page.items.iter().filter_map(|item| item["id"].as_str()).collect::<Vec<_>>();
    assert_eq!(page_ids, after.order().iter().map(String::as_str).collect::<Vec<_>>());
}

#[tokio::test]
async fn agents_publication_failed_refresh_keeps_the_latest_publication() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    roster_resilience_runtime(store, "agent/publication-retained", "old");
    refresh_agent_roster(store, false).unwrap();
    let before = publication_current(store);

    roster_resilience_runtime(store, "agent/publication-retained", "new");
    assert!(store.answer_agent_roster_requests::<()>(|| anyhow::bail!("injected refresh failure")).is_err());
    assert!(Arc::ptr_eq(&publication_current(store), &before),
        "a failed refresh must keep the exact previous publication");
    let (Extension(snapshot), Json(page)) = tokio::time::timeout(Duration::from_millis(200),
        client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
            Query(ClientListQuery { fresh: true, ..ClientListQuery::default() })))
        .await.expect("a failed refresher must not impose the fresh-read wait").unwrap();
    assert_eq!(snapshot.store_index, before.metadata().status_watermark.store_index);
    assert_eq!(page.items[0]["runtime_ids"], json!(["runtime/old"]));
    assert!(Arc::ptr_eq(&publication_current(store), &before));
}

#[tokio::test]
async fn agents_publication_idle_catch_up_and_trim_stay_on_demand() {
    let root = tempfile::tempdir().unwrap();
    let mut app = state(root.path());
    app.store = Arc::new(roster_followup_store());
    let store = app.store.as_ref();
    start_agent_roster(&app);
    // Observe startup without subscribing or reading a legacy list.
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.agents_publication().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let initial = publication_current(store);
    assert!(!store.agents_publication_demand());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let builds = store.agent_resources_builds_for_test();
    for sequence in 1..=3 {
        let mut local = roster_local_timeline();
        local.fields.insert("entry_id".into(), json!(format!("idle-activity-{sequence}")));
        local.fields.insert("sequence".into(), json!(sequence));
        store.append_local_observations_for_test(&[local]);
    }
    roster_resilience_runtime(store, "agent/idle-catch-up", "one");
    let tail = store.local_observations_tail(usize::MAX).unwrap();
    let exported = crate::store::local_observation_position(tail.last().unwrap()).unwrap();
    store.set_otlp_export_cursor(exported).unwrap();
    assert!(store.trim_local_observations(u128::MAX, 1, 64).unwrap() > 0);
    store.forget_current_views();
    assert!(store.agents_publication().is_none());
    delivery_presence::record("agent/idle-catch-up", r#"{"transport":"app-server","pid":4242}"#);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(store.agent_resources_builds_for_test(), builds,
        "idle source commits, presentation notices and full forget must not refold");
    assert!(store.agents_publication().is_none());
    let _demand = store.subscribe_agents_publications();
    let complete = publication_await_view(store, |_| true).await;
    assert_ne!(complete.metadata().node_epoch, initial.metadata().node_epoch);
    assert_eq!(complete.metadata().status_watermark.store_index, store.index().unwrap());
    assert!(complete.order().iter().any(|id| id == "agent/idle-catch-up"));
}


// Measurement: `cargo test -p st3 --lib agents_publication_rate_and_rss_realistic_200_and_400 --
// --ignored --nocapture --test-threads=1`. It reads only this test process.

fn publication_bench_subject(agent: usize) -> String {
    format!("agent/publication-bench-{agent:04}")
}

/// Approximate log-normal row sizes: median 3.9 KB, mean 4.4 KB, largest 15.5 KB at 200 rows.
fn publication_bench_target(agent: usize) -> usize {
    let rank = (agent % 200) * 73 % 200;
    let u = (rank as f64 + 0.5) / 200.0;
    // Tukey-lambda approximation of the standard normal quantile.
    let z = 4.91 * (u.powf(0.14) - (1.0 - u).powf(0.14));
    (3_900.0 * (0.491 * z).exp()).round().clamp(1_200.0, 15_500.0) as usize
}

fn publication_process_memory_kib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").expect("read this measurement process's RSS");
    let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("the measurement requires kernel RSS and peak fields");
    (field("VmRSS:"), field("VmHWM:"))
}

fn publication_allocated_bytes() -> usize {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: glibc's allocator statistics take no pointer and support concurrent callers.
        // This is a live-allocated-byte delta, not RSS; allocator caches can retain freed pages.
        let info = unsafe { libc::mallinfo2() };
        info.uordblks.saturating_add(info.hblkhd)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    panic!("publication retention measurement requires glibc allocator statistics");
}

fn publication_row_bytes(publication: &PublicationArc) -> std::collections::HashMap<String, usize> {
    publication.rows().iter().map(|row| (row["id"].as_str().unwrap().to_owned(),
        serde_json::to_vec(row).unwrap().len())).collect()
}

fn publication_row_distribution(publication: &PublicationArc) -> (usize, f64, usize) {
    let mut sizes = publication_row_bytes(publication).into_values().collect::<Vec<_>>();
    sizes.sort_unstable();
    let median = if sizes.len() % 2 == 0 {
        (sizes[sizes.len() / 2 - 1] + sizes[sizes.len() / 2]) / 2
    } else {
        sizes[sizes.len() / 2]
    };
    let mean = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    (median, mean, *sizes.last().unwrap())
}

/// Ask the actual refresher for a roster and wait until one at or after `cut` is published.
async fn publication_await_cut(store: &Store, cut: u64) -> PublicationArc {
    let mut receiver = store.subscribe_agents_publications();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let latest = receiver.borrow_and_update().clone();
        if let Some(latest) = latest
            && latest.metadata().status_watermark.store_index >= cut
        {
            return latest;
        }
        assert!(Instant::now() < deadline, "no publication reached cut {cut}");
        store.request_fresh_agent_roster(false);
        let _ = tokio::time::timeout(Duration::from_millis(250), receiver.changed()).await;
    }
}

struct PublicationBenchAgent {
    target: usize,
    tasks: usize,
    content: i64,
    texts: Vec<String>,
}

/// Write todos whose task text brings each card to its target size, correcting with the sizes
/// the refresher actually published. Rows within 1% are not written again.
async fn publication_bench_calibrate(
    store: &Store, agents: &mut [PublicationBenchAgent], first: usize,
) -> PublicationArc {
    let mut published = publication_await_cut(store, store.index().unwrap()).await;
    for pass in 0..4 {
        let sizes = publication_row_bytes(&published);
        let mut written = 0;
        for (offset, agent) in agents.iter_mut().enumerate() {
            let number = first + offset;
            let actual = sizes[&publication_bench_subject(number)] as i64;
            let target = agent.target as i64;
            if pass > 0 && (actual - target).abs() * 100 <= target {
                continue;
            }
            if pass == 0 {
                // The todo wrapper is about 420 bytes and each task about 34 bytes.
                agent.content = target - actual - 420 - 34 * agent.tasks as i64;
            } else {
                agent.content += target - actual;
            }
            let total = agent.content.max(agent.tasks as i64) as usize;
            agent.tasks = agent.tasks.max(total.div_ceil(500)).min(100);
            agent.texts = publication_task_texts(number, agent.tasks, total.min(agent.tasks * 512));
            roster_fixture_append(store, &publication_bench_subject(number), "harness.todo.observed",
                publication_todo(&format!("publication-bench-{number}"), &agent.texts, false));
            written += 1;
        }
        if written == 0 {
            break;
        }
        published = publication_await_cut(store, store.index().unwrap()).await;
    }
    published
}

fn publication_bench_seed(store: &Store, agents: std::ops::Range<usize>) -> Vec<PublicationBenchAgent> {
    agents.map(|agent| {
        roster_resilience_runtime(store, &publication_bench_subject(agent), "bench");
        let target = publication_bench_target(agent);
        PublicationBenchAgent { target, tasks: target.div_ceil(480).clamp(1, 100), content: 0, texts: Vec::new() }
    }).collect()
}

fn publication_report_distribution(label: &str, publication: &PublicationArc) {
    let (median, mean, max) = publication_row_distribution(publication);
    println!("agents publication {label}: rows={} row_bytes_median={median} row_bytes_mean={mean:.0} \
        row_bytes_max={max} encoded_bytes={} revision={}",
        publication.rows().len(), publication.encoded().len(), publication.metadata().revision);
    let near = |actual: f64, wanted: f64| (actual - wanted).abs() <= wanted * 0.10;
    assert!(near(median as f64, 3_900.0) && near(mean, 4_400.0) && near(max as f64, 15_500.0),
        "{label}: the row distribution must approximate median 3.9 KB, mean 4.4 KB, max 15.5 KB");
}

struct PublicationPhase {
    label: &'static str,
    started: Instant,
    ended: Instant,
    revisions: u64,
    rss_kib: Vec<u64>,
    fresh_ms: Vec<f64>,
    writes: usize,
}

impl PublicationPhase {
    fn report(&self, observed: &[(Instant, u64, usize, usize)]) -> f64 {
        let seconds = (self.ended - self.started).as_secs_f64();
        let notifications = observed.iter()
            .filter(|(at, ..)| *at >= self.started && *at <= self.ended).count();
        let mut fresh = self.fresh_ms.clone();
        fresh.sort_by(f64::total_cmp);
        let quantile = |q: f64| fresh.get(((fresh.len() as f64 * q) as usize).min(fresh.len().saturating_sub(1)))
            .copied().unwrap_or(0.0);
        let (_, peak) = publication_process_memory_kib();
        let rate = self.revisions as f64 / seconds;
        println!("agents publication phase {}: seconds={seconds:.1} writes={} revisions={} \
            publications_per_s={rate:.2} watch_notifications={notifications} fresh_reads={} \
            fresh_p50_ms={:.1} fresh_p95_ms={:.1} rss_kib_min={} rss_kib_max={} rss_samples={} \
            process_peak_kib={peak}",
            self.label, self.writes, self.revisions, fresh.len(), quantile(0.5), quantile(0.95),
            self.rss_kib.iter().min().copied().unwrap_or(0), self.rss_kib.iter().max().copied().unwrap_or(0),
            self.rss_kib.len());
        rate
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement: run explicitly with --ignored --nocapture --test-threads=1"]
async fn agents_publication_rate_and_rss_realistic_200_and_400() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    // Match the daemon's WAL concurrency; shared-cache memory readers lock out writers.
    state.store = Arc::new(Store::open(&root.path().join("publication.sqlite3"), "node").unwrap());
    let store = Arc::clone(&state.store);
    let (rss_empty, _) = publication_process_memory_kib();
    let mut agents = publication_bench_seed(&store, 0..200);
    let seeded = store.index().unwrap();

    // Every publication the authoritative watch shows, with its time, revision, rows and bytes.
    let observed = Arc::new(parking_lot::Mutex::new(Vec::<(Instant, u64, usize, usize)>::new()));
    let mut receiver = store.subscribe_agents_publications();
    let tracker = tokio::spawn({
        let observed = Arc::clone(&observed);
        async move {
            while receiver.changed().await.is_ok() {
                let Some(latest) = receiver.borrow_and_update().clone() else { continue };
                observed.lock().push((Instant::now(), latest.metadata().revision,
                    latest.rows().len(), latest.encoded().len()));
            }
        }
    });

    // Startup: the actual refresher publishes its startup head, then the complete roster.
    let startup = Instant::now();
    start_agent_roster(&state);
    let first = publication_await_cut(&store, seeded).await;
    let startup_ms = startup.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(first.rows().len(), 200);
    let (rss_startup, peak_startup) = publication_process_memory_kib();
    println!("agents publication startup: rows=200 first_complete_ms={startup_ms:.1} \
        rss_kib_before={rss_empty} rss_kib_after={rss_startup} process_peak_kib={peak_startup} \
        encoded_bytes={}", first.encoded().len());

    // Setup: size every card with todo text, then publish the result.
    let setup = Instant::now();
    let calibrated = publication_bench_calibrate(&store, &mut agents, 0).await;
    assert_eq!(calibrated.rows().len(), 200);
    assert_eq!(calibrated.order(), publication_expected_order(&store).as_slice());
    println!("agents publication setup: seconds={:.1}", setup.elapsed().as_secs_f64());
    publication_report_distribution("200 current agents", &calibrated);
    let (rss_200, peak_200) = publication_process_memory_kib();
    println!("agents publication 200 calibrated: rss_kib_before={rss_empty} \
        rss_kib_after={rss_200} process_peak_kib={peak_200}");
    drop(first);
    drop(calibrated);

    let revision = || store.agents_publication().map_or(0, |latest| latest.metadata().revision);
    let mut phases = Vec::new();

    // Steady quiet: no writes and no reads for 10 s.
    let mut phase = PublicationPhase { label: "steady-quiet", started: Instant::now(),
        ended: Instant::now(), revisions: 0, rss_kib: Vec::new(), fresh_ms: Vec::new(), writes: 0 };
    let start_revision = revision();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        phase.rss_kib.push(publication_process_memory_kib().0);
    }
    phase.ended = Instant::now();
    phase.revisions = revision() - start_revision;
    phases.push(phase);

    let fresh_read = || {
        let state = state.clone();
        async move {
            let started = Instant::now();
            let (Extension(snapshot), _) = tokio::time::timeout(Duration::from_secs(10),
                client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
                    Query(ClientListQuery { fresh: true, ..ClientListQuery::default() })))
                .await.expect("a fresh read must finish").unwrap();
            (snapshot.store_index, started.elapsed().as_secs_f64() * 1000.0)
        }
    };
    let mut turn = 0_usize;
    let mut change = || {
        let agent = turn % agents.len();
        roster_fixture_append(&store, &publication_bench_subject(agent), "harness.todo.observed",
            publication_todo(&format!("publication-bench-{agent}"), &agents[agent].texts,
                (turn / agents.len()) % 2 == 0));
        turn += 1;
        store.index().unwrap()
    };

    // Active paced: one card change and one fresh read per second for 10 s.
    let mut phase = PublicationPhase { label: "active-paced", started: Instant::now(),
        ended: Instant::now(), revisions: 0, rss_kib: Vec::new(), fresh_ms: Vec::new(), writes: 0 };
    let start_revision = revision();
    for _ in 0..10 {
        let tick = Instant::now();
        let written = change();
        phase.writes += 1;
        let (seen, elapsed) = fresh_read().await;
        assert!(seen >= written, "a fresh read must see its own write");
        phase.fresh_ms.push(elapsed);
        phase.rss_kib.push(publication_process_memory_kib().0);
        tokio::time::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed())).await;
    }
    phase.ended = Instant::now();
    phase.revisions = revision() - start_revision;
    phases.push(phase);

    // Active burst: a card change every 20 ms and a fresh read after every fifth, for 10 s.
    let mut phase = PublicationPhase { label: "active-burst-fresh", started: Instant::now(),
        ended: Instant::now(), revisions: 0, rss_kib: Vec::new(), fresh_ms: Vec::new(), writes: 0 };
    let start_revision = revision();
    while phase.started.elapsed() < Duration::from_secs(10) {
        let written = change();
        phase.writes += 1;
        if phase.writes % 5 == 0 {
            let (seen, elapsed) = fresh_read().await;
            assert!(seen >= written, "a fresh read must see its own write");
            phase.fresh_ms.push(elapsed);
            phase.rss_kib.push(publication_process_memory_kib().0);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    phase.ended = Instant::now();
    phase.revisions = revision() - start_revision;
    phases.push(phase);

    // Recovery: quiet again for 5 s.
    let mut phase = PublicationPhase { label: "recovery-quiet", started: Instant::now(),
        ended: Instant::now(), revisions: 0, rss_kib: Vec::new(), fresh_ms: Vec::new(), writes: 0 };
    let start_revision = revision();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        phase.rss_kib.push(publication_process_memory_kib().0);
    }
    phase.ended = Instant::now();
    phase.revisions = revision() - start_revision;
    phases.push(phase);

    let expanded_at = Instant::now();
    let snapshot = observed.lock().clone();
    let rates = phases.iter().map(|phase| (phase.label, phase.report(&snapshot))).collect::<Vec<_>>();
    assert_eq!(phases[0].revisions, 0, "a quiet store must not publish");
    assert!(phases[1].revisions > 0 && phases[2].revisions > 0, "changes must publish");
    for (label, rate) in &rates {
        assert!(*rate <= 11.0, "{label}: {rate:.2} publications/s exceeds the refresher pause bound");
    }
    assert!(snapshot.iter().all(|(_, _, rows, _)| *rows == 200),
        "every publication at 200 agents must be complete");

    let old_subscription = store.subscribe_agents_publications();
    let held_200 = old_subscription.borrow().clone().unwrap();
    let weak_200 = Arc::downgrade(&held_200);
    let rss_before_400 = publication_process_memory_kib().0;
    // 400 current agents: 200 more with the same row-size distribution.
    let mut more = publication_bench_seed(&store, 200..400);
    let expanded = publication_bench_calibrate(&store, &mut more, 200).await;
    assert_eq!(expanded.rows().len(), 400);
    assert_eq!(expanded.order(), publication_expected_order(&store).as_slice());
    publication_report_distribution("400 current agents", &expanded);
    let (rss, peak) = publication_process_memory_kib();
    println!("agents publication 400 setup: seconds={:.1} rss_kib_before={rss_before_400} \
        rss_kib_with_old_200={rss} process_peak_kib={peak}",
        expanded_at.elapsed().as_secs_f64());
    drop(old_subscription);
    let admission = store.admit_agent_resources().await;
    let heap_before = publication_allocated_bytes();
    let retained_200_body = held_200.encoded().len();
    drop(held_200);
    let heap_after = publication_allocated_bytes();
    assert!(weak_200.upgrade().is_none(), "only the subscriber retained its old publication");
    println!("agents publication retained old 200: encoded_bytes={retained_200_body} \
        allocated_bytes_released={} rss_kib_after_release={}",
        heap_before.saturating_sub(heap_after), publication_process_memory_kib().0);
    drop(admission);

    let old_subscription = store.subscribe_agents_publications();
    let held_400 = old_subscription.borrow().clone().unwrap();
    let weak_400 = Arc::downgrade(&held_400);
    drop(expanded);
    let written = change();
    let expanded = publication_await_cut(&store, written).await;
    assert_eq!(expanded.rows().len(), 400);
    drop(old_subscription);
    let admission = store.admit_agent_resources().await;
    let heap_before = publication_allocated_bytes();
    let retained_400_body = held_400.encoded().len();
    let rss_held = publication_process_memory_kib().0;
    drop(held_400);
    let heap_after = publication_allocated_bytes();
    assert!(weak_400.upgrade().is_none(), "the old 400-row publication must be released");
    println!("agents publication retained old 400: encoded_bytes={retained_400_body} \
        allocated_bytes_released={} rss_kib_with_old={rss_held} rss_kib_after_release={}",
        heap_before.saturating_sub(heap_after), publication_process_memory_kib().0);
    drop(admission);

    tracker.abort();
    let observed = observed.lock().clone();
    assert!(observed.windows(2).all(|pair| pair[0].1 < pair[1].1), "revisions must increase");
    println!("agents publication totals: watch_notifications={} last_revision={} \
        (transport and subscriber costs are not measured here)",
        observed.len(), expanded.metadata().revision);
}
