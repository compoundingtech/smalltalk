use super::*;
use std::time::Instant;

fn append_fixture_claim(
    store: &Store,
    subject: &str,
    kind: &str,
    actor: &str,
    fields: BTreeMap<String, Value>,
) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some(actor.into()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn send_fixture_message(store: &Store, index: usize, seed: usize) -> String {
    let subject = format!("message/sql-{seed}-{index:05}");
    let from = if index % 11 == 0 {
        "agent/sql-narrow"
    } else if index % 3 == 0 {
        "person/sql-alex"
    } else {
        "agent/sql-sender"
    };
    let to = match (index + seed) % 4 {
        0 => "person/sql-alex",
        1 => "person/sql-robin",
        2 => "agent/sql-narrow",
        _ => "agent/sql-recipient",
    };
    let mut fields = BTreeMap::from([
        ("from".into(), json!(from)),
        ("to".into(), json!(to)),
        ("content".into(), json!(format!("body {seed}/{index}: λ\nsecond line"))),
        ("status".into(), json!("sent")),
    ]);
    if index % 2 == 0 {
        fields.insert("title".into(), json!(format!("title {index}")));
        fields.insert("session_id".into(), json!(format!("session/{seed}/{index}")));
    }
    if index % 3 == 0 {
        fields.insert("in_reply_to".into(), json!("message/sql-parent"));
    }
    let tags = if index % 5 == 0 {
        vec![format!("reminder:sql-{}", index % 3), format!("version:{}", index % 4)]
    } else {
        vec![format!("fixture:{seed}"), "unicode:λ".into()]
    };
    fields.insert("tags".into(), json!(tags));
    if index % 7 == 0 {
        fields.insert("attachments".into(), json!([{
            "sha256": "ab".repeat(32), "media_type": "text/plain",
            "name": "notes λ.txt", "size": 42, "origin": "host/sql-fixture"
        }]));
    }
    append_fixture_claim(store, &subject, "message.sent", from, fields);
    // Different lifecycle depths exercise closed/read/staged as well as sent messages.
    for (kind, status) in [
        ("message.staged", "staged"),
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ]
    .into_iter()
    .take((index + seed) % 5)
    {
        append_fixture_claim(
            store,
            &subject,
            kind,
            to,
            BTreeMap::from([("status".into(), json!(status))]),
        );
    }
    subject
}

fn generated_fixture(store: &Store, count: usize, seed: usize) {
    for index in 0..count {
        send_fixture_message(store, index, seed);
        // Unrelated claims must not change message selection or metadata.
        append_fixture_claim(
            store,
            &format!("doc/sql-{seed}-{index}"),
            "publication.operation",
            "agent/sql-sender",
            BTreeMap::from([
                ("operation".into(), json!(format!("fixture/{seed}/{index}"))),
                ("action".into(), json!("fixture-unrelated")),
                ("status".into(), json!("accepted")),
            ]),
        );
    }
}

async fn collect_sql_pages(
    state: &AppState,
    query: ClientListQuery,
    request_time: u128,
) -> Vec<Value> {
    let snapshot = new_client_snapshot(state);
    let mut continuation = query.clone();
    let mut items = Vec::new();
    loop {
        let (Extension(returned_snapshot), Json(page)) = client_messages_sql_page_at(
            state,
            snapshot.clone(),
            &continuation,
            request_time,
        )
        .await
        .unwrap();
        assert_eq!(returned_snapshot.id, snapshot.id);
        assert_eq!(returned_snapshot.store_index, snapshot.store_index);
        assert_eq!(page.collection, "messages");
        assert_eq!(page.filters, client_page_filters("messages", &query));
        assert!(page.items.len() <= page.page.limit);
        assert_eq!(page.page.has_more, page.page.next_cursor.is_some());
        let cursor = page.page.next_cursor;
        if cursor.is_some() {
            assert!(!page.items.is_empty(), "a continuation must make progress");
        }
        items.extend(page.items);
        let Some(cursor) = cursor else { break };
        continuation.cursor = Some(cursor);
    }
    items
}

#[tokio::test]
async fn sql_message_pages_match_full_oracle_for_generated_filter_matrix() {
    for seed in [0, 3] {
        let root = tempfile::tempdir().unwrap();
        let state = super::tests::state(root.path());
        generated_fixture(&state.store, 41, seed);
        for history in [false, true] {
            for person in [None, Some("person/sql-alex"), Some("person/sql-absent")] {
                for actor in [None, Some("agent/sql-narrow"), Some("person/sql-robin")] {
                    for limit in [Some(1), Some(7), Some(999), None] {
                        let query = ClientListQuery {
                            history,
                            person: person.map(str::to_owned),
                            actor: actor.map(str::to_owned),
                            limit,
                            ..Default::default()
                        };
                        let now = client_now_ms();
                        let expected = client_message_resources_at(
                            &state.store, person, history, actor, now,
                        ).unwrap();
                        let actual = collect_sql_pages(&state, query, now).await;
                        assert_eq!(actual, expected,
                            "seed={seed}, history={history}, person={person:?}, actor={actor:?}, limit={limit:?}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn sql_message_pages_preserve_snapshot_fields_and_delivery_age_after_writes() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    generated_fixture(&state.store, 29, 0);
    let query = ClientListQuery { history: true, limit: Some(3), ..Default::default() };
    let now = client_now_ms();
    let expected = client_message_resources_at(&state.store, None, true, None, now).unwrap();
    let snapshot = new_client_snapshot(&state);
    let (Extension(first_snapshot), Json(first)) =
        client_messages_sql_page_at(&state, snapshot.clone(), &query, now).await.unwrap();
    let first_cursor = first.page.next_cursor.clone().expect("fixture spans pages");
    let mut actual = first.items;
    // Change an existing row still ahead of the cursor, add new mail, and supersede
    // reminder versions. Neither metadata nor selection may leak into this traversal.
    append_fixture_claim(
        &state.store, "message/sql-0-00000", "message.staged", "person/sql-alex",
        BTreeMap::from([("status".into(), json!("staged"))]),
    );
    send_fixture_message(&state.store, 115, 0);
    send_fixture_message(&state.store, 101, 0);
    let fresh_snapshot = new_client_snapshot(&state);
    assert!(fresh_snapshot.store_index > snapshot.store_index);
    let mut continuation = ClientListQuery { cursor: Some(first_cursor), ..query };
    loop {
        let (Extension(returned_snapshot), Json(page)) = client_messages_sql_page_at(
            &state, first_snapshot.clone(), &continuation, now + 60_000,
        ).await.unwrap();
        assert_eq!(returned_snapshot.id, first_snapshot.id);
        assert_eq!(returned_snapshot.store_index, first_snapshot.store_index);
        actual.extend(page.items);
        let Some(cursor) = page.page.next_cursor else { break };
        continuation.cursor = Some(cursor);
    }
    assert_eq!(actual, expected);
    // Replaying a cursor returns exactly the original next page, age included.
    let replay_query = ClientListQuery {
        history: true, limit: Some(3),
        cursor: first.page.next_cursor,
        ..Default::default()
    };
    let (_, Json(replayed)) = client_messages_sql_page_at(
        &state, first_snapshot, &replay_query, now + 120_000,
    ).await.unwrap();
    assert_eq!(replayed.items, expected[3..6]);
}

#[tokio::test]
async fn sql_message_cursors_reject_changed_filters_and_limits() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    generated_fixture(&state.store, 23, 0);
    let query = ClientListQuery { history: true, limit: Some(1), ..Default::default() };
    let snapshot = new_client_snapshot(&state);
    let (_, Json(first)) = client_messages_sql_page(&state, snapshot.clone(), &query).await.unwrap();
    let cursor = first.page.next_cursor.expect("fixture spans pages");
    for changed in [
        ClientListQuery { history: false, ..query.clone() },
        ClientListQuery { person: Some("person/sql-alex".into()), ..query.clone() },
        ClientListQuery { actor: Some("agent/sql-narrow".into()), ..query.clone() },
        ClientListQuery { limit: Some(2), ..query.clone() },
    ] {
        let changed = ClientListQuery { cursor: Some(cursor.clone()), ..changed };
        let error = client_messages_sql_page(&state, snapshot.clone(), &changed)
            .await.err().expect("changed query must not reuse a cursor");
        assert_eq!(error.status, StatusCode::GONE);
    }
    let malformed = ClientListQuery { cursor: Some("not-a-cursor".into()), ..query };
    assert!(client_messages_sql_page(&state, snapshot, &malformed).await.is_err());
}

#[tokio::test]
async fn sql_message_empty_store_matches_oracle() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    for history in [false, true] {
        let query = ClientListQuery { history, limit: Some(1), ..Default::default() };
        let now = client_now_ms();
        let actual = collect_sql_pages(&state, query, now).await;
        assert_eq!(actual, client_message_resources_at(&state.store, None, history, None, now).unwrap());
        assert!(actual.is_empty());
    }
}

#[tokio::test]
async fn sql_message_cursor_refreshes_presence_but_keeps_first_delivery_age() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let recipient = "agent/sql-live-presence-only";
    for index in 0..3 {
        append_fixture_claim(
            &state.store, &format!("message/sql-presence-{index}"), "message.sent",
            "agent/sql-sender",
            BTreeMap::from([
                ("from".into(), json!("agent/sql-sender")),
                ("to".into(), json!(recipient)),
                ("content".into(), json!("presence fixture")),
                ("status".into(), json!("sent")),
            ]),
        );
    }
    let now = client_now_ms();
    let query = ClientListQuery { limit: Some(1), ..Default::default() };
    let (Extension(snapshot), Json(first)) = client_messages_sql_page_at(
        &state, new_client_snapshot(&state), &query, now,
    ).await.unwrap();
    let expected = client_message_resources_at(&state.store, None, false, None, now).unwrap();
    let cursor = first.page.next_cursor.expect("fixture spans pages");
    assert!(first.items[0]["delivery"]["recipient_delivery"].is_null());
    delivery_presence::record(recipient,
        r#"{"transport":"fixture","ready":false,"reason":"fixture path blocked"}"#);
    let continuation = ClientListQuery { cursor: Some(cursor), ..query };
    let (_, Json(second)) = client_messages_sql_page_at(
        &state, snapshot, &continuation, now + 60_000,
    ).await.unwrap();
    let sent_at = chrono::DateTime::parse_from_rfc3339(
        expected[1]["sent_at"].as_str().unwrap(),
    ).unwrap().timestamp_millis();
    let mut expected_item = expected[1].clone();
    expected_item["delivery"] = message_delivery_value(
        recipient, "sent", u128::try_from(sent_at).unwrap(), now,
    );
    assert!(!second.items[0]["delivery"]["recipient_delivery"].is_null());
    assert_eq!(second.items[0], expected_item);
}

fn process_cpu_time() -> Duration {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: the pointer refers to an initialized, writable timespec.
    assert_eq!(unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time)
    }, 0);
    Duration::new(
        u64::try_from(time.tv_sec).unwrap(),
        u32::try_from(time.tv_nsec).unwrap(),
    )
}

#[tokio::test]
async fn sql_message_pages_match_desired_only_legacy_messages() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let intent = crate::graph::parse_test_intent(
        r#"version 2
agent "sql-legacy" { workspace "/tmp"; command "true" }
message "sql-legacy-mail" {
    from "requester"
    to "sql-legacy"
    content "legacy desired-only message"
    title "Legacy"
}
"#,
        "node",
    ).unwrap();
    let mission = state.store.mission(&intent, crate::model::IntentInput {
        kdl: "legacy-message-fixture".into(), source_name: None,
    }).unwrap();
    state.store.apply(&intent, &mission.subject_tokens, "legacy-message-fixture").unwrap();
    let messages = state.store.messages(None, true).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(state.store.claims_for(&messages[0].subject, Some("message.sent")).unwrap().is_empty());
    for history in [false, true] {
        let now = client_now_ms();
        let expected = client_message_resources_at(&state.store, None, history, None, now).unwrap();
        let query = ClientListQuery { history, limit: Some(1), ..Default::default() };
        assert_eq!(collect_sql_pages(&state, query, now).await, expected);
    }
}

#[tokio::test]
async fn sql_message_pages_match_replicated_tied_claims_in_both_arrival_orders() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let source = Store::open_memory("sql-source").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let accepted_at = client_now_ms().saturating_sub(60_000);
    source.set_write_clock_at(accepted_at).unwrap();
    for index in 0..6 {
        send_fixture_message(&source, index, 17);
    }
    let tied = source.claims_for("message/sql-17-00000", None).unwrap();
    assert!(tied.len() > 1);
    assert!(tied.iter().all(|claim| claim.accepted_at_unix_ms == accepted_at));
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = super::tests::state(root.path());
        state.store.bind_fleet(FLEET).unwrap();
        let mut exchange = source.export_replication_exchange(
            FLEET, &state.store.replication_inventory().unwrap(),
        ).unwrap();
        if reverse {
            exchange.envelopes.reverse();
        }
        state.store.receive_replication_exchange("sql-source", FLEET, &exchange).unwrap();
        state.store.validate_replication_backlog().unwrap();
        assert!(state.store.project_replication_backlog().unwrap());
        for history in [false, true] {
            let now = client_now_ms();
            let expected = client_message_resources_at(&state.store, None, history, None, now).unwrap();
            assert!(!expected.is_empty());
            let query = ClientListQuery { history, limit: Some(1), ..Default::default() };
            assert_eq!(collect_sql_pages(&state, query, now).await, expected,
                "replicated reverse-arrival={reverse}, history={history}");
        }
    }
}

#[tokio::test]
#[ignore = "manual isolated old/new message paging timing comparison"]
async fn benchmark_sql_message_pages_against_full_oracle() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    for index in 0..2_000 {
        let sender = if index % 40 == 0 { "agent/sql-narrow" } else { "agent/sql-sender" };
        append_fixture_claim(
            &state.store, &format!("message/sql-9-{index:05}"), "message.sent", sender,
            BTreeMap::from([
                ("from".into(), json!(sender)),
                ("to".into(), json!("agent/sql-recipient")),
                ("content".into(), json!(format!("cost fixture {index}"))),
                ("status".into(), json!("sent")),
            ]),
        );
    }
    for index in 0..2_000 {
        for metadata in 0..32 {
            append_fixture_claim(
                &state.store, &format!("message/sql-9-{index:05}"),
                "publication.operation", "agent/sql-sender",
                BTreeMap::from([
                    ("operation".into(), json!(format!("fixture/{index}/{metadata}"))),
                    ("action".into(), json!("fixture-history")),
                    ("status".into(), json!("accepted")),
                ]),
            );
        }
    }
    let fixture_claims = state.store.index().unwrap();
    let query = ClientListQuery {
        history: true, actor: Some("agent/sql-narrow".into()), limit: Some(20),
        ..Default::default()
    };
    // Fixture generation is outside all measured intervals. Run both orders against
    // the same isolated store to show cold and warmed algorithm runs, not load.
    for sql_first in [false, true] {
        let snapshot = new_client_snapshot(&state);
        let old = || {
            let start = Instant::now();
            let cpu = process_cpu_time();
            let items = client_message_resources_old(
                &state.store, None, true, query.actor.as_deref(),
            ).unwrap();
            (start.elapsed(), process_cpu_time() - cpu, items.len())
        };
        let before = (!sql_first).then(old);
        let start = Instant::now();
        let cpu = process_cpu_time();
        let (_, Json(first)) = client_messages_sql_page(&state, snapshot.clone(), &query).await.unwrap();
        let first_elapsed = start.elapsed();
        let first_cpu = process_cpu_time() - cpu;
        let continuation = ClientListQuery {
            cursor: Some(first.page.next_cursor.expect("narrow actor spans pages")),
            ..query.clone()
        };
        let start = Instant::now();
        let cpu = process_cpu_time();
        let (_, Json(second)) = client_messages_sql_page(&state, snapshot, &continuation).await.unwrap();
        let next_elapsed = start.elapsed();
        let next_cpu = process_cpu_time() - cpu;
        let (old_elapsed, old_cpu, old_count) = before.unwrap_or_else(old);
        eprintln!(
            "order={} fixture_messages=2000 metadata_claims_per_message=32 fixture_claims={fixture_claims} actor=agent/sql-narrow history=true limit=20 old_wall={old_elapsed:?} old_cpu={old_cpu:?} old_items={old_count} sql_first_wall={first_elapsed:?} sql_first_cpu={first_cpu:?} first_items={} sql_next_wall={next_elapsed:?} sql_next_cpu={next_cpu:?} next_items={}",
            if sql_first { "sql-before-old" } else { "old-before-sql" },
            first.items.len(), second.items.len(),
        );
        assert_eq!(first.items.len(), 20);
        assert_eq!(second.items.len(), 20);
    }
}
