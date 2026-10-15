use super::*;
use smallclaims::claim::ReplicaEnvelopeId;
use smallclaims::replication::ReplicationInventory;
use smallclaims::sync::Backend as _;

const FLEET: &str = "1f91ca65-7793-48cc-866e-ac15690130e1";
const GUARD: &str = "replication/authority-guard";
const SUMMARY: &str = "replication/summary";

fn request(summary_only: bool, authority_guard: bool) -> ReplicationExportRequest {
    ReplicationExportRequest {
        fleet_id: FLEET.into(),
        inventory: Default::default(),
        summary_only,
        authority_guard,
        authority_digest_only: false,
        signature_requests: Vec::new(),
    }
}

fn count(label: &str) -> u64 {
    crate::performance::snapshot()["requests"]
        .as_array().unwrap().iter()
        .find(|row| row["kind"] == label)
        .map_or(0, |row| row["count"].as_u64().unwrap())
}

#[tokio::test]
async fn guard_summary_accounting_preserves_responses_counts_errors_and_unreached_paths() {
    // The global meter must not share this process with parallel tests.
    if crate::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    state.store.bind_fleet(FLEET).unwrap();
    // Settle sealing/judging before comparing the unchanged response bytes and fresh index.
    state.store.export_replication_summary(FLEET).unwrap();
    let expected = ReplicationExportResponse {
        exchange: state.store.export_replication_summary(FLEET).unwrap(),
        store_index: state.store.index().unwrap(),
    };
    assert_eq!(count(GUARD), 0);
    let ordinary = replication_export(State(state.clone()), Json(request(true, false)))
        .await.unwrap().0;
    assert_eq!(serde_json::to_vec(&ordinary).unwrap(), serde_json::to_vec(&expected).unwrap());
    assert_eq!(count(GUARD), 0, "ordinary peer summary did not reach the guard");
    assert_eq!(count(SUMMARY), 1);
    let guarded = replication_export(State(state.clone()), Json(request(true, true)))
        .await.unwrap().0;
    assert_eq!(serde_json::to_vec(&guarded).unwrap(), serde_json::to_vec(&ordinary).unwrap());
    assert_eq!(count(GUARD), 1);
    assert_eq!(count(SUMMARY), 1);
    // The marker cannot change a full exchange into a summary or enter its recorder.
    let full = replication_export(State(state.clone()), Json(request(false, false)))
        .await.unwrap().0;
    let marked_full = replication_export(State(state.clone()), Json(request(false, true)))
        .await.unwrap().0;
    assert_eq!(serde_json::to_vec(&marked_full).unwrap(), serde_json::to_vec(&full).unwrap());
    assert_eq!(count(GUARD), 1);
    assert_eq!(count(SUMMARY), 1);
    // A genuine store refusal is still returned, and its reached guard call is counted.
    state.store.connection.write().execute_batch("DROP TABLE replica_envelopes").unwrap();
    assert!(replication_export(State(state), Json(request(true, true))).await.is_err());
    assert_eq!(count(GUARD), 2);
    let report = crate::performance::snapshot();
    let row = report["requests"].as_array().unwrap().iter()
        .find(|row| row["kind"] == GUARD).unwrap();
    assert_eq!(row["cpu_ms"], 0.0, "wall-only recorder does not measure CPU");
    assert_eq!(report["request_count"], 0, "task labels are not completed route requests");
    assert!(report["clients"].as_array().unwrap().is_empty());
    assert!(report["client_requests"].as_array().unwrap().is_empty());
}

#[test]
fn summary_recorder_preserves_result_and_existing_route_client_counts() {
    if crate::test_support::supervise_test() {
        return;
    }
    const ROUTE: &str = "/v1/internal/replication/export";
    crate::performance::record_request(ROUTE, Some("worker-fixture"), Duration::from_millis(7));
    let before = crate::performance::snapshot();
    let result: Result<Vec<u8>, &str> = record_replication_summary(true, || Ok(vec![3, 1, 4]));
    assert_eq!(result, Ok(vec![3, 1, 4]));
    let error: Result<(), &str> = record_replication_summary(true, || Err("export refused"));
    assert_eq!(error, Err("export refused"));
    let after = crate::performance::snapshot();
    for key in ["request_count", "clients", "client_requests"] {
        assert_eq!(after[key], before[key], "recorder changed {key}");
    }
    let route = |report: &Value| report["requests"].as_array().unwrap().iter()
        .find(|row| row["kind"] == ROUTE).unwrap().clone();
    assert_eq!(route(&after), route(&before));
    assert_eq!(count(GUARD), 2);
}

/// The old request body, retained without the new marker and without deny_unknown_fields.
#[derive(serde::Deserialize, serde::Serialize)]
struct OldExportRequest {
    fleet_id: String,
    inventory: ReplicationInventory,
    #[serde(default)]
    summary_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    signature_requests: Vec<ReplicaEnvelopeId>,
}

#[test]
fn guard_marker_is_ignored_by_old_bodies_and_absent_from_ordinary_requests() {
    let old = OldExportRequest {
        fleet_id: FLEET.into(), inventory: Default::default(),
        summary_only: true, signature_requests: Vec::new(),
    };
    let old_bytes = serde_json::to_vec(&old).unwrap();
    assert_eq!(serde_json::to_vec(&request(true, false)).unwrap(), old_bytes);
    let decoded: ReplicationExportRequest = serde_json::from_slice(&old_bytes).unwrap();
    assert!(!decoded.authority_guard);
    let marked = serde_json::to_vec(&request(true, true)).unwrap();
    let ignored: OldExportRequest = serde_json::from_slice(&marked).unwrap();
    assert_eq!(serde_json::to_vec(&ignored).unwrap(), old_bytes);
}

#[tokio::test]
async fn guard_summary_uses_unchanged_default_backend_and_old_local_handler() {
    if crate::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    state.store.bind_fleet(FLEET).unwrap();
    let graph: &smallclaims::store::Store = &state.store;
    let local = smallclaims::sync::Local(Arc::new(
        smallclaims::store::Store::open_memory("legacy", Arc::new(smallclaims::store::runtime::Plain)).unwrap()
    ));
    let normal = local.export(FLEET, &Default::default(), true, &[]).await.unwrap();
    let guard = local.guard_summary(FLEET).await.unwrap();
    assert_eq!(serde_json::to_vec(&guard).unwrap(), serde_json::to_vec(&normal).unwrap());
    let expected = ReplicationExportResponse {
        exchange: graph.export_replication_summary(FLEET).unwrap(),
        store_index: graph.index().unwrap(),
    };
    let expected_bytes = serde_json::to_vec(&expected).unwrap();
    let expected_digest = expected.exchange.authority_digest.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let answer = serde_json::to_value(expected).unwrap();
    let old_app = Router::new().route("/v1/internal/replication/export", axum::routing::post(
        move |Json(old): Json<OldExportRequest>| {
            let answer = answer.clone();
            handler_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            async move {
                assert_eq!(old.fleet_id, FLEET);
                assert!(old.summary_only);
                assert!(old.signature_requests.is_empty());
                Json(json!({"api_version": "st3.v1", "value": answer}))
            }
        }
    ));
    let socket = root.path().join("old.sock");
    let server_socket = socket.clone();
    let (ready, bound) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        serve_unix_with_ready(&server_socket, old_app, || { let _ = ready.send(()); }).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), bound).await.unwrap().unwrap();
    let main = crate::peer::MainBackend::new(socket);
    let received = main.guard_summary(FLEET).await.unwrap();
    assert_eq!(serde_json::to_vec(&received).unwrap(), expected_bytes);
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(main.authority_digest(FLEET).await.unwrap(), expected_digest);
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2,
        "the additive mode consumes an old response in one request, without retry");
    server.abort();
    let _ = server.await;
}


#[tokio::test]
async fn reduced_mode_requires_all_three_local_flags_and_omits_only_buckets() {
    if crate::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    state.store.bind_fleet(FLEET).unwrap();
    state.store.connection.write().execute(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,payload,relay,receipt_state,received_at_unix_ms)
         VALUES('fixture',1,?1,'0',X'','fixture','pending','0')", ["a".repeat(64)],
    ).unwrap();
    let full_summary = replication_export(State(state.clone()), Json(request(true, false)))
        .await
        .unwrap()
        .0;
    assert!(
        !full_summary.exchange.inventory.buckets.is_empty(),
        "negative control needs real buckets"
    );
    let full_exchange = replication_export(State(state.clone()), Json(request(false, false)))
        .await
        .unwrap()
        .0;
    for summary in [false, true] {
        for guard in [false, true] {
            for digest_only in [false, true] {
                let mut input = request(summary, guard);
                input.authority_digest_only = digest_only;
                let response = replication_export(State(state.clone()), Json(input))
                    .await
                    .unwrap()
                    .0;
                let mut expected = serde_json::to_value(if summary {
                    &full_summary
                } else {
                    &full_exchange
                })
                .unwrap();
                if summary && guard && digest_only {
                    expected["exchange"]["inventory"]
                        .as_object_mut()
                        .unwrap()
                        .remove("buckets");
                }
                assert_eq!(
                    serde_json::to_value(&response).unwrap(),
                    expected,
                    "summary={summary} guard={guard} digest_only={digest_only}"
                );
            }
        }
    }
}

#[test]
fn scalar_option_defaults_preserve_old_bytes_and_never_enter_peer_exchange_type() {
    let old = OldExportRequest {
        fleet_id: FLEET.into(),
        inventory: Default::default(),
        summary_only: true,
        signature_requests: Vec::new(),
    };
    let bytes = serde_json::to_vec(&old).unwrap();
    let decoded: ReplicationExportRequest = serde_json::from_slice(&bytes).unwrap();
    assert!(!decoded.authority_digest_only);
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
    let mut input = request(true, true);
    input.authority_digest_only = true;
    let ignored: OldExportRequest =
        serde_json::from_slice(&serde_json::to_vec(&input).unwrap()).unwrap();
    assert_eq!(serde_json::to_vec(&ignored).unwrap(), bytes);
    // The peer transport serializes ReplicationExchange, never the local request options.
    let store = smallclaims::store::Store::open_memory(
        "fixture",
        Arc::new(smallclaims::store::runtime::Plain),
    )
    .unwrap();
    let peer = serde_json::to_value(store.export_replication_summary(FLEET).unwrap()).unwrap();
    for key in ["authority_digest_only", "authority_guard", "summary_only"] {
        assert!(peer.get(key).is_none());
    }
}

#[tokio::test]
async fn reduced_mode_preserves_reached_store_errors_and_guard_accounting() {
    if crate::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    state.store.bind_fleet(FLEET).unwrap();
    state.store.export_replication_summary(FLEET).unwrap();
    state
        .store
        .connection
        .write()
        .execute_batch("DROP TABLE replica_envelope_holds")
        .unwrap();
    let before = count(GUARD);
    let old = replication_export(State(state.clone()), Json(request(true, true))).await;
    let mut input = request(true, true);
    input.authority_digest_only = true;
    let new = replication_export(State(state), Json(input)).await;
    assert!(old.is_err());
    assert!(new.is_err());
    assert_eq!(
        count(GUARD),
        before + 2,
        "both reached guard failures remain visible"
    );
}

#[tokio::test]
async fn main_backend_scalar_uses_only_new_local_mode_and_reads_a_later_change() {
    if crate::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    state.store.bind_fleet(FLEET).unwrap();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = observed.clone();
    let handler_state = state.clone();
    let app = Router::new().route(
        "/v1/internal/replication/export",
        axum::routing::post(move |Json(input): Json<ReplicationExportRequest>| {
            let state = handler_state.clone();
            let calls = calls.clone();
            async move {
                let flags = (
                    input.summary_only,
                    input.authority_guard,
                    input.authority_digest_only,
                );
                let result = replication_export(State(state), Json(input))
                    .await
                    .unwrap()
                    .0;
                calls
                    .lock()
                    .unwrap()
                    .push((flags, result.exchange.inventory.buckets.len()));
                Json(json!({"api_version":"st3.v1","value":result}))
            }
        }),
    );
    let socket = root.path().join("new.sock");
    let server_socket = socket.clone();
    let (ready, bound) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        serve_unix_with_ready(&server_socket, app, || {
            let _ = ready.send(());
        })
        .await
        .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), bound)
        .await
        .unwrap()
        .unwrap();
    let main = crate::peer::MainBackend::new(socket);
    let ordinary = main
        .export(FLEET, &Default::default(), true, &[])
        .await
        .unwrap();
    let old_guard = main.guard_summary(FLEET).await.unwrap();
    assert_eq!(
        serde_json::to_value(&ordinary).unwrap(),
        serde_json::to_value(&old_guard).unwrap()
    );
    let before = main.authority_digest(FLEET).await.unwrap();
    assert_eq!(before, ordinary.exchange.authority_digest);
    state.store.connection.write().execute(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,payload,relay,receipt_state,received_at_unix_ms)
         VALUES('fixture',1,?1,'0',X'','fixture','pending','0')", ["b".repeat(64)],
    ).unwrap();
    let after = main.authority_digest(FLEET).await.unwrap();
    assert_ne!(
        after, before,
        "a previous guard result is not reused after a write"
    );
    assert_eq!(
        after,
        state
            .store
            .export_replication_summary(FLEET)
            .unwrap()
            .authority_digest
    );
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.0)
            .collect::<Vec<_>>(),
        vec![
            (true, false, false),
            (true, true, false),
            (true, true, true),
            (true, true, true)
        ]
    );
    assert_eq!(observed.lock().unwrap()[3].1, 0);
    server.abort();
    let _ = server.await;
}
