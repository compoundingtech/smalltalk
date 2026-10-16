use super::*;

fn fixture() -> (tempfile::TempDir, Arc<Store>) {
    let directory = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::open(&directory.path().join("claims.sqlite3"), "node").unwrap());
    store
        .append_claim(&smallclaims::ClaimInput {
            subject: "custom/test/clock-anchor".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: Default::default(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    (directory, store)
}

fn notices(
    store: &Store,
) -> (
    smallclaims::sqlite::CommitObserver,
    tokio::sync::watch::Receiver<()>,
) {
    let (sender, receiver) = tokio::sync::watch::channel(());
    let observer = store.observe_commits(move |_| {
        sender.send_replace(());
    });
    (observer, receiver)
}

fn insert(store: &Store, key: &str, deadline: i64) {
    store
        .connection
        .batched(|transaction| {
            transaction.execute(
                "INSERT INTO idempotency(operation_id,response,replay_safe) VALUES (?1,'{}',1)",
                [key],
            )?;
            transaction.execute(
                "UPDATE idempotency SET expires_at_unix_ms=?2 WHERE operation_id=?1",
                rusqlite::params![key, deadline],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap()
        .unwrap();
}

async fn count_becomes(
    store: &Store,
    mut changed: tokio::sync::watch::Receiver<()>,
    expected: u64,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            changed.borrow_and_update();
            let count = store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM idempotency", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap();
            if count == expected {
                return;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("committed response expiry progresses without polling");
}

#[tokio::test]
async fn response_expiry_defers_without_claim_time_and_wakes_on_a_committed_anchor() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("claims.sqlite3"), "node").unwrap());
    insert(&store, "unvalidated", 0);
    let (_observer, changes) = notices(&store);
    let task = spawn_response_expiry(store.clone());
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(store.next_idempotency_expiry().unwrap(), Some(0));
    store
        .append_claim(&smallclaims::ClaimInput {
            subject: "custom/test/clock-anchor".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: Default::default(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    count_becomes(&store, changes, 0).await;
    task.abort();
}

#[tokio::test]
async fn response_expiry_retries_a_busy_writer_and_recovers_after_release() {
    let (directory, store) = fixture();
    store
        .connection
        .write()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    insert(&store, "busy", 0);
    let blocker = rusqlite::Connection::open(directory.path().join("claims.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Both bounded maintenance APIs must retain SQLite's numeric error, including when
    // BEGIN fails before the chunk callback runs.
    for error in [
        store.cleanup_idempotency(now_ms() as i64).unwrap_err(),
        store.migrate_event_payloads().unwrap_err(),
    ] {
        assert!(maintenance_contention(&error));
    }
    let (_observer, changes) = notices(&store);
    let mut attempted = changes.clone();
    let task = spawn_response_expiry(store.clone());
    tokio::time::timeout(Duration::from_secs(5), attempted.changed())
        .await
        .expect("the maintenance writer loan returned after a busy BEGIN")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !task.is_finished(),
        "transient contention must not stop cleanup"
    );
    assert_eq!(store.next_idempotency_expiry().unwrap(), Some(0));
    blocker.execute_batch("ROLLBACK").unwrap();
    count_becomes(&store, changes, 0).await;
    task.abort();
}

#[tokio::test]
async fn response_expiry_first_receipt_wakes_an_empty_scheduler() {
    let (_directory, store) = fixture();
    let (_observer, changes) = notices(&store);
    let task = spawn_response_expiry(store.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;
    insert(&store, "first", 0);
    count_becomes(&store, changes, 0).await;
    task.abort();
}

#[tokio::test]
async fn response_expiry_earlier_commit_interrupts_a_future_deadline() {
    let (_directory, store) = fixture();
    let future = now_ms() as i64 + smallclaims::store::idempotency::RETENTION_MS;
    insert(&store, "future", future);
    let (_observer, changes) = notices(&store);
    let task = spawn_response_expiry(store.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;
    store
        .connection
        .batched(|transaction| {
            transaction.execute(
                "UPDATE idempotency SET expires_at_unix_ms=0 WHERE operation_id='future'",
                [],
            )
        })
        .unwrap()
        .unwrap();
    count_becomes(&store, changes, 0).await;
    assert_eq!(store.next_idempotency_expiry().unwrap(), None);
    task.abort();
}

#[tokio::test]
async fn response_expiry_loan_return_and_rollback_publish_only_authority() {
    let (_directory, store) = fixture();
    let (_observer, changes) = notices(&store);
    let task = spawn_response_expiry(store.clone());
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id,response,replay_safe) VALUES ('rolled-back','{}',1)",
                [],
            )
            .unwrap();
        transaction
            .execute("UPDATE idempotency SET expires_at_unix_ms=0", [])
            .unwrap();
        transaction.rollback().unwrap();
    }
    assert_eq!(store.next_idempotency_expiry().unwrap(), None);
    let mut writer = store.connection.write();
    let transaction = writer.transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO idempotency(operation_id,response,replay_safe) VALUES ('committed','{}',1)",
            [],
        )
        .unwrap();
    transaction
        .execute("UPDATE idempotency SET expires_at_unix_ms=0", [])
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM idempotency", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        1
    );
    drop(writer);
    count_becomes(&store, changes, 0).await;
    task.abort();
}

#[tokio::test]
async fn response_expiry_yields_between_sixty_four_row_commits() {
    let (_directory, store) = fixture();
    let counts = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let observed = counts.clone();
    let _counts = store.observe_commits(move |connection| {
        let count = connection
            .query_row("SELECT COUNT(*) FROM idempotency", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap();
        observed.lock().unwrap().push(count);
    });
    let (_observer, changes) = notices(&store);
    let task = spawn_response_expiry(store.clone());
    store
        .connection
        .batched(|transaction| {
            for index in 0..130 {
                transaction.execute(
                    "INSERT INTO idempotency(operation_id,response,replay_safe) VALUES (?1,'{}',1)",
                    [format!("receipt-{index}")],
                )?;
            }
            transaction.execute("UPDATE idempotency SET expires_at_unix_ms=0", [])?;
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap()
        .unwrap();
    count_becomes(&store, changes, 0).await;
    assert_eq!(*counts.lock().unwrap(), [130, 66, 2, 0]);
    task.abort();
}

#[tokio::test]
async fn response_expiry_schema_read_failure_stops_visibly() {
    let (_directory, store) = fixture();
    store
        .connection
        .write()
        .execute_batch("DROP TABLE idempotency")
        .unwrap();
    let task = spawn_response_expiry(store);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}
