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
async fn sql_message_fresh_pages_do_not_restore_deleted_desired_fields() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let intent = crate::graph::parse_test_intent(r#"version 2
agent "sql-removed" { workspace "/tmp"; command "true" }
message "sql-removed-mail" {
    from "requester"
    to "sql-removed"
    content "removed content"
    title "Removed title"
    tag "reminder:removed"
}
"#, "node").unwrap();
    let mission = state.store.mission(&intent, crate::model::IntentInput {
        kdl: "deleted-desired-message-fixture".into(), source_name: None,
    }).unwrap();
    state.store.apply(&intent,&mission.subject_tokens,"deleted-desired-message-fixture").unwrap();
    let subject = "message/sql-removed-mail";
    let now = client_now_ms();
    let before = collect_sql_pages(&state,ClientListQuery::default(),now).await;
    assert_eq!(before[0]["content"],"removed content");
    state.store.connection.batched(|tx| {
        Ok::<_,rusqlite::Error>(tx.execute("DELETE FROM desired WHERE subject=?1",[subject])?)
    }).unwrap().unwrap();
    assert!(!state.store.claims_for(subject,Some("intent.desired")).unwrap().is_empty());
    for history in [false,true] {
        let expected = client_message_resources_at(&state.store,None,history,None,now).unwrap();
        assert_eq!(expected.len(),1);
        assert_eq!(expected[0]["to"],"");
        assert_eq!(expected[0]["content"],"");
        assert!(expected[0]["title"].is_null());
        assert_eq!(expected[0]["tags"],json!([]));
        let query = ClientListQuery { history,limit:Some(1),..Default::default() };
        assert_eq!(collect_sql_pages(&state,query.clone(),now).await,expected);
        let requester = ClientListQuery { actor:Some("requester".into()),..query.clone() };
        assert_eq!(collect_sql_pages(&state,requester,now).await,expected);
        for filtered in [
            ClientListQuery { person:Some("agent/sql-removed".into()),..query.clone() },
            ClientListQuery { actor:Some("agent/sql-removed".into()),..query.clone() },
        ] {
            assert!(collect_sql_pages(&state,filtered,now).await.is_empty());
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageBenchmarkDesign {
    Version,
    Small,
}

impl MessageBenchmarkDesign {
    fn open(self, path: &Path) -> Store {
        let store = Store::open(path, "message-page-benchmark").unwrap();
        if self == Self::Version {
            store.client_messages_enable_version_benchmark().unwrap();
        }
        store
    }
}

fn message_benchmark_percentiles(samples: &mut [Duration]) -> Value {
    samples.sort_unstable();
    let percentile = |percent: usize| {
        samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1_000.0
    };
    json!({"samples": samples.len(), "p50_ms": percentile(50), "p95_ms": percentile(95)})
}

fn message_benchmark_storage(path: &Path) -> Value {
    let bytes = |suffix: &str| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::metadata(Path::new(&name)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("benchmark storage measurement failed: {error}"),
        }
    };
    let database = bytes("");
    let wal = bytes("-wal");
    let shm = bytes("-shm");
    json!({"database_bytes": database, "wal_bytes": wal, "shm_bytes": shm,
        "total_bytes": database + wal + shm})
}

struct MessageBenchmarkWriter<'a> {
    store: &'a Store,
    path: &'a Path,
    samples: BTreeMap<&'static str, Vec<Duration>>,
    claims: usize,
    completed_wal_bytes: u64,
    peak_wal_bytes: u64,
}

impl MessageBenchmarkWriter<'_> {
    fn append(
        &mut self, class: &'static str, subject: &str, kind: &str,
        actor: &str, fields: BTreeMap<String, Value>,
    ) {
        // Equal accepted times and input order across designs, without timing clock setup.
        self.store.set_write_clock_at(1_800_000_000_000 + u128::try_from(self.claims).unwrap()).unwrap();
        let input = ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: Some(actor.into()), fields,
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        };
        let start = Instant::now();
        self.store.append_claim(&input).unwrap();
        self.samples.entry(class).or_default().push(start.elapsed());
        self.claims += 1;
        // Disable SQLite's automatic checkpoint and use identical bounded segments.
        // Checkpoint/file measurement is outside append timings but inside seed wall time.
        if self.claims.is_multiple_of(256) {
            self.record_wal_segment();
            self.store.client_messages_benchmark_checkpoint().unwrap();
        }
    }

    fn record_wal_segment(&mut self) {
        let bytes = message_benchmark_storage(self.path)["wal_bytes"].as_u64().unwrap();
        self.completed_wal_bytes += bytes;
        self.peak_wal_bytes = self.peak_wal_bytes.max(bytes);
    }
}

const MESSAGE_BENCHMARK_MESSAGES: usize = 128;
const MESSAGE_BENCHMARK_METADATA: usize = 32;

fn seed_message_benchmark(store: &Store, path: &Path) -> Value {
    let mut writer = MessageBenchmarkWriter {
        store, path, samples: BTreeMap::from([
            ("message.sent", Vec::with_capacity(MESSAGE_BENCHMARK_MESSAGES)),
            ("unrelated_metadata", Vec::with_capacity(MESSAGE_BENCHMARK_MESSAGES * MESSAGE_BENCHMARK_METADATA)),
        ]), claims: 0,
        completed_wal_bytes: 0, peak_wal_bytes: 0,
    };
    let start = Instant::now();
    for index in 0..MESSAGE_BENCHMARK_MESSAGES {
        let sender = if index % 3 == 0 { "agent/sql-narrow" } else { "agent/sql-sender" };
        let recipient = if index % 2 == 0 { "person/sql-alex" } else { "agent/sql-recipient" };
        writer.append(
            "message.sent", &format!("message/sql-9-{index:05}"), "message.sent", sender,
            BTreeMap::from([
                ("from".into(), json!(sender)), ("to".into(), json!(recipient)),
                ("content".into(), json!(format!("cost fixture {index}"))),
                ("status".into(), json!("sent")),
            ]),
        );
    }
    for index in 0..MESSAGE_BENCHMARK_MESSAGES {
        for metadata in 0..MESSAGE_BENCHMARK_METADATA {
            writer.append(
                "unrelated_metadata", &format!("message/sql-9-{index:05}"),
                "publication.operation", "agent/sql-sender",
                BTreeMap::from([
                    ("operation".into(), json!(format!("fixture/{index}/{metadata}"))),
                    ("action".into(), json!("fixture-history")),
                    ("status".into(), json!("accepted")),
                ]),
            );
        }
    }
    let seed_wall = start.elapsed();
    writer.record_wal_segment();
    let classes = writer.samples.iter_mut().map(|(class, samples)| {
        ((*class).to_owned(), message_benchmark_percentiles(samples))
    }).collect::<serde_json::Map<String, Value>>();
    assert_eq!(writer.claims, MESSAGE_BENCHMARK_MESSAGES * (MESSAGE_BENCHMARK_METADATA + 1));
    json!({
        "seed_wall_ms": seed_wall.as_secs_f64() * 1_000.0, "claims": writer.claims,
        "append_claim_wall_including_commit": classes,
        "wal_segment_bytes_total": writer.completed_wal_bytes,
        "peak_wal_segment_bytes": writer.peak_wal_bytes,
        "checkpoint_every_claims": 256,
    })
}

fn read_message_benchmark_page(
    store: &Store, design: MessageBenchmarkDesign, person: Option<&str>,
    actor: Option<&str>, history: bool, after: Option<&(u128, String)>,
) -> (Vec<Value>, Option<(u128, String)>, bool) {
    let mut rows = store.read_snapshot(|cut_index| {
        let generation = match design {
            MessageBenchmarkDesign::Version => store.client_messages_version_generation()?,
            MessageBenchmarkDesign::Small => cut_index,
        };
        match design {
            MessageBenchmarkDesign::Version =>
                store.client_messages_version_page(person, actor, history, generation, after, 20),
            MessageBenchmarkDesign::Small =>
                store.client_messages_page(person, actor, history, generation, after, 20),
        }
    }).unwrap();
    let more = rows.len() > 20;
    rows.truncate(20);
    let key = rows.last().map(|(message, metadata, _)| {
        (metadata["sent_at"].as_str().unwrap().parse::<u128>().unwrap(), message.subject.clone())
    });
    let items = rows.into_iter().map(|(message, metadata, current)| {
        client_message_resource(message, &metadata, current, 1_800_000_100_000).unwrap()
    }).collect::<Vec<_>>();
    // Measure the same page-only resource construction and JSON serialization on both paths.
    std::hint::black_box(serde_json::to_vec(&items).unwrap());
    (items, key, more)
}

#[test]
#[ignore = "manual isolated same-fixture version/small message paging comparison"]
fn benchmark_sql_message_pages_against_full_oracle() {
    const SAMPLES: usize = 21;
    let cases = [
        ("actor_history", None, Some("agent/sql-narrow"), true),
        ("broad_history", None, None, true),
        ("person_history", Some("person/sql-alex"), None, true),
        ("broad_open", None, None, false),
        ("person_open", Some("person/sql-alex"), None, false),
    ];
    let mut reference_pages = BTreeMap::new();
    let mut small_narrow_within_target = true;
    // Fresh disk stores, identical inputs. Never access live data. The bounded
    // 4,224-claim short-content fixture is not a claim about 2–5GB production stores.
    // Run one order to keep the isolated writer proof bounded.
    for (round, designs) in [
        [MessageBenchmarkDesign::Version, MessageBenchmarkDesign::Small],
    ].into_iter().enumerate() {
        for design in designs {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("messages.sqlite");
            let start = Instant::now();
            let store = design.open(&path);
            let empty_open = start.elapsed();
            store.client_messages_benchmark_prepare_io().unwrap();
            let empty_storage = message_benchmark_storage(&path);
            let seed = seed_message_benchmark(&store, &path);
            let live_storage = message_benchmark_storage(&path);
            store.client_messages_benchmark_checkpoint().unwrap();
            let checkpointed_storage = message_benchmark_storage(&path);
            let growth = checkpointed_storage["database_bytes"].as_u64().unwrap()
                - empty_storage["database_bytes"].as_u64().unwrap();
            eprintln!("{}", json!({
                "benchmark": "client_messages", "round": round, "design": format!("{design:?}"),
                "fixture_messages": MESSAGE_BENCHMARK_MESSAGES, "metadata_claims_per_message": MESSAGE_BENCHMARK_METADATA, "limit": 20,
                "empty_open_including_schema_ms": empty_open.as_secs_f64() * 1_000.0,
                "seed": seed, "empty_storage": empty_storage, "seeded_live_storage": live_storage,
                "seeded_checkpointed_storage": checkpointed_storage, "database_growth_bytes": growth,
            }));
            drop(store);
            let start = Instant::now();
            let store = design.open(&path);
            let reopen = start.elapsed();
            store.client_messages_benchmark_prepare_io().unwrap();
            eprintln!("{}", json!({
                "benchmark": "client_messages_open", "round": round, "design": format!("{design:?}"),
                "populated_reopen_including_projection_open_ms": reopen.as_secs_f64() * 1_000.0,
            }));
            for (case, person, actor, history) in cases {
                let start = Instant::now();
                let (first, key, more) = read_message_benchmark_page(
                    &store, design, person, actor, history, None,
                );
                let first_observed = start.elapsed();
                assert_eq!(first.len(), 20, "{design:?}/{case}");
                assert!(more, "{design:?}/{case} spans multiple pages");
                for (phase, after) in [("first", None), ("next", key.as_ref())] {
                    let (mut items, _, _) = read_message_benchmark_page(
                        &store, design, person, actor, history, after,
                    );
                    // Claim IDs differ between fresh stores; all other returned fields must match.
                    for item in &mut items { item.as_object_mut().unwrap().remove("revision"); }
                    let expected = reference_pages.entry((case, phase)).or_insert_with(|| items.clone());
                    assert_eq!(&items, expected, "{design:?}/{case}/{phase}");
                    assert_eq!(items.len(), 20, "{design:?}/{case}/{phase}");
                    for _ in 0..3 {
                        std::hint::black_box(read_message_benchmark_page(
                            &store, design, person, actor, history, after,
                        ));
                    }
                    let mut samples = Vec::with_capacity(SAMPLES);
                    for _ in 0..SAMPLES {
                        let start = Instant::now();
                        std::hint::black_box(read_message_benchmark_page(
                            &store, design, person, actor, history, after,
                        ));
                        samples.push(start.elapsed());
                    }
                    let distribution = message_benchmark_percentiles(&mut samples);
                    if design == MessageBenchmarkDesign::Small && case == "actor_history" {
                        small_narrow_within_target &= distribution["p95_ms"].as_f64().unwrap() < 100.0;
                    }
                    eprintln!("{}", json!({
                        "benchmark": "client_messages_page", "round": round,
                        "design": format!("{design:?}"), "case": case, "phase": phase,
                        "person": person, "actor": actor, "history": history, "limit": 20,
                        "first_observed_case_page_ms": first_observed.as_secs_f64() * 1_000.0,
                        "store_snapshot_page_resource_json_wall": distribution,
                        "os_cache_evicted": false,
                    }));
                }
            }
            if design == MessageBenchmarkDesign::Small {
                // A checkpointed copy of the identical source-only fixture models the
                // original version design's one-time populated legacy-store backfill.
                // Keep this work after small reads so enabling versions cannot bias them.
                store.client_messages_benchmark_checkpoint().unwrap();
                let legacy = root.path().join("legacy-backfill.sqlite");
                std::fs::copy(&path, &legacy).unwrap();
                let before = message_benchmark_storage(&legacy);
                let start = Instant::now();
                let migrated = MessageBenchmarkDesign::Version.open(&legacy);
                let open_backfill = start.elapsed();
                let after = message_benchmark_storage(&legacy);
                let migrated_page = read_message_benchmark_page(
                    &migrated, MessageBenchmarkDesign::Version, None, Some("agent/sql-narrow"), true, None,
                );
                assert_eq!(migrated_page.0.len(), 20);
                let mut migrated_items = migrated_page.0;
                for item in &mut migrated_items {
                    item.as_object_mut().unwrap().remove("revision");
                }
                assert_eq!(
                    &migrated_items, &reference_pages[&("actor_history", "first")],
                    "legacy backfill must preserve the same page resources",
                );
                migrated.client_messages_benchmark_checkpoint().unwrap();
                eprintln!("{}", json!({
                    "benchmark": "client_messages_legacy_backfill", "round": round,
                    "fixture_messages": MESSAGE_BENCHMARK_MESSAGES, "fixture_claims": MESSAGE_BENCHMARK_MESSAGES * (MESSAGE_BENCHMARK_METADATA + 1),
                    "open_including_version_backfill_ms": open_backfill.as_secs_f64() * 1_000.0,
                    "before_storage": before, "after_live_storage": after,
                    "after_checkpointed_storage": message_benchmark_storage(&legacy),
                }));
            }
        }
    }
    eprintln!("{}", json!({
        "benchmark": "client_messages_target", "case": "actor_history",
        "limit": 20, "target_p95_ms_exclusive": 100,
        "small_first_and_next_pass": small_narrow_within_target,
    }));
}
