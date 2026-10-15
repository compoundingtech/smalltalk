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
    state.store.connection.write().execute_batch(/* sqlite-large-value: allow-for-now lv-h407ed81d4800 owner=st3::api::replication_guard_tests TODO=chunk-or-reference-exact-wire-values */ "DROP TABLE replica_envelopes").unwrap();
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
    let answer = serde_json::to_value(expected).unwrap();
    let old_app = Router::new().route("/v1/internal/replication/export", axum::routing::post(
        move |Json(old): Json<OldExportRequest>| {
            let answer = answer.clone();
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
    server.abort();
    let _ = server.await;
}
