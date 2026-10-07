//! Tests of the sync worker on stores opened with the plain runtime.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;

use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt as _;

use super::*;
use crate::claim::{ClaimInput, ReplicaEnvelopeId};
use crate::fleet::handshake::JoinRequest;
use crate::replication::{ReplicationExportResponse, ReplicationReceiveResponse};
use crate::store::runtime::Plain;
use crate::store::{CheckpointAction, CheckpointManifestNeed};
use crate::store::{Runtime as _, Store};
use crate::sync::Local;

/// A store on the plain runtime that heals without waiting, as the tests of smalltalk's stores do.
fn plain_memory(origin: &str) -> anyhow::Result<Store> {
    let mut store = Store::open_memory(origin, Arc::new(Plain))?;
    store.heal_replay_backoff_ms = 0;
    Ok(store)
}

fn plain_open(path: &Path, origin: &str) -> anyhow::Result<Store> {
    let mut store = Store::open(path, origin, Arc::new(Plain))?;
    store.heal_replay_backoff_ms = 0;
    Ok(store)
}

/// Different checkpoint lineages leave payloadless ranges that can never settle through
/// envelope transport. Both live writers already have a shared range, so a summary alone
/// cannot prove either of their later gaps.
#[tokio::test]
async fn checkpoint_tombstones_do_not_starve_later_live_ranges() {
    let root = tempfile::tempdir().unwrap();
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet, &[7; 32]);
    let left = Arc::new(plain_open(&root.path().join("left.sqlite3"), "birch").unwrap());
    let right = Arc::new(plain_open(&root.path().join("right.sqlite3"), "cedar").unwrap());
    let note = |store: &Store, text: &str| {
        store
            .append_claim(&ClaimInput {
                subject: format!("note/{text}"),
                kind: "example.note".into(),
                actor: None,
                fields: BTreeMap::from([("text".into(), Value::String(text.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    for store in [&left, &right] {
        store.bind_fleet(fleet).unwrap();
        note(store, &format!("{}-shared", store.origin));
    }
    for (from, to) in [(&left, &right), (&right, &left)] {
        let shared = from
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        to.receive_replication_exchange(&from.origin, fleet, &shared)
            .unwrap();
    }
    for store in [&left, &right] {
        for index in 0..20 {
            note(store, &format!("{}-private-{index}", store.origin));
        }
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        for id in crate::store::test_envelope_ids("alder-retired", 0..768, &store.origin) {
            transaction
                .execute(
                    "INSERT INTO checkpoint_envelopes VALUES (?1, ?2, ?3, 0, ?4)",
                    rusqlite::params![
                        id.writer,
                        id.sequence,
                        id.hash,
                        format!("checkpoint/{}", store.origin)
                    ],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        store.replica_rows_changed();
    }

    let state = PeerState {
        backend: Local(right.clone()),
        node: right.origin.clone(),
        auth: auth.clone(),
        fleet: FleetContext::legacy(BTreeSet::from([left.origin.clone()])),
        outbound_notify: watch::channel(0_u64).0,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = PeerConfig {
        name: right.origin.clone(),
        url: format!("http://{}", listener.local_addr().unwrap()),
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let router = peer_router(state, Router::new()).layer(axum::middleware::from_fn(
        move |request: Request<Body>, next: axum::middleware::Next| {
            let observed = observed.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, MAX_EXCHANGE_BYTES).await.unwrap();
                let decoded = if deflated(&parts.headers) {
                    inflate(&bytes).unwrap()
                } else {
                    bytes.to_vec()
                };
                let query: ReplicationExchange = serde_json::from_slice(&decoded).unwrap();
                assert!(query.envelopes.len() <= crate::store::REPLICATION_PAGE_LIMIT as usize);
                assert!(
                    !query.inventory.digest.is_empty(),
                    "a full-inventory request keeps its digest"
                );
                observed.lock().unwrap().push((
                    query.inventory.buckets.is_empty(),
                    query.inventory.envelopes.len(),
                    query.envelopes.len(),
                ));
                next.run(Request::from_parts(parts, Body::from(bytes)))
                    .await
            }
        },
    ));
    let server = tokio::spawn(axum::serve(listener, router).into_future());
    let http = replication_http_client();
    let fleet_context = FleetContext::legacy(BTreeSet::from([right.origin.clone()]));
    let result = exchange(
        &http,
        &Local(left.clone()),
        &left.origin,
        &peer,
        &auth,
        &fleet_context,
    )
    .await
    .unwrap();
    assert!(
        result.0,
        "a divergent compact exchange must reach live ranges beyond the tombstones"
    );
    {
        let requests = requests.lock().unwrap();
        assert!(
            requests.len() <= 4,
            "one compact round plus at most one full round, never an unbounded retry"
        );
        assert!(
            requests.contains(&(true, 0, 0)),
            "a bare digest cannot prove an empty inventory"
        );
    }
    for store in [&left, &right] {
        store.validate_replication_backlog().unwrap();
        store.project_replication_backlog().unwrap();
        for writer in ["birch", "cedar"] {
            for index in 0..20 {
                let subject = format!("note/{writer}-private-{index}");
                assert_eq!(
                    store
                        .claims_for(&subject, Some("example.note"))
                        .unwrap()
                        .len(),
                    1,
                    "both directions must receive every later live note: {subject}"
                );
            }
        }
        assert_eq!(
            store.checkpointed_envelopes().unwrap(),
            768,
            "tombstones are not transported"
        );
    }
    assert_ne!(
        left.replication_inventory().unwrap().digest,
        right.replication_inventory().unwrap().digest,
        "checkpoint-lineage reconciliation remains separate"
    );
    assert!(
        !exchange(
            &http,
            &Local(left.clone()),
            &left.origin,
            &peer,
            &auth,
            &fleet_context
        )
        .await
        .unwrap()
        .0,
        "tombstone-only divergence must let the worker rest"
    );
    note(&left, "published-after-quiet-comparison");
    assert!(
        exchange(
            &http,
            &Local(left.clone()),
            &left.origin,
            &peer,
            &auth,
            &fleet_context
        )
        .await
        .unwrap()
        .0
    );
    assert!(
        right
            .latest_claim(
                "note/published-after-quiet-comparison",
                Some("example.note")
            )
            .unwrap()
            .is_some(),
        "a later write must still cross the unresolved tombstone prefix"
    );

    server.abort();
}

/// Measure the exceptional full-inventory path at the reported fleet size, on an isolated
/// disk-backed store. Run explicitly with --ignored --nocapture; no live database is opened.
#[test]
#[ignore = "fleet-size inventory cost measurement"]
fn fleet_size_full_inventory_fallback_cost() {
    let root = tempfile::tempdir().unwrap();
    let store = plain_open(&root.path().join("store.sqlite3"), "birch").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    for id in crate::store::test_envelope_ids("alder-retired", 0..70_000, "lineage") {
        transaction
            .execute(
                "INSERT INTO checkpoint_envelopes VALUES (?1, ?2, ?3, 0, 'checkpoint/example')",
                rusqlite::params![id.writer, id.sequence, id.hash],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    store.replica_rows_changed();
    let started = std::time::Instant::now();
    let summary = store.export_replication_summary("fleet/example").unwrap();
    let snapshot_ms = started.elapsed().as_secs_f64() * 1000.;
    let mut divergent = summary.inventory.clone();
    divergent.digest = "different".into();
    for bucket in divergent.buckets.iter_mut().take(18) {
        bucket.digest = "00".repeat(32);
    }
    let started = std::time::Instant::now();
    let compact = store
        .export_replication_exchange("fleet/example", &divergent)
        .unwrap();
    let compact_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(compact.inventory.envelopes.len(), 512);
    let started = std::time::Instant::now();
    let full = store
        .export_replication_exchange(
            "fleet/example",
            &ReplicationInventory {
                digest: "different".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let export_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = std::time::Instant::now();
    let bytes = serde_json::to_vec(&full).unwrap();
    let json_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = std::time::Instant::now();
    let compressed = deflate(&bytes).unwrap();
    let deflate_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(full.inventory.envelopes.len(), 70_000);
    assert!(full.envelopes.is_empty());
    assert!(bytes.len() < MAX_EXCHANGE_BYTES);
    eprintln!(
        "identities=70000 summary_bytes={} compact_bytes={} full_bytes={} deflated_bytes={} snapshot_ms={snapshot_ms:.2} compact_ms={compact_ms:.2} export_ms={export_ms:.2} json_ms={json_ms:.2} deflate_ms={deflate_ms:.2}",
        serde_json::to_vec(&summary).unwrap().len(),
        serde_json::to_vec(&compact).unwrap().len(),
        bytes.len(),
        compressed.len()
    );
}

#[tokio::test]
async fn stalled_replication_http_stream_is_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stalled = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _socket = socket;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    let request = replication_http_client()
        .get(format!("http://{address}/stalled"))
        .send();
    let result = tokio::time::timeout(Duration::from_secs(25), request).await;
    stalled.abort();
    assert!(matches!(result, Ok(Err(error)) if error.is_timeout()));
}

#[test]
fn signed_messages_detect_tampering_and_wrong_fleets() {
    let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[7; 32]);
    let body = br#"{"hello":"fleet"}"#;
    let headers = auth.request_headers("node-a", body).unwrap();
    assert!(
        auth.verify(&headers, "POST", EXCHANGE_PATH, body, Some("node-a"), None)
            .is_ok()
    );
    assert!(
        auth.verify(
            &headers,
            "POST",
            EXCHANGE_PATH,
            br#"{"hello":"other"}"#,
            Some("node-a"),
            None
        )
        .is_err()
    );
    let other = FleetAuth::test("48608b46-bf75-442a-a462-787085dc574e", &[7; 32]);
    assert!(
        other
            .verify(&headers, "POST", EXCHANGE_PATH, body, Some("node-a"), None)
            .is_err()
    );
}

#[test]
fn response_signatures_bind_to_one_request() {
    let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[9; 32]);
    let body = b"response";
    let headers = auth
        .response_headers_for(EXCHANGE_PATH, "node-b", body, "request-a")
        .unwrap();
    assert!(
        auth.verify(
            &headers,
            "RESPONSE",
            EXCHANGE_PATH,
            body,
            Some("node-b"),
            Some("request-a")
        )
        .is_ok()
    );
    assert!(
        auth.verify(
            &headers,
            "RESPONSE",
            EXCHANGE_PATH,
            body,
            Some("node-b"),
            Some("request-b")
        )
        .is_err()
    );
}

#[test]
fn fleet_secret_loading_accepts_private_raw_or_hex_files_only() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fleet-secret");
    fs::write(&path, [3_u8; 32]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    FleetAuth::load("1f91ca65-7793-48cc-866e-ac15690130e1", &path).unwrap();

    fs::write(&path, format!("{}\n", hex::encode([4_u8; 32]))).unwrap();
    FleetAuth::load("1f91ca65-7793-48cc-866e-ac15690130e1", &path).unwrap();

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(FleetAuth::load("1f91ca65-7793-48cc-866e-ac15690130e1", &path).is_err());
}

#[tokio::test]
async fn an_authenticated_failure_has_a_request_bound_signature() {
    let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[5; 32]);
    let body = Bytes::from_static(b"not json");
    let request_digest = FleetAuth::body_digest(&body);
    let response = receive_exchange(
        State(PeerState {
            backend: Local(Arc::new(plain_memory("target").unwrap())),
            node: "target".into(),
            auth: auth.clone(),
            fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
            outbound_notify: watch::channel(0_u64).0,
        }),
        auth.request_headers("source", &body).unwrap(),
        body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    auth.verify(
        &headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &bytes,
        Some("target"),
        Some(&request_digest),
    )
    .unwrap();
}

#[tokio::test]
async fn the_peer_route_accepts_an_exchange_above_axums_default_body_limit() {
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet, &[5; 32]);
    let state = PeerState {
        backend: Local(Arc::new(plain_memory("target").unwrap())),
        node: "target".into(),
        auth: auth.clone(),
        fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
        outbound_notify: watch::channel(0_u64).0,
    };
    let exchange = ReplicationExchange {
        projection_digests: Default::default(),
        peer: "source".into(),
        fleet_id: fleet.into(),
        schema_digest: Plain.schema_digest(),
        authority_digest: String::new(),
        graph_digest: String::new(),
        inventory: ReplicationInventory::default(),
        envelopes: Vec::new(),
        signature_requests: Vec::new(),
        signatures: Vec::new(),
    };
    let mut body = serde_json::to_vec(&exchange).unwrap();
    body.resize(2 * 1024 * 1024 + 1, b' ');
    let request_digest = FleetAuth::body_digest(&body);
    let mut request = Request::builder()
        .method("POST")
        .uri(EXCHANGE_PATH)
        .body(Body::from(body.clone()))
        .unwrap();
    request
        .headers_mut()
        .extend(auth.request_headers("source", &body).unwrap());
    let response = peer_router(state, Router::new())
        .oneshot(request)
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let headers = response.headers().clone();
    let response_body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    auth.verify(
        &headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &response_body,
        Some("target"),
        Some(&request_digest),
    )
    .unwrap();
}

#[test]
fn exchange_bodies_deflate_and_refuse_a_body_that_inflates_too_far() {
    let body = serde_json::to_vec(&serde_json::json!({"envelopes": vec!["same"; 1_000]})).unwrap();
    let compressed = deflate(&body).unwrap();
    assert!(compressed.len() * 10 < body.len());
    assert_eq!(inflate(&compressed).unwrap(), body);

    let bomb = deflate(&vec![0_u8; MAX_EXCHANGE_BYTES + 1]).unwrap();
    assert!(bomb.len() < 1024 * 1024);
    assert!(inflate(&bomb).is_err());
    assert!(inflate(b"not deflate").is_err());

    let mut headers = HeaderMap::new();
    assert!(!accepts_deflate(&headers));
    headers.insert(
        "accept-encoding",
        HeaderValue::from_static("gzip, Deflate;q=0.5"),
    );
    assert!(accepts_deflate(&headers));
    headers.insert(
        "accept-encoding",
        HeaderValue::from_static("gzip, deflated"),
    );
    assert!(!accepts_deflate(&headers));
}

#[tokio::test]
async fn the_peer_route_deflates_only_for_a_requester_that_asks() {
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet, &[5; 32]);
    // Enough envelopes that the answer to an empty inventory is worth compressing.
    let target = Arc::new(plain_memory("target").unwrap());
    target.bind_fleet(fleet).unwrap();
    for index in 0..300 {
        target
            .append_claim(&ClaimInput {
                subject: format!("host/peer-{index}"),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let state = PeerState {
        backend: Local(target),
        node: "target".into(),
        auth: auth.clone(),
        fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
        outbound_notify: watch::channel(0_u64).0,
    };
    let exchange = ReplicationExchange {
        projection_digests: Default::default(),
        peer: "source".into(),
        fleet_id: fleet.into(),
        schema_digest: Plain.schema_digest(),
        authority_digest: String::new(),
        graph_digest: String::new(),
        inventory: ReplicationInventory::default(),
        envelopes: Vec::new(),
        signature_requests: Vec::new(),
        signatures: Vec::new(),
    };
    let body = serde_json::to_vec(&exchange).unwrap();
    let request_digest = FleetAuth::body_digest(&body);
    // An older build sends plain JSON and does not ask for compression; a new one sends a
    // compressed request once the peer has answered compressed, and always asks.
    for (compress, ask) in [(false, false), (false, true), (true, true)] {
        let mut request = Request::builder()
            .method("POST")
            .uri(EXCHANGE_PATH)
            .body(Body::from(if compress {
                deflate(&body).unwrap()
            } else {
                body.clone()
            }))
            .unwrap();
        request
            .headers_mut()
            .extend(auth.request_headers("source", &body).unwrap());
        if compress {
            request.headers_mut().insert(
                "content-encoding",
                HeaderValue::from_static(EXCHANGE_ENCODING),
            );
        }
        if ask {
            request.headers_mut().insert(
                "accept-encoding",
                HeaderValue::from_static(EXCHANGE_ENCODING),
            );
        }
        let response = peer_router(state.clone(), Router::new())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert!(
            accepts_deflate(&headers),
            "a new build takes compressed requests"
        );
        assert_eq!(deflated(&headers), ask);
        let response_body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_body = if ask {
            inflate(&response_body).unwrap()
        } else {
            response_body.to_vec()
        };
        assert!(response_body.len() >= DEFLATE_MIN_BYTES);
        auth.verify(
            &headers,
            "RESPONSE",
            EXCHANGE_PATH,
            &response_body,
            Some("target"),
            Some(&request_digest),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn signed_peer_exchange_moves_new_authority_in_both_directions() {
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet, &[6; 32]);
    let source = Arc::new(plain_memory("source").unwrap());
    let target = Arc::new(plain_memory("target").unwrap());
    source.bind_fleet(fleet).unwrap();
    target.bind_fleet(fleet).unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("source-up".into()),
        })
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = PeerState {
        backend: Local(target.clone()),
        node: "target".into(),
        auth: auth.clone(),
        fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
        outbound_notify: watch::channel(0_u64).0,
    };
    let connection_ports = Arc::new(Mutex::new(BTreeSet::new()));
    let observed_ports = connection_ports.clone();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route(EXCHANGE_PATH, post(receive_exchange))
                .layer(axum::middleware::from_fn(
                    move |request: Request<Body>, next: axum::middleware::Next| {
                        let observed_ports = observed_ports.clone();
                        async move {
                            if let Some(axum::extract::ConnectInfo(address)) =
                                request
                                    .extensions()
                                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                            {
                                observed_ports.lock().unwrap().insert(address.port());
                            }
                            next.run(request).await
                        }
                    },
                ))
                .with_state(state)
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    let peer = PeerConfig {
        name: "target".into(),
        url: format!("http://{address}"),
    };
    let http = replication_http_client();
    let pushed = exchange(
        &http,
        &Local(source.clone()),
        "source",
        &peer,
        &auth,
        &FleetContext::legacy(BTreeSet::from(["target".into()])),
    )
    .await
    .unwrap()
    .0;
    assert!(pushed, "the peer stored what this node pushed");
    assert!(
        target
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .is_some()
    );

    target
        .append_claim(&ClaimInput {
            subject: "host/target".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("target-up".into()),
        })
        .unwrap();
    let pulled = exchange(
        &http,
        &Local(source.clone()),
        "source",
        &peer,
        &auth,
        &FleetContext::legacy(BTreeSet::from(["target".into()])),
    )
    .await
    .unwrap()
    .0;
    assert!(pulled, "this node stored what the peer sent");
    assert!(
        source
            .latest_claim("host/target", Some("transport.observed"))
            .unwrap()
            .is_some()
    );
    let source_status = source.replication_status(true, Some(fleet), &[]).unwrap();
    let target_status = target.replication_status(true, Some(fleet), &[]).unwrap();
    assert_eq!(
        source_status.authority_digest,
        target_status.authority_digest
    );
    assert_eq!(
        connection_ports.lock().unwrap().len(),
        1,
        "the two-phase exchanges and later wakeup should reuse one TCP connection"
    );
    let moved = exchange(
        &replication_http_client(),
        &Local(source.clone()),
        "source",
        &peer,
        &auth,
        &FleetContext::legacy(BTreeSet::from(["target".into()])),
    )
    .await
    .unwrap()
    .0;
    assert!(
        !moved,
        "converged nodes store nothing, so the worker may rest"
    );
    assert_eq!(
        connection_ports.lock().unwrap().len(),
        2,
        "a fresh client should demonstrate the former extra dial"
    );
    let summary = source.export_replication_summary(fleet).unwrap();
    assert!(summary.inventory.envelopes.is_empty());
    assert!(!summary.inventory.digest.is_empty());
    let converged = target
        .export_replication_exchange(fleet, &summary.inventory)
        .unwrap();
    assert!(converged.inventory.envelopes.is_empty());
    assert!(converged.envelopes.is_empty());
    server.abort();
}

#[tokio::test]
async fn a_duplicate_inbound_exchange_does_not_wake_outbound_replication() {
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet, &[7; 32]);
    let source = plain_memory("source").unwrap();
    let target = Arc::new(plain_memory("target").unwrap());
    source.bind_fleet(fleet).unwrap();
    target.bind_fleet(fleet).unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("source-up".into()),
        })
        .unwrap();
    let exchange = source
        .export_replication_exchange(fleet, &ReplicationInventory::default())
        .unwrap();
    let body = Bytes::from(serde_json::to_vec(&exchange).unwrap());
    let (outbound_notify, mut outbound_wake) = watch::channel(0_u64);
    let state = PeerState {
        backend: Local(target),
        node: "target".into(),
        auth: auth.clone(),
        fleet: FleetContext::legacy(BTreeSet::from(["source".into()])),
        outbound_notify,
    };

    let first = receive_exchange(
        State(state.clone()),
        auth.request_headers("source", &body).unwrap(),
        body.clone(),
    )
    .await;
    assert!(first.status().is_success());
    outbound_wake.changed().await.unwrap();
    let _ = outbound_wake.borrow_and_update();

    let duplicate = receive_exchange(
        State(state.clone()),
        auth.request_headers("source", &body).unwrap(),
        body,
    )
    .await;
    assert!(duplicate.status().is_success());
    assert!(
        !outbound_wake.has_changed().unwrap(),
        "a duplicate receipt must not start a replication echo loop"
    );
}

#[test]
fn the_dial_set_comes_from_membership_and_skips_dial_out_members() {
    let member =
        |name: &str, state: &str, mode: &str, endpoints: Vec<Value>| crate::fleet::MemberView {
            name: name.into(),
            member_key: format!("{name}-key"),
            state: state.into(),
            mode: mode.into(),
            endpoints,
            start: 1,
            end: None,
            ended: None,
            removed_by: None,
            removal_reason: None,
        };
    let loopback = |port: u16| {
        vec![serde_json::json!({"transport": "loopback", "address": format!("127.0.0.1:{port}")})]
    };
    let view = FleetView {
        anchor: Some("a-key".into()),
        members: vec![
            member("a", "current", "listening", loopback(1)),
            member("server", "current", "listening", loopback(2)),
            member("laptop", "current", "dial-out", Vec::new()),
            member("gone", "ended", "listening", loopback(3)),
            member("quiet", "current", "listening", Vec::new()),
        ],
        legacy_removed: vec!["old".into()],
    };
    let config = |name: &str, port: u16| PeerConfig {
        name: name.into(),
        url: format!("http://127.0.0.1:{port}"),
    };
    let targets = dial_targets(
        &view,
        "a",
        &[
            config("server", 9002),
            config("laptop", 9003),
            config("gone", 9004),
            config("old", 9005),
            config("legacy", 9006),
            PeerConfig {
                name: "inbound".into(),
                url: String::new(),
            },
        ],
        LocalTransports::default(),
    );
    assert_eq!(
        targets,
        BTreeMap::from([
            (
                "server".to_owned(),
                vec![
                    Route::Http("http://127.0.0.1:9002".into()),
                    Route::Http("http://127.0.0.1:2".into())
                ]
            ),
            (
                "legacy".to_owned(),
                vec![Route::Http("http://127.0.0.1:9006".into())]
            ),
        ])
    );
}

fn fleet_claim(store: &Store, kind: &str, subject: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn member_context(
    store: &Store,
    own: &MemberKey,
    bootstrap: &[&str],
    config_peers: &[&str],
    legacy: bool,
) -> FleetContext {
    FleetContext::member(store, own, bootstrap, config_peers, legacy)
}

#[tokio::test]
async fn members_exchange_with_signatures_and_a_removed_member_is_refused() {
    let fleet = "3b241101-e2bb-4255-8caf-4136c566a962";
    let secret = [9_u8; 32];
    let anchor = Arc::new(MemberKey::generate().unwrap().0);
    let member = Arc::new(MemberKey::generate().unwrap().0);
    let a = Arc::new(plain_memory("a").unwrap());
    let b = Arc::new(plain_memory("b").unwrap());
    for (store, key) in [(&a, &anchor), (&b, &member)] {
        store.bind_fleet(fleet).unwrap();
        store.pin_fleet_anchor(anchor.public()).unwrap();
        store.set_member_key(Some(key.clone())).unwrap();
    }
    let admitted = |name: &str, key: &MemberKey, via: &str| {
        serde_json::json!({
            "fleet_id": fleet, "member_key": key.public(), "via": via, "mode": "listening"
        })
        .as_object()
        .map(|fields| {
            fleet_claim(
                &a,
                "fleet.member-admitted",
                &format!("host/{name}"),
                Value::Object(fields.clone()),
            )
        })
    };
    admitted("a", &anchor, "anchor");
    admitted("b", &member, "invite");

    // a listens as a member, without legacy acceptance.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let a_fleet = member_context(&a, &anchor, &[], &[], false);
    let state = PeerState {
        backend: Local(a.clone()),
        node: "a".into(),
        auth: FleetAuth::test(fleet, &secret).with_member_key(Some(anchor.clone())),
        fleet: a_fleet.clone(),
        outbound_notify: watch::channel(0_u64).0,
    };
    let server = tokio::spawn(async move {
        axum::serve(listener, peer_router(state, Router::new()))
            .await
            .unwrap();
    });
    let peer = PeerConfig {
        name: "a".into(),
        url: format!("http://{address}"),
    };
    let http = replication_http_client();
    let b_auth = FleetAuth::test(fleet, &secret).with_member_key(Some(member.clone()));
    // b knows nothing yet but its anchor, which is enough to trust a's answer.
    let b_fleet = member_context(&b, &member, &[anchor.public()], &[], false);
    exchange(&http, &Local(b.clone()), "b", &peer, &b_auth, &b_fleet)
        .await
        .unwrap();
    assert!(matches!(
        b.fleet_membership().unwrap().state("b"),
        crate::fleet::MemberState::Current(_)
    ));

    // Without its member key, b is refused.
    let unsigned = exchange(
        &http,
        &Local(b.clone()),
        "b",
        &peer,
        &FleetAuth::test(fleet, &secret),
        &b_fleet,
    )
    .await
    .unwrap_err();
    assert!(
        unsigned.to_string().contains("member-signature-required"),
        "{unsigned:#}"
    );

    // a removes b; b's next exchange gets a signed refusal that names its key.
    fleet_claim(
        &a,
        "fleet.member-removed",
        "host/b",
        serde_json::json!({"member_key": member.public(), "high_water": 100, "reason": "test"}),
    );
    *a_fleet.view.write().unwrap() = a.fleet_view().unwrap();
    *b_fleet.view.write().unwrap() = b.fleet_view().unwrap();
    let refused = exchange(&http, &Local(b.clone()), "b", &peer, &b_auth, &b_fleet)
        .await
        .unwrap_err();
    let removed = refused
        .downcast_ref::<RemovedFromFleet>()
        .expect("a signed refusal naming b's key");
    assert_eq!(removed.code, "member-removed");

    // Another machine without a key, posing as b or as an unknown name, is refused too.
    // It accepts a's answers as a legacy config peer, so it sees a's refusal itself.
    let stranger = FleetContext::legacy(BTreeSet::from(["a".into()]));
    for name in ["b", "stranger"] {
        let error = exchange(
            &http,
            &Local(b.clone()),
            name,
            &peer,
            &FleetAuth::test(fleet, &secret),
            &stranger,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("403"), "{error:#}");
    }
    server.abort();
}

mod retry_tests {
    use super::*;

    #[tokio::test]
    async fn authenticated_activity_does_not_start_redundant_healthy_exchanges() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fleet_id = "d706eab5-433c-474b-ae43-fd2a7891d85d";
        let auth = FleetAuth::test(fleet_id, &[42; 32]);
        let local = Arc::new(plain_memory("harbor").unwrap());
        let remote = Arc::new(plain_memory("beacon").unwrap());
        for store in [&local, &remote] {
            store.bind_fleet(fleet_id).unwrap();
        }
        let fleet = FleetContext::legacy(BTreeSet::from(["beacon".into()]));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let app = peer_router(
            PeerState {
                backend: Local(remote),
                node: "beacon".into(),
                auth: auth.clone(),
                fleet: FleetContext::legacy(BTreeSet::from(["harbor".into()])),
                outbound_notify: watch::channel(0).0,
            },
            Router::new(),
        )
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                counter.fetch_add(1, Ordering::Relaxed);
                async move { next.run(request).await }
            },
        ));
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let (_route_tx, routes) = watch::channel(vec![Route::Http(url)]);
        let (notify, _) = watch::channel(0);
        let dialer = tokio::spawn(dial_peer(
            Local(local),
            "harbor".into(),
            "beacon".into(),
            routes,
            auth,
            fleet.clone(),
            notify.subscribe(),
        ));
        // The first exchange moves both nodes' fleet bindings, so the dialer starts a follow-up
        // exchange at once; on a loaded machine that can come long after the first answer.
        // Measure from the healthy wait: a completed exchange, then no request for longer than
        // the coalescing window.
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut seen = requests.load(Ordering::Relaxed);
            loop {
                tokio::time::sleep(3 * REPLICATION_WAKE_COALESCE).await;
                let now = requests.load(Ordering::Relaxed);
                if now == seen && fleet.activity.read().unwrap().contains_key("beacon") {
                    break;
                }
                seen = now;
            }
        })
        .await
        .unwrap();
        let before = requests.load(Ordering::Relaxed);
        // A successful client read/probe proves peer life, but it does not change the graph.
        fleet.note_activity("beacon");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            requests.load(Ordering::Relaxed),
            before,
            "authenticated activity interrupted healthy anti-entropy coalescing"
        );
        dialer.abort();
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn signs_of_life_during_a_failed_attempt_are_not_lost_before_the_retry_wait() {
        let fleet = FleetContext::legacy(BTreeSet::new());
        let (_route_tx, mut routes) = watch::channel(Vec::new());
        let mut inbound = fleet.inbound_changed.subscribe();
        let mut activity = fleet.activity_changed.subscribe();
        let mut connectivity = fleet.connectivity_changed.subscribe();
        let http = replication_http_client();
        for completed_exchange in [false, true] {
            let attempt_started = tokio::time::Instant::now();
            tokio::time::advance(Duration::from_secs(1)).await;
            if completed_exchange {
                fleet
                    .inbound
                    .write()
                    .unwrap()
                    .insert("traveller".into(), tokio::time::Instant::now());
                fleet
                    .inbound_changed
                    .send_modify(|generation| *generation += 1);
            } else {
                fleet.note_activity("traveller");
            }
            // The outbound request fails later, after a coalesced notification was read.
            inbound.borrow_and_update();
            activity.borrow_and_update();
            tokio::time::advance(REPLICATION_EXCHANGE_TIMEOUT).await;
            assert!(
                wait_peer_retry(
                    &Local(Arc::new(plain_memory("retry-fixture").unwrap())),
                    crate::store::now_ms(),
                    Duration::from_secs(3600),
                    &mut routes,
                    &mut inbound,
                    &mut activity,
                    &mut connectivity,
                    &fleet,
                    "traveller",
                    attempt_started,
                    None,
                    &http,
                )
                .await
            );
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        // Old activity must not reset the next failed attempt's backoff.
        assert!(
            !wait_peer_retry(
                &Local(Arc::new(plain_memory("retry-fixture").unwrap())),
                crate::store::now_ms(),
                Duration::from_secs(3600),
                &mut routes,
                &mut inbound,
                &mut activity,
                &mut connectivity,
                &fleet,
                "traveller",
                tokio::time::Instant::now(),
                None,
                &http,
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_silent_http_return_caps_backoff_without_an_inventory_export() {
        let store = Arc::new(plain_memory("beacon").unwrap());
        let fleet = FleetContext::legacy(BTreeSet::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(
                listener,
                peer_router(
                    PeerState {
                        backend: Local(store),
                        node: "beacon".into(),
                        auth: FleetAuth::test("invented-fleet", &[42; 32]),
                        fleet: fleet.clone(),
                        outbound_notify: watch::channel(0).0,
                    },
                    Router::new(),
                ),
            )
            .into_future(),
        );
        let (_route_tx, mut routes) = watch::channel(vec![Route::Http(url.clone())]);
        let mut inbound = fleet.inbound_changed.subscribe();
        let mut activity = fleet.activity_changed.subscribe();
        let mut connectivity = fleet.connectivity_changed.subscribe();
        let http = replication_http_client();
        let previous = (
            url.clone(),
            tokio::time::Instant::now() - Duration::from_secs(120),
        );
        let started = tokio::time::Instant::now();
        assert!(
            !tokio::time::timeout(
                Duration::from_secs(35),
                wait_peer_retry(
                    &Local(Arc::new(plain_memory("retry-fixture").unwrap())),
                    crate::store::now_ms(),
                    Duration::from_secs(3600),
                    &mut routes,
                    &mut inbound,
                    &mut activity,
                    &mut connectivity,
                    &fleet,
                    "traveller",
                    started,
                    Some(&previous),
                    &http,
                )
            )
            .await
            .unwrap()
        );
        assert!(
            fleet.activity.read().unwrap().is_empty(),
            "HEAD must not refresh authenticated activity"
        );
        assert!(
            fleet.inbound.read().unwrap().is_empty(),
            "HEAD must not invoke receive_exchange"
        );

        // Without a successful signed exchange for five minutes, absence stays quiet.
        inbound.borrow_and_update();
        let old = (url, tokio::time::Instant::now() - PEER_PROBE_WINDOW);
        tokio::time::pause();
        assert!(
            !wait_peer_retry(
                &Local(Arc::new(plain_memory("retry-fixture").unwrap())),
                crate::store::now_ms(),
                Duration::from_secs(3600),
                &mut routes,
                &mut inbound,
                &mut activity,
                &mut connectivity,
                &fleet,
                "traveller",
                tokio::time::Instant::now(),
                Some(&old),
                &http,
            )
            .await
        );
        server.abort();
    }

    #[test]
    fn refused_routes_allow_at_most_three_attempts_per_hour_from_the_first_refusal() {
        for jitter in [0, 200, 400, u16::MAX] {
            let delay = fabric_refusal_delay_with_jitter(jitter);
            assert!(delay >= Duration::from_secs(24 * 60));
            assert!(delay <= Duration::from_secs(36 * 60));
            let attempts = 1 + 3600 / delay.as_secs();
            assert!(attempts <= 3, "{attempts} attempts per hour");
        }
    }

    #[tokio::test]
    async fn isolated_leaves_refuse_direct_fabric_routes_and_converge_through_the_hub() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let names = ["indigo", "amber", "cobalt"];
        let fleet_id = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
        let auth = FleetAuth::test(fleet_id, &[37; 32]);
        let stores = names.map(|name| {
            let store =
                Arc::new(plain_open(&root.path().join(format!("{name}.db")), name).unwrap());
            store.bind_fleet(fleet_id).unwrap();
            store
        });
        let log = root.path().join("refusals");
        let grant = root.path().join("grant");
        let program = root.path().join("fabric");
        std::fs::write(&program, format!(
            "#!/bin/sh\nif test -f '{}'; then cat '{}'; exit 0; fi\necho \"$2\" >> '{}'\necho 'peer not permitted for service st3-peer-v1' >&2\nexit 1\n",
            grant.display(), grant.display(), log.display()
        )).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let contexts = names.map(|name| {
            let mut context = FleetContext::legacy(
                names
                    .iter()
                    .filter(|peer| **peer != name)
                    .map(|peer| (*peer).into())
                    .collect(),
            );
            context.fabric = Some(Fabric::new(program.clone()));
            context
        });
        let wakes = names.map(|_| watch::channel(0).0);
        let mut tasks = Vec::new();
        let mut addresses = Vec::new();
        for index in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addresses.push(listener.local_addr().unwrap());
            let state = PeerState {
                backend: Local(stores[index].clone()),
                node: names[index].into(),
                auth: auth.clone(),
                fleet: contexts[index].clone(),
                outbound_notify: wakes[index].clone(),
            };
            tasks.push(tokio::spawn(async move {
                axum::serve(listener, peer_router(state, Router::new()))
                    .await
                    .unwrap();
            }));
        }
        let mut routes = Vec::new();
        for (from, to) in [(1, 0), (2, 0), (1, 2), (2, 1)] {
            let route = if to == 0 {
                Route::Http(format!("http://{}", addresses[to]))
            } else {
                Route::Fabric {
                    node: names[to].into(),
                    protocol: "st3-peer-v1".into(),
                }
            };
            let (sender, receiver) = watch::channel(vec![route]);
            routes.push(sender);
            tasks.push(tokio::spawn(dial_peer(
                Local(stores[from].clone()),
                names[from].into(),
                names[to].into(),
                receiver,
                auth.clone(),
                contexts[from].clone(),
                wakes[from].subscribe(),
            )));
        }
        let write = |index: usize, note: &str| {
            stores[index]
                .append_claim(&crate::claim::ClaimInput {
                    subject: format!("daemon/{note}"),
                    kind: "daemon.diagnostic".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("severity".into(), Value::String("warning".into())),
                        (
                            "code".into(),
                            Value::String("isolated-refusal-proof".into()),
                        ),
                        ("reason".into(), Value::String(note.into())),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            wakes[index].send_modify(|generation| *generation += 1);
        };
        // The hub cannot dial an isolated leaf, so a leaf that finished its exchanges before the
        // other leaf pushed learns of it at its next contact. Stand in for that contact while
        // waiting: an inbound change wakes the hub dialers, never a refused route.
        async fn converge(stores: &[Arc<Store>], contexts: &[FleetContext]) {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    for context in contexts {
                        context
                            .inbound_changed
                            .send_modify(|generation| *generation += 1);
                    }
                    let snapshots = stores
                        .iter()
                        .map(|store| store.replication_status(true, None, &[]).unwrap())
                        .collect::<Vec<_>>();
                    if snapshots.iter().all(|snapshot| {
                        snapshot.authority_digest == snapshots[0].authority_digest
                            && snapshot.graph_digest == snapshots[0].graph_digest
                    }) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("the leaves must converge through the hub");
        }
        for index in 0..3 {
            write(index, &format!("initial-{}", names[index]));
        }
        converge(&stores, &contexts).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .count()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for (from, to) in [(1, 2), (2, 1)] {
            let status = stores[from]
                .replication_status(true, Some(fleet_id), &[names[to].into()])
                .unwrap();
            assert_eq!(status.peers[0].status, "refused");
            assert!(status.peers[0].last_error.is_none());
            assert!(
                status.peers[0]
                    .refusal_reason
                    .as_ref()
                    .unwrap()
                    .contains("that member's Fabric grants")
            );
        }
        // Busy graph, network and online events must leave both refused routes asleep.
        for round in 0..8 {
            for index in 0..3 {
                write(index, &format!("busy-{round}-{}", names[index]));
                contexts[index]
                    .connectivity_changed
                    .send_modify(|generation| *generation += 1);
                contexts[index]
                    .online
                    .write()
                    .unwrap()
                    .insert("cobalt".into(), tokio::time::Instant::now());
                contexts[index]
                    .inbound_changed
                    .send_modify(|generation| *generation += 1);
            }
            converge(&stores, &contexts).await;
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
        // Advance an hour on the real dialers. Let shell I/O finish in real time between
        // advances so the command timeout measures a dial, not our simulated hour.
        tokio::time::pause();
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(10 * 60)).await;
            tokio::time::resume();
            tokio::time::sleep(Duration::from_millis(30)).await;
            tokio::time::pause();
        }
        tokio::time::resume();
        let attempts = std::fs::read_to_string(&log).unwrap();
        for name in ["amber", "cobalt"] {
            let count = attempts.lines().filter(|line| *line == name).count();
            assert!((2..=3).contains(&count), "{name}: {count} dials in an hour");
            println!(
                "isolated refusal proof: {name}: {count} direct attempts in a simulated hour; leaves converged through indigo"
            );
        }
        // A refused Fabric route must not hold up another route to the same member.
        routes[2].send_replace(vec![
            Route::Fabric {
                node: "cobalt".into(),
                protocol: "st3-peer-v1".into(),
            },
            Route::Http(format!("http://{}", addresses[2])),
        ]);
        tokio::time::timeout(Duration::from_secs(3), async {
            while stores[1]
                .replication_status(true, Some(fleet_id), &["cobalt".into()])
                .unwrap()
                .peers[0]
                .status
                != "up"
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a refused Fabric route must not block the HTTP fallback");
        // A changed membership route is usable immediately, despite the grant cooldown.
        let before_route_change = stores[1].replication_peer_last_success("cobalt").unwrap();
        std::fs::write(&grant, addresses[2].to_string()).unwrap();
        routes[2].send_replace(vec![Route::Fabric {
            node: "cobalt-new-route".into(),
            protocol: "st3-peer-v1".into(),
        }]);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if stores[1]
                    .replication_status(true, Some(fleet_id), &["cobalt".into()])
                    .unwrap()
                    .peers[0]
                    .status
                    == "up"
                    && stores[1].replication_peer_last_success("cobalt").unwrap()
                        > before_route_change
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a changed route must interrupt the refusal cooldown");
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    #[test]
    fn absent_members_cost_a_few_attempts_per_hour_after_exponential_backoff() {
        // Use the shortest jitter so this bounds the most aggressive retry schedule.
        for away in [Duration::from_secs(120), Duration::from_secs(4 * 3600)] {
            let mut elapsed = Duration::ZERO;
            let mut attempts = 0;
            while elapsed < away {
                elapsed += PeerBackoff::delay(attempts, 0);
                attempts += 1;
            }
            assert!(
                attempts <= if away.as_secs() == 120 { 9 } else { 20 },
                "{attempts} retries in {away:?}"
            );
        }
        assert_ne!(PeerBackoff::delay(10, 0), PeerBackoff::delay(10, 400));
    }

    #[tokio::test(start_paused = true)]
    async fn an_inbound_return_or_changed_route_interrupts_even_hours_of_absence() {
        let http = replication_http_client();
        let fleet = FleetContext::legacy(BTreeSet::new());
        let (route_tx, mut routes) = watch::channel(vec![Route::Http("http://127.0.0.1:1".into())]);
        let mut inbound = fleet.inbound_changed.subscribe();
        let mut activity = fleet.activity_changed.subscribe();
        let mut connectivity = fleet.connectivity_changed.subscribe();
        for away in [Duration::from_secs(120), Duration::from_secs(4 * 3600)] {
            tokio::time::advance(away).await;
            let backend = Local(Arc::new(plain_memory("retry-fixture").unwrap()));
            let wait = wait_peer_retry(
                &backend,
                crate::store::now_ms(),
                Duration::from_secs(300),
                &mut routes,
                &mut inbound,
                &mut activity,
                &mut connectivity,
                &fleet,
                "traveller",
                tokio::time::Instant::now(),
                None,
                &http,
            );
            tokio::pin!(wait);
            assert!(futures_util::poll!(&mut wait).is_pending());
            // Another peer returning must not reset this member's backoff.
            fleet
                .inbound
                .write()
                .unwrap()
                .insert("other".into(), tokio::time::Instant::now());
            fleet
                .inbound_changed
                .send_modify(|generation| *generation += 1);
            assert!(futures_util::poll!(&mut wait).is_pending());
            fleet
                .inbound
                .write()
                .unwrap()
                .insert("traveller".into(), tokio::time::Instant::now());
            fleet
                .inbound_changed
                .send_modify(|generation| *generation += 1);
            assert!(wait.await);
        }
        tokio::time::advance(Duration::from_millis(1)).await;
        let backend = Local(Arc::new(plain_memory("retry-fixture").unwrap()));
        let wait = wait_peer_retry(
            &backend,
            crate::store::now_ms(),
            Duration::from_secs(300),
            &mut routes,
            &mut inbound,
            &mut activity,
            &mut connectivity,
            &fleet,
            "traveller",
            tokio::time::Instant::now(),
            None,
            &http,
        );
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        route_tx.send_replace(vec![Route::Http("http://127.0.0.1:2".into())]);
        assert!(wait.await);
    }

    #[tokio::test]
    async fn isolated_outbound_only_node_converges_after_two_minutes_and_four_hours() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fleet_id = "d706eab5-433c-474b-ae43-fd2a7891d85d";
        let auth = FleetAuth::test(fleet_id, &[41; 32]);
        let root = tempfile::tempdir().unwrap();
        let names = ["harbor", "beacon", "traveller"];
        let stores = names.map(|name| {
            let store =
                Arc::new(plain_open(&root.path().join(format!("{name}.db")), name).unwrap());
            store.bind_fleet(fleet_id).unwrap();
            store
        });
        let contexts = names.map(|name| {
            FleetContext::legacy(
                names
                    .iter()
                    .filter(|peer| **peer != name)
                    .map(|peer| (*peer).into())
                    .collect(),
            )
        });
        let wakes = names.map(|_| watch::channel(0).0);
        let mut tasks = Vec::new();
        let mut addresses = Vec::new();
        let mut shutdowns = Vec::new();
        for index in 0..2 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addresses.push(format!("http://{}", listener.local_addr().unwrap()));
            let state = PeerState {
                backend: Local(stores[index].clone()),
                node: names[index].into(),
                auth: auth.clone(),
                fleet: contexts[index].clone(),
                outbound_notify: wakes[index].clone(),
            };
            let (shutdown, stopped) = tokio::sync::oneshot::channel();
            shutdowns.push(shutdown);
            tasks.push(tokio::spawn(async move {
                axum::serve(listener, peer_router(state, Router::new()))
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            }));
        }
        // The traveller accepts no inbound requests. The rejector counts costly redundant
        // attempts, while its real node can initiate authenticated exchanges to both servers.
        let rejector = TcpListener::bind("127.0.0.1:0").await.unwrap();
        addresses.push(format!("http://{}", rejector.local_addr().unwrap()));
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok((stream, _)) = rejector.accept().await {
                count.fetch_add(1, Ordering::Relaxed);
                drop(stream);
            }
        }));
        let start = |from: usize, to: usize| {
            let (routes, receiver) = watch::channel(vec![Route::Http(addresses[to].clone())]);
            let task = tokio::spawn(dial_peer(
                Local(stores[from].clone()),
                names[from].into(),
                names[to].into(),
                receiver,
                auth.clone(),
                contexts[from].clone(),
                wakes[from].subscribe(),
            ));
            (routes, task)
        };
        let mut dialers = vec![start(0, 1), start(1, 0), start(0, 2), start(1, 2)];
        let mut outbound = vec![start(2, 0), start(2, 1)];
        let write = |index: usize, note: &str| {
            stores[index]
                .append_claim(&crate::claim::ClaimInput {
                    subject: format!("daemon/{note}"),
                    kind: "daemon.diagnostic".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("severity".into(), Value::String("warning".into())),
                        ("code".into(), Value::String("isolated-proof".into())),
                        ("reason".into(), Value::String(note.into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            wakes[index].send_modify(|generation| *generation += 1);
        };
        async fn converge(stores: &[Arc<Store>]) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let digests = stores
                    .iter()
                    .map(|store| {
                        store
                            .replication_status(true, None, &[])
                            .unwrap()
                            .authority_digest
                    })
                    .collect::<BTreeSet<_>>();
                if digests.len() == 1 {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "isolated nodes did not converge within ten seconds"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        write(0, "initial-harbor");
        write(1, "initial-beacon");
        write(2, "initial-traveller");
        converge(&stores).await;
        for (label, away) in [("minutes", 120), ("hours", 4 * 3600)] {
            for (_, task) in outbound.drain(..) {
                task.abort();
                let _ = task.await;
            }
            let before = attempts.load(Ordering::Relaxed);
            write(0, &format!("{label}-harbor"));
            write(1, &format!("{label}-beacon"));
            write(2, &format!("{label}-traveller"));
            // Run actual worker timers over minutes and hours on a virtual clock. Local
            // wakes throughout the absence must not undo the failed peers' backoff.
            tokio::time::pause();
            for _ in 0..away / 10 {
                tokio::time::advance(Duration::from_secs(10)).await;
                for wake in &wakes[..2] {
                    wake.send_modify(|generation| *generation += 1);
                }
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            }
            tokio::time::resume();
            let retries = attempts.load(Ordering::Relaxed) - before;
            assert!(
                retries <= if away == 120 { 20 } else { 40 },
                "{retries} connection attempts in {away}s"
            );
            for server in &stores[..2] {
                server.age_replication_peer_for_test("traveller");
                let status = server
                    .replication_status(true, Some(fleet_id), &["traveller".into()])
                    .unwrap();
                assert_eq!(status.peers[0].status, "last-seen");
                // The servers' failed dials to the traveller's old address are evidence, kept
                // with their time until the traveller exchanges again.
                assert_eq!(
                    status.peers[0].last_error.is_some(),
                    status.peers[0].last_failure_at_unix_ms.is_some()
                );
            }
            // A changed source address is irrelevant: only the returning side needs to
            // reach the stable servers. Both queues drain despite server hour-long backoff.
            outbound = vec![start(2, 0), start(2, 1)];
            converge(&stores).await;
            let before = attempts.load(Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(
                attempts.load(Ordering::Relaxed),
                before,
                "recent inbound exchange opened a redundant connection"
            );
        }
        for (_, task) in outbound.drain(..) {
            task.abort();
            let _ = task.await;
        }
        // An always-on server uses exactly the same absence policy. Stop its listener and
        // dialers for four virtual hours; return on a different port, while its neighbor
        // retains the old address and an hour-long retry. Its own announcement converges.
        let (_, harbor_dialer) = dialers.remove(0);
        harbor_dialer.abort();
        let _ = harbor_dialer.await;
        let (_, harbor_dialer) = dialers.remove(1);
        harbor_dialer.abort();
        let _ = harbor_dialer.await;
        shutdowns.remove(0).send(()).unwrap();
        tasks.remove(0).await.unwrap();
        write(0, "server-offline-queued");
        write(1, "server-peer-queued");
        tokio::time::pause();
        for _ in 0..4 * 3600 / 10 {
            tokio::time::advance(Duration::from_secs(10)).await;
            wakes[1].send_modify(|generation| *generation += 1);
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
        tokio::time::resume();
        stores[1].age_replication_peer_for_test("harbor");
        assert_eq!(
            stores[1]
                .replication_status(true, Some(fleet_id), &["harbor".into()])
                .unwrap()
                .peers[0]
                .status,
            "last-seen"
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert_ne!(
            format!("http://{}", listener.local_addr().unwrap()),
            addresses[0]
        );
        let state = PeerState {
            backend: Local(stores[0].clone()),
            node: names[0].into(),
            auth: auth.clone(),
            fleet: contexts[0].clone(),
            outbound_notify: wakes[0].clone(),
        };
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, peer_router(state, Router::new()))
                .await
                .unwrap();
        }));
        dialers.push(start(0, 1));
        converge(&stores[..2]).await;
        for (_, task) in dialers.drain(..) {
            task.abort();
        }
        for task in tasks {
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_network_change_refreshes_tailnet_without_waiting_for_the_minute_timer() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let polls = root.path().join("polls");
        let program = root.path().join("tailnet");
        std::fs::write(&program, format!(
            "#!/bin/sh\nprintf '%s\\n' poll >> '{}'\nprintf '%s\\n' '{{\"TailscaleIPs\":[]}}'\n",
            polls.display(),
        )).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (changed, connectivity) = watch::channel(0);
        let task = tokio::spawn(keep_tailnet_current(
            program,
            None,
            None,
            Endpoints::default(),
            Arc::default(),
            watch::channel(0).0,
            connectivity,
        ));
        async fn observed(path: &Path, count: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if std::fs::read_to_string(path)
                        .unwrap_or_default()
                        .lines()
                        .count()
                        >= count
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("tailnet did not refresh within two seconds");
        }
        observed(&polls, 1).await;
        changed.send_modify(|generation| *generation += 1);
        observed(&polls, 2).await;
        task.abort();
        let _ = task.await;
    }

    #[tokio::test(start_paused = true)]
    async fn fabric_online_events_reset_only_the_member_they_name() {
        let http = replication_http_client();
        let mut fleet = FleetContext::legacy(BTreeSet::new());
        let member = crate::fleet::MemberView {
            name: "traveller".into(),
            state: "current".into(),
            mode: "listening".into(),
            member_key: "member-key".into(),
            start: 0,
            end: None,
            ended: None,
            removed_by: None,
            removal_reason: None,
            endpoints: vec![
                serde_json::json!({"transport":"fabric", "node":"invented-node-id", "protocol":"sync"}),
            ],
        };
        fleet.view.write().unwrap().members.push(member);
        fleet
            .configured_fabric_peers
            .insert("configured-id".into(), "beacon".into());
        let (_route_tx, mut routes) =
            watch::channel(vec![Route::Http("http://127.0.0.1:1".into())]);
        let mut inbound = fleet.inbound_changed.subscribe();
        let mut activity = fleet.activity_changed.subscribe();
        let mut connectivity = fleet.connectivity_changed.subscribe();
        let backend = Local(Arc::new(plain_memory("retry-fixture").unwrap()));
        let wait = wait_peer_retry(
            &backend,
            crate::store::now_ms(),
            Duration::from_secs(3600),
            &mut routes,
            &mut inbound,
            &mut activity,
            &mut connectivity,
            &fleet,
            "traveller",
            tokio::time::Instant::now(),
            None,
            &http,
        );
        tokio::pin!(wait);
        // Retain the routes sender while waiting.
        assert!(futures_util::poll!(&mut wait).is_pending());
        apply_fabric_presence(
            &fleet,
            &serde_json::json!({"reset":false,"events":[{"peer_id":"invented-node-id","online":false}]}),
        );
        assert!(futures_util::poll!(&mut wait).is_pending());
        apply_fabric_presence(
            &fleet,
            &serde_json::json!({"reset":false,"events":[{"peer_id":"invented-node-id","online":true}]}),
        );
        assert!(wait.await);
        apply_fabric_presence(
            &fleet,
            &serde_json::json!({"reset":false,"events":[{"peer_id":"configured-id","online":true}]}),
        );
        assert!(fleet.online.read().unwrap().contains_key("beacon"));
    }
}

#[derive(Clone)]
struct SlowExport {
    local: Local<Store>,
    slow: Arc<std::sync::atomic::AtomicBool>,
    fail_next: Arc<std::sync::atomic::AtomicBool>,
    receives: Arc<std::sync::atomic::AtomicUsize>,
}

impl Backend for SlowExport {
    async fn export(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        if !summary_only
            && self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            anyhow::bail!("injected export failure");
        }
        if !summary_only && self.slow.swap(false, std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(25)).await;
        }
        self.local
            .export(fleet_id, inventory, summary_only, signature_requests)
            .await
    }
    async fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
    ) -> Result<ReplicationReceiveResponse> {
        let result = self
            .local
            .receive(peer, fleet_id, exchange, round_trip)
            .await?;
        self.receives
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(result)
    }
    async fn heal_answer(
        &self,
        peer: &str,
        fleet_id: &str,
        query: &ReplicationHealQuery,
    ) -> Result<ReplicationHealAnswer> {
        self.local.heal_answer(peer, fleet_id, query).await
    }
    async fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> Result<ReplicationHealStep> {
        self.local.heal_next(peer, answer).await
    }
    async fn checkpoint_manifest(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        self.local.checkpoint_manifest(request).await
    }
    async fn checkpoint_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        self.local.checkpoint_need().await
    }
    async fn adopt_checkpoint(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>> {
        self.local.adopt_checkpoint(manifest).await
    }
    async fn publish_endpoints(&self, mode: &str, endpoints: &[Value]) -> Result<()> {
        self.local.publish_endpoints(mode, endpoints).await
    }
    async fn redeem(&self, request: &JoinRequest) -> Result<Value> {
        self.local.redeem(request).await
    }
    async fn fleet_view(&self) -> Result<FleetView> {
        self.local.fleet_view().await
    }
    async fn record_failure(&self, peer: &str, status: &str, error: &str) -> Result<()> {
        self.local.record_failure(peer, status, error).await
    }
}

/// Isolated two-node proof: a cancelled response leaves admitted envelopes intact;
/// identical signed retries share backend work while a 25-second export exceeds the
/// old caller deadline. No live graph, transport or fleet configuration is used.
#[tokio::test]
async fn slow_export_returns_signed_overload_and_preserves_interrupted_receipt() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let fleet_id = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet_id, &[7; 32]);
    let left = Arc::new(plain_open(&root.path().join("birch.sqlite3"), "birch").unwrap());
    let right = Arc::new(plain_open(&root.path().join("cedar.sqlite3"), "cedar").unwrap());
    for store in [&left, &right] {
        store.bind_fleet(fleet_id).unwrap();
        store
            .append_claim(&ClaimInput {
                subject: format!("note/{}", store.origin),
                kind: "example.note".into(),
                actor: None,
                fields: BTreeMap::from([("text".into(), Value::String(store.origin.clone()))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let backend = SlowExport {
        local: Local(right.clone()),
        slow: Arc::new(AtomicBool::new(true)),
        fail_next: Arc::new(AtomicBool::new(false)),
        receives: Arc::new(AtomicUsize::new(0)),
    };
    let state = PeerState::new(
        backend.clone(),
        "cedar".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["birch".into()])),
    );
    let query = left
        .export_replication_exchange(fleet_id, &ReplicationInventory::default())
        .unwrap();
    let bytes = Bytes::from(serde_json::to_vec(&query).unwrap());
    let digest = FleetAuth::body_digest(&bytes);
    let headers = auth.request_headers("birch", &bytes).unwrap();
    let interrupted = tokio::spawn(receive_exchange(
        State(state.clone()),
        headers.clone(),
        bytes.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.receives.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    interrupted.abort();
    assert!(
        right
            .latest_claim("note/birch", Some("example.note"))
            .unwrap()
            .is_some()
    );

    let started = tokio::time::Instant::now();
    let overload = receive_exchange(State(state.clone()), headers.clone(), bytes.clone()).await;
    assert_eq!(overload.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(started.elapsed() < REPLICATION_EXCHANGE_TIMEOUT);
    let overload_headers = overload.headers().clone();
    let body = to_bytes(overload.into_body(), MAX_EXCHANGE_BYTES)
        .await
        .unwrap();
    auth.verify_sender(
        &overload_headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &body,
        Some("cedar"),
        Some(&digest),
    )
    .unwrap();
    assert!(
        auth.verify_sender(
            &overload_headers,
            "RESPONSE",
            EXCHANGE_PATH,
            &body,
            Some("cedar"),
            Some("different-request")
        )
        .is_err()
    );
    let answer: PeerResponse<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer.value["code"], "replication-overloaded");
    assert_eq!(answer.value["retry_after_ms"], 5000);
    assert_eq!(
        backend.receives.load(Ordering::Relaxed),
        1,
        "retry must reuse admitted work"
    );

    // A different request from the same peer is refused immediately without adding work.
    let changed = Bytes::from(
        serde_json::to_vec(&left.export_replication_summary(fleet_id).unwrap()).unwrap(),
    );
    let busy = receive_exchange(
        State(state.clone()),
        auth.request_headers("birch", &changed).unwrap(),
        changed,
    )
    .await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(backend.receives.load(Ordering::Relaxed), 1);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = PeerConfig {
        name: "cedar".into(),
        url: format!("http://{}", listener.local_addr().unwrap()),
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = peer_router(state, Router::new()).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            count.fetch_add(1, Ordering::Relaxed);
            async move { next.run(request).await }
        },
    ));
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let (answer, _) = post_signed(
        &replication_http_client(),
        &Local(left.clone()),
        &peer,
        "birch",
        &auth,
        &fleet,
        &query,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        requests.load(Ordering::Relaxed),
        1,
        "completed in-flight export is fetched, not restarted"
    );
    assert_eq!(backend.receives.load(Ordering::Relaxed), 1);
    Local(left.clone())
        .receive("cedar", fleet_id, &answer, None)
        .await
        .unwrap();
    for store in [&left, &right] {
        for writer in ["birch", "cedar"] {
            assert_eq!(
                store
                    .claims_for(&format!("note/{writer}"), Some("example.note"))
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    // Once an answer is delivered, a later identical query must export fresh data.
    post_signed(
        &replication_http_client(),
        &Local(left.clone()),
        &peer,
        "birch",
        &auth,
        &fleet,
        &query,
        false,
    )
    .await
    .unwrap();
    assert_eq!(backend.receives.load(Ordering::Relaxed), 2);
    assert_eq!(
        right
            .claims_for("note/birch", Some("example.note"))
            .unwrap()
            .len(),
        1
    );
    exchange(
        &replication_http_client(),
        &Local(left.clone()),
        "birch",
        &peer,
        &auth,
        &fleet,
    )
    .await
    .unwrap();
    assert_eq!(
        left.replication_inventory().unwrap().digest,
        right.replication_inventory().unwrap().digest
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn authenticated_overload_polls_have_separate_time_and_request_bounds() {
    // Direct client/server HTTP with injected clock: every response is an authenticated
    // overload; the five-second minimum permits at most 60 requests in five minutes.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[7; 32]);
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let state = PeerState::new(
        Local(Arc::new(plain_memory("cedar").unwrap())),
        "cedar".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["birch".into()])),
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = Router::new().route(
        EXCHANGE_PATH,
        post(move |body: Bytes| {
            let state = state.clone();
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                signed_overload(&state, &FleetAuth::body_digest(&body))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = PeerConfig {
        name: "cedar".into(),
        url: format!("http://{}", listener.local_addr().unwrap()),
    };
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    // Prevent automatic clock advances from racing socket readiness.
    let keep_awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let started = tokio::time::Instant::now();
    let task = tokio::spawn(async move {
        post_signed(
            &replication_http_client(),
            &Local(Arc::new(plain_memory("birch").unwrap())),
            &peer,
            "birch",
            &auth,
            &fleet,
            &plain_memory("birch")
                .unwrap()
                .export_replication_summary(auth.fleet_id())
                .unwrap(),
            false,
        )
        .await
    });
    for _ in 0..65 {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        if task.is_finished() {
            break;
        }
        tokio::time::advance(OVERLOAD_RETRY).await;
    }
    let error = task.await.unwrap().unwrap_err();
    assert!(error.is::<PeerOverloaded>(), "{error:#}");
    assert!(started.elapsed() <= EXCHANGE_POLL_BUDGET + OVERLOAD_RETRY);
    assert!(requests.load(Ordering::Relaxed) <= 60);
    assert!(requests.load(Ordering::Relaxed) > 1);
    keep_awake.abort();
    server.abort();
}

#[tokio::test]
async fn worker_phase_status_is_local_and_reports_retry_deadline() {
    let store = Arc::new(plain_memory("birch").unwrap());
    let index = store.index().unwrap();
    let attempt = crate::store::now_ms();
    for phase in ["exchange", "heal", "backoff", "idle"] {
        let retry = (phase == "backoff").then_some(Duration::from_secs(30));
        record_worker(&Local(store.clone()), "cedar", phase, attempt, retry).await;
        let status = store
            .replication_status(false, None, &["cedar".into()])
            .unwrap();
        let worker = status.peers[0].worker.as_ref().unwrap();
        assert_eq!(worker.phase, phase);
        assert_eq!(worker.last_attempt_at_unix_ms, attempt);
        assert_eq!(worker.next_retry_at_unix_ms.is_some(), retry.is_some());
    }
    assert_eq!(
        store.index().unwrap(),
        index,
        "worker state must never create replicated claims"
    );
    let fleet = FleetContext::legacy(BTreeSet::new());
    fleet.note_activity("cedar");
    assert_eq!(
        retry_delay(Duration::from_secs(3600), &fleet, "cedar"),
        Duration::from_secs(30)
    );
    assert_eq!(
        retry_delay(Duration::from_secs(3600), &fleet, "absent"),
        Duration::from_secs(3600)
    );
}

#[tokio::test]
async fn overload_requires_authentication_and_clamps_signed_retry_delays() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let query = plain_memory("birch")
        .unwrap()
        .export_replication_summary(auth.fleet_id())
        .unwrap();
    // Each request gets one controlled wire answer: unsigned, forged, replayed, then
    // authenticated delays below, within, and above the permitted range.
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let signing = auth.clone();
    let app = Router::new().route(
        EXCHANGE_PATH,
        post(move |body: Bytes| {
            let index = count.fetch_add(1, Ordering::Relaxed);
            let signing = signing.clone();
            async move {
                let millis = [0, 0, 0, 0, 4999, 10_000, 30_001, u64::MAX][index];
                let envelope = PeerResponse::new(
                    "cedar",
                    0,
                    serde_json::json!({
                        "code": "replication-overloaded", "retry_after_ms": millis
                    }),
                );
                let bytes = serde_json::to_vec(&envelope).unwrap();
                let mut response = (StatusCode::SERVICE_UNAVAILABLE, bytes.clone()).into_response();
                if index != 0 {
                    let auth = if index == 1 {
                        FleetAuth::test("invented-fleet", &[8; 32])
                    } else {
                        signing
                    };
                    let digest = if index == 2 {
                        "another-request".into()
                    } else {
                        FleetAuth::body_digest(&body)
                    };
                    response.headers_mut().extend(
                        auth.response_headers_for(EXCHANGE_PATH, "cedar", &bytes, &digest)
                            .unwrap(),
                    );
                }
                response
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = PeerConfig {
        name: "cedar".into(),
        url: format!("http://{}", listener.local_addr().unwrap()),
    };
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let http = replication_http_client();
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let local = Local(Arc::new(plain_memory("birch").unwrap()));
    for expected_count in 1..=3 {
        let error = post_signed(&http, &local, &peer, "birch", &auth, &fleet, &query, false)
            .await
            .unwrap_err();
        assert!(
            !error.is::<PeerOverloaded>(),
            "unverified overload must not enter polling: {error:#}"
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            expected_count,
            "unverified overload must not be retried"
        );
        assert!(
            fleet.activity.read().unwrap().is_empty(),
            "unverified overload cannot mark the peer alive"
        );
    }
    for seconds in [5, 5, 10, 30, 30] {
        let error = post_signed_to::<_, ReplicationExchange>(
            &http,
            &peer,
            "birch",
            &auth,
            &fleet,
            EXCHANGE_PATH,
            &query,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<PeerOverloaded>().unwrap().retry_after,
            Duration::from_secs(seconds)
        );
        assert!(fleet.activity.read().unwrap().contains_key("cedar"));
    }
    server.abort();
}

#[tokio::test]
async fn failed_export_job_is_removed_and_identical_retry_preserves_receipt() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let left = plain_open(&root.path().join("birch.sqlite3"), "birch").unwrap();
    let right = Arc::new(plain_open(&root.path().join("cedar.sqlite3"), "cedar").unwrap());
    for store in [&left, right.as_ref()] {
        store.bind_fleet(auth.fleet_id()).unwrap();
    }
    left.append_claim(&ClaimInput {
        subject: "note/copper".into(),
        kind: "example.note".into(),
        actor: None,
        fields: BTreeMap::from([("text".into(), Value::String("copper".into()))]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    })
    .unwrap();
    let query = left
        .export_replication_exchange(auth.fleet_id(), &ReplicationInventory::default())
        .unwrap();
    let bytes = Bytes::from(serde_json::to_vec(&query).unwrap());
    let backend = SlowExport {
        local: Local(right.clone()),
        slow: Arc::new(AtomicBool::new(false)),
        fail_next: Arc::new(AtomicBool::new(true)),
        receives: Arc::new(AtomicUsize::new(0)),
    };
    let state = PeerState::new(
        backend.clone(),
        "cedar".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["birch".into()])),
    );
    for expected_status in [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::OK] {
        let response = receive_exchange(
            State(state.clone()),
            auth.request_headers("birch", &bytes).unwrap(),
            bytes.clone(),
        )
        .await;
        assert_eq!(response.status(), expected_status);
        let headers = response.headers().clone();
        let answer = to_bytes(response.into_body(), MAX_EXCHANGE_BYTES)
            .await
            .unwrap();
        auth.verify_sender(
            &headers,
            "RESPONSE",
            EXCHANGE_PATH,
            &answer,
            Some("cedar"),
            Some(&FleetAuth::body_digest(&bytes)),
        )
        .unwrap();
        assert!(
            state.fleet.exchange_jobs.peers.lock().unwrap().is_empty(),
            "failed and delivered jobs must not pin the peer slot"
        );
        assert_eq!(
            right
                .claims_for("note/copper", Some("example.note"))
                .unwrap()
                .len(),
            1,
            "admitted envelope survives export failure and remains idempotent"
        );
    }
    assert_eq!(
        backend.receives.load(Ordering::Relaxed),
        2,
        "identical retry must recompute the failed job"
    );
}

#[tokio::test(start_paused = true)]
async fn an_always_failing_http_peer_cannot_refresh_its_own_probe_window() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let probes = Arc::new(AtomicUsize::new(0));
    let count = probes.clone();
    let app = Router::new().route(
        EXCHANGE_PATH,
        axum::routing::head(move || {
            count.fetch_add(1, Ordering::Relaxed);
            async { StatusCode::OK }
        })
        .post(|| async { StatusCode::UNAUTHORIZED }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let peer = PeerConfig {
        name: "cedar".into(),
        url: url.clone(),
    };
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let keep_awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let (_route_tx, mut routes) = watch::channel(vec![Route::Http(url.clone())]);
    let mut inbound = fleet.inbound_changed.subscribe();
    let mut activity = fleet.activity_changed.subscribe();
    let mut connectivity = fleet.connectivity_changed.subscribe();
    let http = replication_http_client();
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let store = Arc::new(plain_memory("birch").unwrap());
    let local = Local(store.clone());
    let query = store.export_replication_summary(auth.fleet_id()).unwrap();
    let previous = (url, tokio::time::Instant::now());
    // A recent successful exchange permits a cheap probe to shorten a retry once.
    let error = post_signed(&http, &local, &peer, "birch", &auth, &fleet, &query, false)
        .await
        .unwrap_err();
    assert!(!error.is::<PeerOverloaded>());
    {
        let started = tokio::time::Instant::now();
        let wait = wait_peer_retry(
            &local,
            crate::store::now_ms(),
            Duration::from_secs(3600),
            &mut routes,
            &mut inbound,
            &mut activity,
            &mut connectivity,
            &fleet,
            "cedar",
            started,
            Some(&previous),
            &http,
        );
        tokio::pin!(wait);
        let mut completed = false;
        // Drive sockets without Tokio advancing time ahead of them.
        for _ in 0..35 {
            tokio::select! {
                result = &mut wait => { assert!(!result); completed = true; break; }
                _ = async { for _ in 0..100 { tokio::task::yield_now().await; } } => {}
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert!(
            completed,
            "a recent HTTP route should recover within 30 seconds"
        );
    }
    assert!(probes.load(Ordering::Relaxed) > 0);
    assert!(fleet.activity.read().unwrap().is_empty());
    // The endpoint still answers, but repeated failing signed exchanges cannot extend
    // eligibility. Once the authenticated window expires, a full quiet wait resumes.
    tokio::time::advance(PEER_PROBE_WINDOW).await;
    let before = probes.load(Ordering::Relaxed);
    let error = post_signed(&http, &local, &peer, "birch", &auth, &fleet, &query, false)
        .await
        .unwrap_err();
    assert!(!error.is::<PeerOverloaded>());
    let started = tokio::time::Instant::now();
    let delay = retry_delay(Duration::from_secs(3600), &fleet, "cedar");
    assert_eq!(delay, Duration::from_secs(3600));
    let wait = wait_peer_retry(
        &local,
        crate::store::now_ms(),
        delay,
        &mut routes,
        &mut inbound,
        &mut activity,
        &mut connectivity,
        &fleet,
        "cedar",
        started,
        Some(&previous),
        &http,
    );
    tokio::pin!(wait);
    for _ in 0..2 {
        tokio::select! {
            _ = &mut wait => panic!("an expired route must retain quiet backoff"),
            _ = async { for _ in 0..100 { tokio::task::yield_now().await; } } => {}
        }
        tokio::time::advance(Duration::from_secs(15)).await;
    }
    tokio::time::advance(Duration::from_secs(3570)).await;
    assert!(!wait.await);
    assert_eq!(
        probes.load(Ordering::Relaxed),
        before,
        "an expired route must not keep probing itself alive"
    );
    keep_awake.abort();
    server.abort();
}

#[tokio::test]
async fn concurrent_slow_exports_refuse_more_work_with_authenticated_overload() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(plain_open(&root.path().join("cedar.sqlite3"), "cedar").unwrap());
    let auth = FleetAuth::test("1f91ca65-7793-48cc-866e-ac15690130e1", &[7; 32]);
    store.bind_fleet(auth.fleet_id()).unwrap();
    let fleet = FleetContext::legacy(
        ["birch", "alder", "ash", "fir", "elm"]
            .into_iter()
            .map(String::from)
            .collect(),
    );
    let query = store.export_replication_summary(auth.fleet_id()).unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let mut requests = Vec::new();
    for name in ["birch", "alder", "ash", "fir"] {
        let mut input = query.clone();
        input.peer = name.into();
        let bytes = Bytes::from(serde_json::to_vec(&input).unwrap());
        let backend = SlowExport {
            local: Local(store.clone()),
            slow: Arc::new(AtomicBool::new(true)),
            fail_next: Arc::new(AtomicBool::new(false)),
            receives: received.clone(),
        };
        let state = PeerState::new(backend, "cedar".into(), auth.clone(), fleet.clone());
        requests.push(tokio::spawn(receive_exchange(
            State(state),
            auth.request_headers(name, &bytes).unwrap(),
            bytes,
        )));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.load(Ordering::Relaxed) < MAX_EXCHANGE_JOBS {
            for request in &mut requests {
                if request.is_finished() {
                    let response = request.await.unwrap();
                    let status = response.status();
                    let body = to_bytes(response.into_body(), MAX_EXCHANGE_BYTES)
                        .await
                        .unwrap();
                    panic!(
                        "unexpected early admission response {status}: {}",
                        String::from_utf8_lossy(&body)
                    );
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut input = query;
    input.peer = "elm".into();
    let query = Bytes::from(serde_json::to_vec(&input).unwrap());
    let state = PeerState::new(Local(store), "cedar".into(), auth.clone(), fleet.clone());
    let started = tokio::time::Instant::now();
    let response = receive_exchange(
        State(state),
        auth.request_headers("elm", &query).unwrap(),
        query.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(started.elapsed() < Duration::from_secs(1));
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), MAX_EXCHANGE_BYTES)
        .await
        .unwrap();
    auth.verify_sender(
        &headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &body,
        Some("cedar"),
        Some(&FleetAuth::body_digest(&query)),
    )
    .unwrap();
    assert_eq!(received.load(Ordering::Relaxed), MAX_EXCHANGE_JOBS);
    assert_eq!(
        fleet.exchange_jobs.peers.lock().unwrap().len(),
        MAX_EXCHANGE_JOBS
    );
    for request in requests {
        request.abort();
    }
}
