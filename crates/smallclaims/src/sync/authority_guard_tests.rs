//! Exercise the actual dialer branch as well as the default scalar method. No live fleet or
//! daemon is used. The mock changes only the guard's response; ordinary exports stay full.
use super::*;
use crate::fleet::handshake::JoinRequest;
use crate::replication::{ReplicationExportResponse, ReplicationReceiveResponse};
use crate::store::Runtime as _;
use crate::store::{CheckpointAction, CheckpointManifestNeed};
use crate::sync::Local;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

const FLEET: &str = "fixture-fleet";

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Clone)]
struct GuardBackend {
    answer: Arc<Mutex<Result<String, String>>>,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
}

fn response(digest: String) -> ReplicationExportResponse {
    ReplicationExportResponse {
        store_index: 0,
        exchange: ReplicationExchange {
            peer: "local".into(),
            fleet_id: FLEET.into(),
            schema_digest: String::new(),
            authority_digest: digest.clone(),
            graph_digest: String::new(),
            projection_digests: Default::default(),
            projection_registry_digest: Some(
                crate::store::runtime::Plain.projection_registry_digest(),
            ),
            projection_versions: crate::store::runtime::Plain.projection_versions(),
            inventory: ReplicationInventory {
                digest,
                ..Default::default()
            },
            envelopes: Vec::new(),
            signature_requests: Vec::new(),
            signatures: Vec::new(),
        },
    }
}

impl GuardBackend {
    fn new(answer: Result<&str, &str>) -> Self {
        Self {
            answer: Arc::new(Mutex::new(answer.map(str::to_owned).map_err(str::to_owned))),
            calls: Arc::default(),
            gate: None,
        }
    }
}

impl Backend for GuardBackend {
    async fn export(
        &self,
        _fleet: &str,
        _inventory: &ReplicationInventory,
        _summary: bool,
        _signatures: &[crate::claim::ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        Ok(response("ordinary-full-export".into()))
    }
    async fn guard_summary(&self, _fleet: &str) -> Result<ReplicationExportResponse> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(gate) = &self.gate
            && call == 0
        {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.answer
            .lock()
            .unwrap()
            .clone()
            .map(response)
            .map_err(anyhow::Error::msg)
    }
    // Do not override authority_digest: test the compatibility default and its one call.
    async fn receive(
        &self,
        _peer: &str,
        _fleet: &str,
        _exchange: &ReplicationExchange,
        _trip: Option<Duration>,
        _carrier: crate::replication::ReplicationCarrier,
    ) -> Result<ReplicationReceiveResponse> {
        unreachable!()
    }
    async fn checkpoint_manifest(
        &self,
        _request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        unreachable!()
    }
    async fn checkpoint_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        unreachable!()
    }
    async fn adopt_checkpoint(
        &self,
        _manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>> {
        unreachable!()
    }
    async fn publish_endpoints(&self, _mode: &str, _endpoints: &[Value]) -> Result<()> {
        unreachable!()
    }
    async fn redeem(&self, _request: &JoinRequest) -> Result<Value> {
        unreachable!()
    }
    async fn fleet_view(&self) -> Result<FleetView> {
        unreachable!()
    }
    async fn record_failure(
        &self,
        _peer: &str,
        _status: &str,
        _error: &str,
        _carrier: Option<crate::replication::ReplicationCarrier>,
    ) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn default_scalar_calls_unchanged_full_guard_once_and_preserves_errors() {
    let backend = GuardBackend::new(Ok("current"));
    assert_eq!(backend.authority_digest(FLEET).await.unwrap(), "current");
    assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    let backend = GuardBackend::new(Err("injected guard error"));
    assert_eq!(
        backend
            .authority_digest(FLEET)
            .await
            .unwrap_err()
            .to_string(),
        "injected guard error"
    );
    assert_eq!(
        backend.calls.load(Ordering::Relaxed),
        1,
        "no retry on failure"
    );
}

async fn dial_case(
    backend: GuardBackend,
    offered: Option<&str>,
    recent_inbound: bool,
    expect_dial: bool,
    change_in_flight: bool,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fleet = FleetContext::legacy(BTreeSet::from(["peer".into()]));
    // Presence requests a local push; only the existing recent-inbound guard can suppress it.
    fleet
        .online
        .write()
        .unwrap()
        .insert("peer".into(), tokio::time::Instant::now());
    if recent_inbound {
        fleet
            .inbound
            .write()
            .unwrap()
            .insert("peer".into(), tokio::time::Instant::now());
    }
    if let Some(offered) = offered {
        fleet
            .inbound_authority
            .write()
            .unwrap()
            .insert("peer".into(), offered.into());
    }
    let (_routes, route_rx) = watch::channel(vec![Route::Http(url)]);
    let (_wake, wake_rx) = watch::channel(0_u64);
    let task = tokio::spawn(dial_peer(
        backend.clone(),
        "local".into(),
        "peer".into(),
        route_rx,
        FleetAuth::test(FLEET, &[7; 32]),
        fleet,
        wake_rx,
    ));
    if change_in_flight {
        let gate = backend.gate.as_ref().unwrap();
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        *backend.answer.lock().unwrap() = Ok("changed-during-guard".into());
        gate.release.notify_one();
    }
    // A matching recent-inbound witness waits30s; a new/different/empty/error guard keeps
    // the existing1s coalescing window, or the immediate bootstrap path. No timer is changed.
    let timeout = if expect_dial {
        Duration::from_secs(3)
    } else {
        Duration::from_millis(1400)
    };
    let attempted = tokio::time::timeout(timeout, listener.accept())
        .await
        .is_ok();
    task.abort();
    let _ = task.await;
    assert_eq!(
        attempted, expect_dial,
        "offered={offered:?}, recent={recent_inbound}, in_flight={change_in_flight}"
    );
    let calls = backend.calls.load(Ordering::Relaxed);
    if recent_inbound && offered.is_some() {
        assert!(
            calls >= 1,
            "the actual guard was reached; coalescing can read it again"
        );
    } else {
        assert_eq!(
            calls, 0,
            "no new guard on bootstrap or without an offered witness"
        );
    }
}

#[tokio::test]
async fn actual_dialer_matches_old_nonempty_decisions_and_never_skips_empty_error_or_bootstrap() {
    for (answer, offered, recent) in [
        (Ok("same"), Some("same"), true),
        (Ok("different"), Some("same"), true),
        (Ok(""), Some("same"), true),
        (Err("read failed"), Some("same"), true),
        (Ok("same"), None, true),
        (Ok("same"), Some("same"), false),
    ] {
        // The independent old branch used the full guard's digest, at the same mocked cut.
        let old_skip = recent && offered.is_some() && answer.as_ref().ok().copied() == offered;
        dial_case(GuardBackend::new(answer), offered, recent, !old_skip, false).await;
    }
    // Invalid empty/empty cannot authorize a quiet skip, even though raw string equality can.
    dial_case(GuardBackend::new(Ok("")), Some(""), true, true, false).await;
}

#[tokio::test]
async fn local_change_while_guard_is_in_flight_keeps_the_existing_push_path() {
    let mut backend = GuardBackend::new(Ok("same"));
    backend.gate = Some(Arc::default());
    dial_case(backend, Some("same"), true, true, true).await;
}

#[tokio::test]
async fn signed_peer_request_cannot_select_the_local_reduced_mode() {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt as _;
    let store = Arc::new(
        crate::store::Store::open_memory("target", Arc::new(crate::store::runtime::Plain)).unwrap(),
    );
    store.bind_fleet(FLEET).unwrap();
    store
        .append_claim(&crate::claim::ClaimInput {
            subject: "note/fixture".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let auth = FleetAuth::test(FLEET, &[7; 32]);
    let state = PeerState::new(
        Local(store.clone()),
        "target".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["source".into()])),
    );
    let mut query = response(String::new()).exchange;
    query.schema_digest = crate::store::runtime::Plain.schema_digest();
    let mut body = serde_json::to_value(query).unwrap();
    body["peer"] = Value::String("source".into());
    // Even an authenticated peer cannot opt the ordinary peer route into the local mode.
    for key in ["summary_only", "authority_guard", "authority_digest_only"] {
        body[key] = Value::Bool(true);
    }
    let body = serde_json::to_vec(&body).unwrap();
    let headers = auth.request_headers("source", &body).unwrap();
    let mut request = Request::post(EXCHANGE_PATH)
        .body(Body::from(body.clone()))
        .unwrap();
    request.headers_mut().extend(headers);
    let result = peer_router(state, Router::new())
        .oneshot(request)
        .await
        .unwrap();
    assert!(result.status().is_success());
    let headers = result.headers().clone();
    let bytes = to_bytes(result.into_body(), MAX_EXCHANGE_BYTES)
        .await
        .unwrap();
    auth.verify(
        &headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &bytes,
        Some("target"),
        Some(&FleetAuth::body_digest(&body)),
    )
    .unwrap();
    let result: PeerResponse<ReplicationExchange> = serde_json::from_slice(&bytes).unwrap();
    assert!(
        !result.value.inventory.envelopes.is_empty(),
        "signed route retained its ordinary identity proof; the local reduced mode has none"
    );
    assert!(
        !result.value.envelopes.is_empty(),
        "ordinary missing envelopes were still exported"
    );
    let full = store.export_replication_summary(FLEET).unwrap();
    assert_eq!(result.value.authority_digest, full.authority_digest);
}
