use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use rusqlite::{Connection, params};
use std::sync::{Barrier, Mutex};

// Only explicitly tracked fixture stores are counted. Other parallel tests cannot
// affect this admission-order control, and the registration is removed on drop.
static SNAPSHOT_CONSTRUCTIONS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

pub(in crate::api) fn note_snapshot_construction(state: &AppState) {
    if let Some(count) = SNAPSHOT_CONSTRUCTIONS
        .lock()
        .unwrap()
        .get_mut(&(Arc::as_ptr(&state.store) as usize))
    {
        *count += 1;
    }
}

struct SnapshotConstructions(usize);

impl SnapshotConstructions {
    fn track(state: &AppState) -> Self {
        let key = Arc::as_ptr(&state.store) as usize;
        assert!(
            SNAPSHOT_CONSTRUCTIONS
                .lock()
                .unwrap()
                .insert(key, 0)
                .is_none()
        );
        Self(key)
    }

    fn count(&self) -> usize {
        SNAPSHOT_CONSTRUCTIONS.lock().unwrap()[&self.0]
    }
}

impl Drop for SnapshotConstructions {
    fn drop(&mut self) {
        SNAPSHOT_CONSTRUCTIONS.lock().unwrap().remove(&self.0);
    }
}

// These controls measure whole-router global SQL counters; keep their isolated stores
// sequential without changing the host's RUST_TEST_THREADS or global clock.
static SERIAL: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
const OBSERVER: &str = "observer/detail/watch/source";
const SUBSCRIPTION: &str = "subscription/detail/watch/source";

fn declare(state: &AppState, count: usize) {
    let mut source = String::from(
        r#"version 2
resource "watch/source" { kind "filesystem.file" }
observer "watch/source" {
 resource "resource/watch/source"; provider "local.file"; locator "/tmp/source"; field "status"
}
subscription "watch/source" {
 observer "observer/watch/source"; to "agent/detail/worker"; on "status"; delivery "message"
}
"#,
    );
    for index in 0..count {
        source.push_str(&format!(
            r#"
observer "other/{index}" {{
 resource "resource/watch/source"; provider "local.file"; locator "/tmp/other"; field "status"
}}
subscription "other/{index}" {{
 observer "observer/other/{index}"; to "agent/detail/worker"; on "status"; delivery "message"
}}
"#
        ));
    }
    let intent = crate::graph::parse_execution_intent(&source, "detail-test", "detail").unwrap();
    state
        .store
        .apply_internal(&intent, &format!("detail-declarations-{count}"))
        .unwrap();
}

fn append_state(state: &AppState, subject: &str, status: &str) {
    state
        .store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: if subject.starts_with("observer/") {
                "observer.state".into()
            } else {
                "subscription.state".into()
            },
            actor: None,
            fields: BTreeMap::from([("state".into(), json!(status))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn fixture(root: &std::path::Path) -> AppState {
    let state = tests::test_state_named(root, "detail-test");
    declare(&state, 0);
    append_state(&state, OBSERVER, "healthy");
    append_state(&state, SUBSCRIPTION, "active");
    state
}

async fn get(state: &AppState, path: &str) -> (StatusCode, Value) {
    let response = super::super::router(state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn head_status(state: &AppState, path: &str) -> StatusCode {
    super::super::router(state.clone())
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn selected(state: &AppState, kind: &str, id: &str, snapshot: &ClientSnapshot) -> Option<Value> {
    observer_subscription_detail_at(
        state,
        kind,
        id,
        snapshot,
        &mut crate::store::observer_subscription_detail::DetailBudget::new(),
    )
    .unwrap()
}

fn oracle(state: &AppState, kind: &str, id: &str, snapshot: &ClientSnapshot) -> Value {
    client_detail(
        observer_subscription_resources(state, kind, true, snapshot).unwrap(),
        kind,
        id,
    )
    .unwrap()
    .0
}

#[tokio::test]
async fn detail_router_preserves_ids_kinds_stops_and_actor_scope() {
    let _serial = SERIAL.acquire().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    for (collection, kind, subject) in [
        ("observers", "observer", OBSERVER),
        ("subscriptions", "subscription", SUBSCRIPTION),
    ] {
        for id in [subject, subject.strip_prefix(&format!("{kind}/")).unwrap()] {
            let (status, value) = get(&state, &format!("/v1/client/{collection}/{id}")).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            state
                .store
                .read_snapshot(|index| {
                    let snapshot = client_snapshot_at(&state, index);
                    assert_eq!(value["value"], oracle(&state, kind, id, &snapshot));
                    assert_eq!(value["snapshot"]["store_index"], index);
                    assert_eq!(value["snapshot"]["id"], snapshot.id);
                    Ok(())
                })
                .unwrap();
        }
        for id in [
            "missing",
            if kind == "observer" {
                SUBSCRIPTION
            } else {
                OBSERVER
            },
        ] {
            let (status, value) = get(&state, &format!("/v1/client/{collection}/{id}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{value}");
            assert_eq!(value["code"], "not-found");
            assert!(value.get("value").is_none());
        }
    }
    // Projection compatibility fixture for legacy raw IDs, not a new admission rule.
    let connection = Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&connection).unwrap();
    connection.execute("UPDATE desired SET owner_run='mission-run/owner', owner_generation='run-generation/owner', owner_step='step-run/owner/watch' WHERE subject=?1", [OBSERVER]).unwrap();
    let (_, owned) = get(&state, &format!("/v1/client/observers/{OBSERVER}")).await;
    assert_eq!(owned["value"]["owner_run_id"], "mission-run/owner");
    assert_eq!(
        owned["value"]["owner_generation_id"],
        "run-generation/owner"
    );
    assert_eq!(owned["value"]["owner_step_id"], "step-run/owner/watch");
    state
        .store
        .read_snapshot(|index| {
            assert_eq!(
                owned["value"],
                oracle(
                    &state,
                    "observer",
                    OBSERVER,
                    &client_snapshot_at(&state, index)
                )
            );
            Ok(())
        })
        .unwrap();
    connection.execute("UPDATE desired SET owner_run=NULL,owner_generation=NULL,owner_step=NULL WHERE subject=?1", [OBSERVER]).unwrap();
    connection
        .execute(
            "INSERT INTO desired SELECT 'detail/watch/source',kind,revision,claim_id,
        body,member,owner_run,owner_generation,owner_step FROM desired WHERE subject=?1",
            [OBSERVER],
        )
        .unwrap();
    let (_, value) = get(&state, "/v1/client/observers/detail/watch/source").await;
    assert_eq!(value["value"]["id"], "detail/watch/source");
    assert_eq!(value["value"]["state"], "pending");
    connection
        .execute(
            "UPDATE desired SET kind='agent' WHERE subject='detail/watch/source'",
            [],
        )
        .unwrap();
    let (_, value) = get(&state, "/v1/client/observers/detail/watch/source").await;
    assert_eq!(value["value"]["id"], OBSERVER);
    connection
        .execute(
            "DELETE FROM desired WHERE subject='detail/watch/source'",
            [],
        )
        .unwrap();

    let stop = crate::graph::parse_execution_intent(
        "version 2\nobserver \"watch/source\" { stop }\nsubscription \"watch/source\" { stop }",
        "detail-test",
        "detail",
    )
    .unwrap();
    state.store.apply_internal(&stop, "detail-stop").unwrap();
    for (collection, kind, id) in [
        ("observers", "observer", OBSERVER),
        ("subscriptions", "subscription", SUBSCRIPTION),
    ] {
        let (status, value) = get(&state, &format!("/v1/client/{collection}/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["value"]["state"], "stopped");
        assert_eq!(value["value"]["operational"]["layer"], "history");
        state
            .store
            .read_snapshot(|index| {
                assert_eq!(
                    value["value"],
                    oracle(&state, kind, id, &client_snapshot_at(&state, index))
                );
                Ok(())
            })
            .unwrap();
    }

    // Actual authenticated fabric router: missing read scope refuses even a missing ID;
    // projection scope retains the existing actor's observer/subscription visibility.
    for (index, scopes, expected) in [
        (0, json!([]), StatusCode::FORBIDDEN),
        (1, json!(["read.projections"]), StatusCode::OK),
    ] {
        let credential = format!("detail-reader-{index}");
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("custom/client/detail-reader-{index}"),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::from([
                    (
                        "credential_hash".into(),
                        json!(credential_digest(&credential)),
                    ),
                    ("session_actor".into(), json!("client/detail-reader")),
                    ("person_id".into(), json!("person/ada")),
                    ("scopes".into(), scopes),
                    (
                        "expires_at_unix_ms".into(),
                        json!(client_now_ms() as u64 + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        for path in [
            format!("/v1/client/observers/{OBSERVER}"),
            format!("/v1/client/subscriptions/{SUBSCRIPTION}"),
        ] {
            let response = super::super::fabric_router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(AUTHORIZATION, format!("Bearer {credential}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }
}

#[tokio::test]
async fn detail_read_and_envelope_keep_the_pinned_cut_during_a_write() {
    let _serial = SERIAL.acquire().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let barrier = Arc::new(Barrier::new(2));
    let writer_state = state.clone();
    let writer_barrier = barrier.clone();
    let writer = std::thread::spawn(move || {
        writer_barrier.wait();
        let stop = crate::graph::parse_execution_intent(
            "version 2\nobserver \"watch/source\" { stop }",
            "detail-test",
            "detail",
        )
        .unwrap();
        writer_state
            .store
            .apply_internal(&stop, "concurrent-detail-stop")
            .unwrap();
        append_state(&writer_state, OBSERVER, "stopped");
        writer_barrier.wait();
    });
    let (snapshot, before) = state
        .store
        .read_snapshot(|index| {
            let snapshot = client_snapshot_at(&state, index);
            let before = oracle(&state, "observer", OBSERVER, &snapshot);
            barrier.wait();
            barrier.wait();
            assert_eq!(
                selected(&state, "observer", OBSERVER, &snapshot),
                Some(before.clone())
            );
            assert_eq!(client_snapshot_at(&state, index).id, snapshot.id);
            Ok((snapshot, before))
        })
        .unwrap();
    writer.join().unwrap();
    let (status, after) = get(&state, &format!("/v1/client/observers/{OBSERVER}")).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(before["state"], "healthy");
    assert_eq!(after["value"]["state"], "stopped");
    assert!(after["snapshot"]["store_index"].as_u64().unwrap() > snapshot.store_index);
    assert_ne!(after["snapshot"]["id"], snapshot.id);
}

async fn refuses(state: &AppState, path: &str, code: &str) -> Value {
    let (status, value) = get(state, path).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
    assert_eq!(value["code"], code, "{value}");
    assert_eq!(value["retryable"], false);
    assert!(
        value.get("value").is_none(),
        "no partial resource or list fallback: {value}"
    );
    value
}

#[tokio::test]
async fn detail_router_admits_timestamp_and_host_before_any_snapshot_construction() {
    let _serial = SERIAL.acquire().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let connection = Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&connection).unwrap();
    let (position, accepted): (u64, String) = connection
        .query_row(
            "SELECT store_index,accepted_at_unix_ms FROM claims ORDER BY store_index DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    // Normal digest admission rejects malformed timestamp encoding. This isolated
    // corruption fixture suspends its UPDATE digest trigger, restores the exact
    // original timestamp, then reinstalls the trigger before successful reads.
    let triggers = connection.prepare("SELECT name,sql FROM sqlite_master WHERE type='trigger' AND tbl_name='claims' AND sql LIKE '%AFTER UPDATE%' AND sql LIKE '%st_projection_change%'")
        .unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(!triggers.is_empty());
    for (name, _) in &triggers {
        connection
            .execute_batch(&format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\"")))
            .unwrap();
    }
    let snapshots = SnapshotConstructions::track(&state);
    // The latest claim is a subscription state, not the selected observer state.
    // Both routes must guard this envelope input before the initial snapshot helper.
    for collection in ["observers", "subscriptions"] {
        let subject = if collection == "observers" {
            OBSERVER
        } else {
            SUBSCRIPTION
        };
        let path = format!("/v1/client/{collection}/{subject}");
        connection
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE store_index=?1",
                params![position, "0".repeat(280_000)],
            )
            .unwrap();
        refuses(&state, &path, "projection-detail-too-large").await;
        assert_eq!(
            head_status(&state, &path).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            snapshots.count(),
            0,
            "oversize timestamp was formatted before admission"
        );
        for invalid in ["X'80'", "CAST(X'80' AS TEXT)", "'invalid'"] {
            connection
                .execute(
                    &format!(
                        "UPDATE claims SET accepted_at_unix_ms={invalid} WHERE store_index=?1"
                    ),
                    [position],
                )
                .unwrap();
            refuses(&state, &path, "projection-detail-invalid-source").await;
            assert_eq!(
                snapshots.count(),
                0,
                "invalid timestamp reached snapshot construction"
            );
        }
        connection
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE store_index=?1",
                params![position, accepted],
            )
            .unwrap();
        let mut oversized_host = state.clone();
        oversized_host.node = "h".repeat(4097);
        refuses(&oversized_host, &path, "projection-detail-too-large").await;
        assert_eq!(
            head_status(&oversized_host, &path).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            snapshots.count(),
            0,
            "oversize host was formatted before admission"
        );
    }
    for (_, sql) in triggers {
        connection.execute_batch(&sql).unwrap();
    }
    // A successful detail constructs exactly the guarded response snapshot, which
    // still appears in the wire envelope. Other routes keep admission snapshots.
    let (status, value) = get(&state, &format!("/v1/client/observers/{OBSERVER}")).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(snapshots.count(), 1);
    assert_eq!(value["snapshot"]["store_index"], position);
    assert!(
        value["snapshot"]["id"]
            .as_str()
            .unwrap()
            .starts_with("snapshot/")
    );
    let (status, _) = get(&state, "/v1/client/observers").await;
    assert_eq!(status, StatusCode::OK);
    assert!(snapshots.count() > 1, "list admission remains unchanged");
}

#[tokio::test]
async fn detail_width_type_encoding_and_cumulative_output_refusals_are_named() {
    let _serial = SERIAL.acquire().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let path = format!("/v1/client/observers/{OBSERVER}");
    let connection = Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&connection).unwrap();
    let huge = "Ä".repeat(140_000); // bytes, not Unicode character count
    assert_eq!(
        connection
            .query_row("SELECT octet_length(?1)", [&huge], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        huge.len()
    );
    let error = refuses(
        &state,
        &format!("/v1/client/observers/{}", "k".repeat(4097)),
        "projection-detail-too-large",
    )
    .await;
    assert!(error["message"].as_str().unwrap().contains("key budget"));
    for column in [
        "body",
        "revision",
        "owner_run",
        "owner_generation",
        "owner_step",
    ] {
        let original: Option<String> = connection
            .query_row(
                &format!("SELECT {column} FROM desired WHERE subject=?1"),
                [OBSERVER],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                &format!("UPDATE desired SET {column}=?2 WHERE subject=?1"),
                params![OBSERVER, huge],
            )
            .unwrap();
        let error = refuses(&state, &path, "projection-detail-too-large").await;
        assert!(error["message"].as_str().unwrap().contains("source"));
        connection
            .execute(
                &format!("UPDATE desired SET {column}=?2 WHERE subject=?1"),
                params![OBSERVER, original],
            )
            .unwrap();
    }
    // Malformed encodings/types cannot pass the normal projection digest writer.
    // This isolated corruption fixture suspends only its desired UPDATE digest trigger,
    // restores the original row, then restores the trigger; production admission is intact.
    let triggers = connection.prepare("SELECT name,sql FROM sqlite_master WHERE type='trigger' AND tbl_name='desired' AND sql LIKE '%AFTER UPDATE%' AND sql LIKE '%st_projection_change%'")
        .unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?)))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(!triggers.is_empty());
    for (name, _) in &triggers {
        connection
            .execute_batch(&format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\"")))
            .unwrap();
    }
    // Kind eligibility must not materialize an oversized wrong-kind row before
    // returning absent/fallback. A raw candidate refusing admission never falls
    // through to the otherwise valid prefixed declaration.
    connection.execute(
        "INSERT INTO desired SELECT 'detail/watch/source','agent',revision,claim_id,body,NULL,NULL,NULL,NULL FROM desired WHERE subject=?1",
        [OBSERVER],
    ).unwrap();
    let raw_path = "/v1/client/observers/detail/watch/source";
    assert_eq!(
        get(&state, raw_path).await.0,
        StatusCode::OK,
        "ordinary wrong-kind raw ID falls back"
    );
    connection
        .execute(
            "UPDATE desired SET kind=?2 WHERE subject=?1",
            params!["detail/watch/source", huge],
        )
        .unwrap();
    refuses(&state, raw_path, "projection-detail-too-large").await;
    for value in ["X'80'", "CAST(X'80' AS TEXT)"] {
        connection
            .execute(
                &format!("UPDATE desired SET kind={value} WHERE subject=?1"),
                ["detail/watch/source"],
            )
            .unwrap();
        refuses(&state, raw_path, "projection-detail-invalid-source").await;
    }
    // DELETE digest admission reads OLD.kind. Restore the original valid kind
    // while this fixture's UPDATE digest trigger is still suspended.
    connection
        .execute(
            "UPDATE desired SET kind='agent' WHERE subject='detail/watch/source'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM desired WHERE subject='detail/watch/source'",
            [],
        )
        .unwrap();
    for value in ["X'80'", "CAST(X'80' AS TEXT)", "'{'"] {
        let original: String = connection
            .query_row(
                "SELECT body FROM desired WHERE subject=?1",
                [OBSERVER],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                &format!("UPDATE desired SET body={value} WHERE subject=?1"),
                [OBSERVER],
            )
            .unwrap();
        refuses(&state, &path, "projection-detail-invalid-source").await;
        connection
            .execute(
                "UPDATE desired SET body=?2 WHERE subject=?1",
                params![OBSERVER, original],
            )
            .unwrap();
    }
    for (_, sql) in triggers {
        connection.execute_batch(&sql).unwrap();
    }
    // A transaction on this fixture connection would hide modifications from router readers.
    // Persist each corrupt fixture update, then restore it before the next case.
    let original: String = connection
        .query_row(
            "SELECT body FROM desired WHERE subject=?1",
            [OBSERVER],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE desired SET member=?2 WHERE subject=?1",
            params![OBSERVER, huge],
        )
        .unwrap();
    assert_eq!(
        get(&state, &path).await.0,
        StatusCode::OK,
        "unused membership is never decoded"
    );
    connection
        .execute(
            "UPDATE desired SET owner_run=?2 WHERE subject=?1",
            params![OBSERVER, "\u{1}".repeat(90_000)],
        )
        .unwrap();
    let error = refuses(&state, &path, "projection-detail-too-large").await;
    assert!(error["message"].as_str().unwrap().contains("output"));
    connection
        .execute(
            "UPDATE desired SET owner_run=NULL WHERE subject=?1",
            [OBSERVER],
        )
        .unwrap();
    connection.execute("INSERT INTO desired SELECT 'detail/watch/source',kind,revision,claim_id,?2,NULL,NULL,NULL,NULL FROM desired WHERE subject=?1", params![OBSERVER, json!({"padding":"x".repeat(140_000),"children":[]}).to_string()]).unwrap();
    let mut padded: Value = serde_json::from_str(&original).unwrap();
    padded["padding"] = json!("x".repeat(140_000));
    connection
        .execute(
            "UPDATE desired SET body=?2 WHERE subject=?1",
            params![OBSERVER, padded.to_string()],
        )
        .unwrap();
    refuses(
        &state,
        "/v1/client/observers/detail/watch/source",
        "projection-detail-too-large",
    )
    .await;
    connection
        .execute(
            "DELETE FROM desired WHERE subject='detail/watch/source'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE desired SET body=?2 WHERE subject=?1",
            params![OBSERVER, original],
        )
        .unwrap();
    let position: u64 = connection
        .query_row(
            "SELECT max(store_index) FROM claims WHERE subject=?1 AND kind='observer.state'",
            [OBSERVER],
            |row| row.get(0),
        )
        .unwrap();
    let body: String = connection
        .query_row(
            "SELECT body FROM claims WHERE store_index=?1",
            [position],
            |row| row.get(0),
        )
        .unwrap();
    // Claim expression indexes require valid JSON even for internal fixture writes.
    let large_state = json!({"fields":{"state":"healthy","reason":huge}}).to_string();
    connection
        .execute(
            "UPDATE claims SET body=?2 WHERE store_index=?1",
            params![position, large_state],
        )
        .unwrap();
    refuses(&state, &path, "projection-detail-too-large").await;
    connection
        .execute(
            "UPDATE claims SET body=?2, predecessors=?3 WHERE store_index=?1",
            params![position, body, huge],
        )
        .unwrap();
    assert_eq!(
        get(&state, &path).await.0,
        StatusCode::OK,
        "unused predecessors are never decoded"
    );
    let accepted: String = connection
        .query_row(
            "SELECT accepted_at_unix_ms FROM claims WHERE store_index=?1",
            [position],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE claims SET accepted_at_unix_ms='invalid' WHERE store_index=?1",
            [position],
        )
        .unwrap();
    refuses(&state, &path, "projection-detail-invalid-source").await;
    connection
        .execute(
            "UPDATE claims SET accepted_at_unix_ms=?2 WHERE store_index=?1",
            params![position, accepted],
        )
        .unwrap();
}

#[tokio::test]
async fn detail_router_sql_work_stays_fixed_when_declarations_and_history_grow() {
    let _serial = SERIAL.acquire().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    declare(&state, 32);
    let mut before = Vec::new();
    let mut old = Vec::new();
    for count in [32, 320] {
        if count == 320 {
            declare(&state, count);
            for index in 0..320 {
                append_state(
                    &state,
                    OBSERVER,
                    if index % 2 == 0 {
                        "degraded"
                    } else {
                        "healthy"
                    },
                );
                append_state(
                    &state,
                    SUBSCRIPTION,
                    if index % 2 == 0 { "pending" } else { "active" },
                );
            }
        }
        for (collection, kind, subject) in [
            ("observers", "observer", OBSERVER),
            ("subscriptions", "subscription", SUBSCRIPTION),
        ] {
            let start = smallclaims::sqlite::work::total();
            let (status, value) = get(&state, &format!("/v1/client/{collection}/{subject}")).await;
            let cost = smallclaims::sqlite::work::total() - start;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(value["value"]["id"], subject);
            assert_eq!(
                value["value"]["state"],
                if kind == "observer" {
                    "healthy"
                } else {
                    "active"
                }
            );
            assert_eq!(cost.fullscan_steps, 0, "{kind}/{count}: {cost:?}");
            assert_eq!(cost.sorts, 0, "{kind}/{count}: {cost:?}");
            eprintln!(
                "selected-detail {kind} declarations={count} one_item bytes={} SQL={cost:?}",
                serde_json::to_vec(&value["value"]).unwrap().len()
            );
            if count == 32 {
                before.push(cost);
            } else {
                let prior = before[if kind == "observer" { 0 } else { 1 }];
                assert_eq!(cost.statements, prior.statements);
                assert!(
                    cost.vm_steps <= prior.vm_steps + 32,
                    "{prior:?} -> {cost:?}"
                );
            }
            state
                .store
                .read_snapshot(|index| {
                    let snapshot = client_snapshot_at(&state, index);
                    let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
                    let expected = oracle(&state, kind, subject, &snapshot);
                    let old_cost = scope.finish();
                    assert_eq!(value["value"], expected);
                    if count == 32 {
                        old.push(old_cost);
                    } else {
                        let prior = old[if kind == "observer" { 0 } else { 1 }];
                        assert!(
                            old_cost.vm_steps > prior.vm_steps * 3,
                            "negative oracle: {prior:?} -> {old_cost:?}"
                        );
                        assert!(old_cost.fullscan_steps > prior.fullscan_steps * 3);
                    }
                    eprintln!("old-list-detail {kind} declarations={count} SQL={old_cost:?}");
                    Ok(())
                })
                .unwrap();
        }
    }
}

#[tokio::test]
async fn detail_seek_plans_and_metadata_bytecodes_use_indexed_headers() {
    let _serial = SERIAL.acquire().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    let connection = Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&connection).unwrap();
    use crate::store::observer_subscription_detail::{
        DECLARATION_KIND_METADATA_SQL, DECLARATION_METADATA_SQL, SNAPSHOT_METADATA_SQL,
        STATE_METADATA_SQL,
    };
    let state_index_root: i64 = connection.query_row(
        "SELECT rootpage FROM sqlite_master WHERE type='index' AND name='claims_subject_kind_index'",
        [],
        |row| row.get(0),
    ).unwrap();
    for (sql, table) in [
        (DECLARATION_KIND_METADATA_SQL, "desired"),
        (DECLARATION_METADATA_SQL, "desired"),
        (STATE_METADATA_SQL, "claims"),
        (SNAPSHOT_METADATA_SQL, "claims"),
    ] {
        let mut query = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let mut rows = query.raw_query();
        let mut plan = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            plan.push(row.get::<_, String>(3).unwrap());
        }
        assert!(
            plan.iter()
                .any(|row| row.contains(&format!("SEARCH {table}"))),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|row| row.contains("SCAN")
                || row.contains("TEMP B-TREE")
                || row.contains("CORRELATED")),
            "{plan:?}"
        );
        if sql == STATE_METADATA_SQL {
            assert!(
                plan.iter()
                    .any(|row| row.contains("claims_subject_kind_index")
                        && row.contains("store_index<?")),
                "{plan:?}"
            );
        }
        // Audit every OP_Column, including kind, revision, owners and timestamps.
        // Only the composite index's integer store_index may use an ordinary
        // column read; every variable-width source field needs byte/type flags.
        let mut query = connection.prepare(&format!("EXPLAIN {sql}")).unwrap();
        let mut rows = query.raw_query();
        let mut cursor_roots = BTreeMap::new();
        let mut flags = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            let opcode = row.get::<_, String>(1).unwrap();
            let cursor = row.get::<_, i64>(2).unwrap();
            let column = row.get::<_, i64>(3).unwrap();
            if opcode == "OpenRead" {
                cursor_roots.insert(cursor, column);
            }
            if opcode == "Column" {
                let flag = row.get::<_, u8>(6).unwrap();
                let integer_index_position = sql == STATE_METADATA_SQL
                    && cursor_roots.get(&cursor) == Some(&state_index_root)
                    && column == 2;
                assert!(
                    matches!(flag & 0xc0, 0x80 | 0xc0) || integer_index_position,
                    "unguarded source OP_Column cursor={cursor} column={column} p5={flag} in {sql}"
                );
                flags.push((cursor, column, flag));
            }
        }
        // EXPLAIN p5 is column 6; P2 (stored column number) is column 3.
        assert!(!flags.is_empty(), "no metadata column in {sql}");
        eprintln!("detail query plan={plan:?} metadata OP_Column p5={flags:?}");
    }
    let snapshot = state
        .store
        .read_snapshot(|index| Ok(client_snapshot_at(&state, index)))
        .unwrap();
    let expected = state
        .store
        .read_snapshot(|_| Ok(selected(&state, "observer", OBSERVER, &snapshot)))
        .unwrap();
    for index in 0..256 {
        append_state(
            &state,
            OBSERVER,
            if index % 2 == 0 {
                "degraded"
            } else {
                "healthy"
            },
        );
    }
    state
        .store
        .read_snapshot(|_| {
            let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
            assert_eq!(selected(&state, "observer", OBSERVER, &snapshot), expected);
            let cost = scope.finish();
            assert_eq!(cost.fullscan_steps, 0, "old-cut range seek: {cost:?}");
            assert_eq!(cost.sorts, 0);
            assert!(cost.vm_steps < 400, "old-cut range seek: {cost:?}");
            Ok(())
        })
        .unwrap();
}
