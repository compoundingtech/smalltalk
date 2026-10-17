use super::*;
#[test]
fn quiet_node_retry_coalesces_and_exhausts_without_self_rearming() {
    let now = tokio::time::Instant::now();
    let mut retry = ProjectionContentionRetry::default();
    assert_eq!(retry.poll(now, 1, true), None);
    // Repeated receives do not push the first deadline back.
    assert_eq!(retry.poll(now + Duration::from_secs(10), 50, true), None);
    assert_eq!(retry.poll(now + Duration::from_secs(30), 50, true), Some(1));
    retry.finish(now + Duration::from_secs(30), 51, true);
    assert_eq!(retry.poll(now + Duration::from_secs(90), 51, true), Some(2));
    retry.finish(now + Duration::from_secs(90), 52, true);
    assert_eq!(
        retry.poll(now + Duration::from_secs(210), 52, true),
        Some(3)
    );
    retry.finish(now + Duration::from_secs(210), 53, true);
    assert_eq!(retry.poll(now + Duration::from_secs(500), 53, true), None);
    // Only a fresh external contention generation rearms an exhausted episode.
    assert_eq!(retry.poll(now + Duration::from_secs(500), 54, true), None);
    assert_eq!(
        retry.poll(now + Duration::from_secs(530), 54, true),
        Some(1)
    );
}
#[test]
fn recovered_or_nonbusy_work_cancels_retry_and_no_contention_means_no_schedule() {
    let now = tokio::time::Instant::now();
    let mut retry = ProjectionContentionRetry::default();
    assert_eq!(retry.poll(now, 0, true), None);
    assert_eq!(retry.poll(now, 1, true), None);
    assert_eq!(retry.poll(now + Duration::from_secs(30), 1, true), Some(1));
    retry.finish(now + Duration::from_secs(30), 1, false);
    assert_eq!(retry.poll(now + Duration::from_secs(500), 1, true), None);
    retry.poll(now + Duration::from_secs(500), 2, true);
    assert_eq!(retry.poll(now + Duration::from_secs(501), 2, false), None);
    assert_eq!(retry.poll(now + Duration::from_secs(550), 2, true), None);
}
#[test]
fn external_contention_racing_success_is_not_lost() {
    let now = tokio::time::Instant::now();
    let mut retry = ProjectionContentionRetry::default();
    retry.poll(now, 1, true);
    assert_eq!(retry.poll(now + Duration::from_secs(30), 1, true), Some(1));
    // The successful call reports its pre-call watermark, not a newer external busy.
    retry.finish(now + Duration::from_secs(30), 1, false);
    assert_eq!(retry.poll(now + Duration::from_secs(31), 2, true), None);
    assert_eq!(retry.poll(now + Duration::from_secs(61), 2, true), Some(1));
}
#[tokio::test(start_paused = true)]
async fn quiet_node_recovers_automatically_after_busy_writer_releases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("source").unwrap();
    source
        .append_client_claim(&smallclaims::ClaimInput {
            subject: "resource/quiet-busy".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([("kind".into(), json!("custom.test.replication"))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    source.bind_fleet(FLEET).unwrap();
    let store = Arc::new(Store::open(&path, "node").unwrap());
    store.bind_fleet(FLEET).unwrap();
    store.project_replication_backlog().unwrap();
    let frontier =
        || {
            store.readers.get().query_row(
        "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
        [], |row| row.get::<_, u64>(0)).unwrap()
        };
    let before = frontier();
    let inventory = store.export_replication_summary(FLEET).unwrap().inventory;
    let exchange = source
        .export_replication_exchange(FLEET, &inventory)
        .unwrap();
    store
        .receive_replication_exchange("source", FLEET, &exchange)
        .unwrap();
    assert!(store.validate_replication_backlog().unwrap().changed);
    let target = store.index().unwrap();
    assert!(target > before);
    assert_eq!(frontier(), before);
    let visible = || {
        store.readers.get().query_row(
        "SELECT COUNT(*) FROM events WHERE subject='resource/quiet-busy' AND kind='resource.observed'",
        [], |row| row.get::<_, i64>(0)).unwrap()
    };
    assert_eq!(visible(), 0);
    store
        .connection
        .write()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.project_replication_backlog().is_err());
    assert!(store.replication_projection_deferred());
    assert_eq!(frontier(), before);
    other.execute_batch("ROLLBACK").unwrap();
    // No receive, wake or manual projection follows this release: the real worker
    // must publish the admitted claim and its projected event by itself.
    let (events, mut received) = watch::channel(0u64);
    let (polls, mut polled) = watch::channel(false);
    let worker = retry_projection_contention_observed(
        Arc::downgrade(&store),
        Arc::new(Notify::new()),
        events,
        dir.path().to_owned(),
        move |backoff| { polls.send_replace(backoff); },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    loop { let backoff = *polled.borrow_and_update(); if backoff { break; } polled.changed().await.unwrap(); }
    tokio::time::advance(Duration::from_secs(30)).await;
    let observed = store.clone();
    let recovered = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while observed.replication_projection_deferred() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        !observed.replication_projection_deferred()
    })
    .await
    .unwrap();
    if recovered {
        received.changed().await.unwrap();
    }
    worker.abort();
    assert!(
        recovered,
        "scheduled retry must finish without a receive/wake"
    );
    assert_eq!(*received.borrow(), 1);
    assert_eq!(frontier(), target);
    assert_eq!(visible(), 1);
}
#[tokio::test(start_paused = true)]
async fn retry_worker_does_not_keep_store_alive_during_scheduled_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap());
    store.project_replication_backlog().unwrap();
    store
        .connection
        .write()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let other = rusqlite::Connection::open(dir.path().join("claims.sqlite3")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.project_replication_backlog().is_err());
    other.execute_batch("ROLLBACK").unwrap();
    let weak = Arc::downgrade(&store);
    let (events, _) = watch::channel(0u64);
    let (polls, mut polled) = watch::channel(false);
    let worker = retry_projection_contention_observed(
        weak.clone(),
        Arc::new(Notify::new()),
        events,
        dir.path().to_owned(),
        move |backoff| { polls.send_replace(backoff); },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    loop { let backoff = *polled.borrow_and_update(); if backoff { break; } polled.changed().await.unwrap(); }
    // The worker has observed contention and waits for its first 30s deadline.
    assert_eq!(weak.strong_count(), 1);
    drop(store);
    assert!(weak.upgrade().is_none());
    worker.abort();
}
#[test]
fn only_the_terminal_provider_refusal_counts_as_an_ended_provider() {
    assert!(provider_ended_refusal(
        "st API returned 422 stale-mailbox-session: the provider authority is terminal"
    ));
    for other in [
        "st API returned 422 stale-mailbox-session: the provider session was superseded",
        "st API returned 422 mailbox-session-starting: waiting for provider ownership",
        "st API returned 500 internal: the provider authority is terminal",
    ] {
        assert!(!provider_ended_refusal(other), "{other}");
    }
}
#[tokio::test(start_paused = true)]
async fn a_provider_task_that_has_returned_is_settled_and_one_that_has_not_is_not() {
    let done = tokio::spawn(async { Ok(()) });
    assert!(provider_task_settled(&done).await);
    let running = tokio::spawn(std::future::pending::<Result<()>>());
    assert!(!provider_task_settled(&running).await);
    running.abort();
}
