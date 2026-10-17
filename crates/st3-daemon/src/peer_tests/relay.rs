//! Peer relay tests that run the API on a socket; they live in st3-daemon because the API does.

#![allow(unused_imports)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use anyhow::{Context as _, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State, OriginalUri, Query};
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use notify::Watcher as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;
use smallclaims::sync::{ Backend, FleetAuth, Local, MAX_EXCHANGE_BYTES, MAX_MANIFEST_BYTES, WorkerConfig, };
use smallclaims::sync::{dial_targets, signed_error_response_for, signed_response_for};
use crate::client::Client;
use crate::config::{Config, PeerConfig};
use crate::fleet::transport::{Fabric, LocalTransports, Route, local_addresses, parse_route, resolve_tool};
use crate::fleet::{FleetView, MemberKey};
use crate::model::{ ApiResponse, ReplicaEnvelopeId, ReplicationExchange, ReplicationExportRequest, ReplicationExportResponse, ReplicationInventory, ReplicationPeerFailureRequest, ReplicationReceiveRequest, ReplicationReceiveResponse, };
use crate::store::Store;
use crate::relay_trace::{self, Outcome, Phase};
use crate::store::{ CheckpointAction, CheckpointManifest, CheckpointManifestNeed, CheckpointManifestPage, CheckpointManifestRequest, };
use crate::client::CLIENT_READ_FORWARD_PATH;
use crate::model::*;
use std::os::unix::fs::PermissionsExt as _;
use st3_peer::peer::*;
use crate::model::ClaimInput;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use std::net::SocketAddr;
use std::sync::Mutex;
use tower::ServiceExt as _;
use sha2::{Digest as _, Sha256};
use smallclaims::sync::{ FleetContext, PeerState, exchange, fetch_checkpoint_manifest, peer_router, replication_http_client, };
use tokio::net::TcpListener;
use crate::store::{ClaimTombstone, EnvelopeTombstone, checkpoint_cut};
use crate::store::{CheckpointContext, newest_due_cut};

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

struct ScratchRawPty {
    binary: std::ffi::OsString,
    root: PathBuf,
}

impl Drop for ScratchRawPty {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.binary)
            .env("PTY_ROOT", &self.root)
            .args(["kill", "raw-seat"])
            .output();
    }
}

fn raw_app(root: &Path, node: &str) -> crate::api::AppState {
    crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    }
}

async fn raw_packet(stream: &mut tokio::net::UnixStream) -> pty_core::protocol::Packet {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut header = [0_u8; 5];
        stream.read_exact(&mut header).await.unwrap();
        let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        assert!(length <= 16 * 1024 * 1024);
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await.unwrap();
        pty_core::protocol::Packet {
            type_: pty_core::protocol::MessageType::from_u8(header[0]),
            payload,
        }
    })
    .await
    .expect("raw owner packet deadline")
}

async fn raw_geometry(stream: &mut tokio::net::UnixStream, rows: u16, cols: u16) {
    for _ in 0..100 {
        let packet = raw_packet(stream).await;
        if packet.type_ == pty_core::protocol::MessageType::Geometry {
            assert_eq!(
                pty_core::protocol::decode_geometry(&packet.payload),
                (rows, cols)
            );
            return;
        }
    }
    panic!("owner did not send GEOMETRY");
}

#[tokio::test]
async fn membership_only_gateway_holds_raw_writers_and_fences_capabilities() {
    use pty_core::protocol::{MessageType, encode_attach, encode_data, encode_peek, encode_resize};
    use st3_client::RawTerminalMode;
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let owner = raw_app(owner_root.path(), "raw-owner");
    let mut gateway = raw_app(gateway_root.path(), "raw-gateway");
    let fleet = "3b241101-e2bb-4255-8caf-4136c566a962";
    let owner_key =
        Arc::new(MemberKey::load_or_create(&owner_root.path().join("fleet/node.key")).unwrap());
    let gateway_key =
        Arc::new(MemberKey::load_or_create(&gateway_root.path().join("fleet/node.key")).unwrap());
    for (store, key) in [(&owner.store, &owner_key), (&gateway.store, &gateway_key)] {
        store.bind_fleet(fleet).unwrap();
        store.pin_fleet_anchor(owner_key.public()).unwrap();
        store.set_member_key(Some(key.clone())).unwrap();
    }
    for (name, key, via) in [
        ("raw-owner", &owner_key, "anchor"),
        ("raw-gateway", &gateway_key, "invite"),
    ] {
        fleet_claim(
            &owner.store,
            "fleet.member-admitted",
            &format!("host/{name}"),
            serde_json::json!({
                "fleet_id": fleet, "member_key": key.public(), "via": via, "mode": "listening"
            }),
        );
    }
    let binary = std::env::var_os("PTY_BINARY").unwrap_or_else(|| "pty".into());
    let pty = ScratchRawPty {
        binary,
        root: owner.pty_root.clone(),
    };
    let started = std::process::Command::new(&pty.binary)
        .env("PTY_ROOT", &pty.root)
        .args([
            "run",
            "-d",
            "--force",
            "--id",
            "raw-seat",
            "--tag",
            "st3.subject=agent/raw-seat",
            "--",
            "sh",
            "-c",
            "printf 'owner-baseline\\r\\n'; cat",
        ])
        .output()
        .expect("install pty or set PTY_BINARY for raw transport integration");
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let metadata = pty_core::registry::read_metadata_in(&pty.root, "raw-seat").unwrap();
    let incarnation = format!("{}:{}", metadata.daemon_pid.unwrap(), metadata.created_at);
    owner
        .store
        .append_claim(&ClaimInput {
            subject: "agent/raw-seat".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/raw-seat".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), serde_json::json!("raw-seat")),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("status".into(), serde_json::json!("running")),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let owner_socket = owner_root.path().join("st3.sock");
    let owner_app = crate::api::router(owner.clone());
    let served = owner_socket.clone();
    let owner_task = tokio::spawn(async move {
        crate::api::serve_unix(&served, owner_app).await.unwrap();
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    owner
        .store
        .publish_fleet_endpoints(
            "listening",
            &[serde_json::json!({"transport": "loopback", "address": address.to_string()})],
            "test",
        )
        .unwrap();
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "raw-owner".into(), FleetAuth::test(fleet, &[7; 32]).with_member_key(Some(owner_key.clone())), member_context(&owner.store, &owner_key, &[], &[], false));
    let peer_task = tokio::spawn(async move {
        axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while tokio::net::UnixStream::connect(&owner_socket)
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Real signed replication over the owner's peer HTTP listener: the gateway learns the
    // membership, the owner's advertised endpoint, and the seat's runtime incarnation.
    exchange(
        &replication_http_client(),
        &Local(gateway.store.clone()),
        "raw-gateway",
        &PeerConfig {
            name: "raw-owner".into(),
            url: format!("http://{address}"),
        },
        &FleetAuth::test(fleet, &[7; 32]).with_member_key(Some(gateway_key.clone())),
        &member_context(
            &gateway.store,
            &gateway_key,
            &[owner_key.public()],
            &[],
            false,
        ),
    )
    .await
    .unwrap();
    assert!(
        gateway
            .store
            .fleet_view_sealed()
            .unwrap()
            .members
            .iter()
            .any(|member| member.name == "raw-owner" && !member.endpoints.is_empty())
    );
    let secret = gateway_root.path().join("fleet/secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let config = Config {
        node: "raw-gateway".into(),
        state_dir: gateway_root.path().into(),
        fleet_id: Some(fleet.into()),
        shared_secret_file: Some(secret.clone()),
        peers: Vec::new(),
        fleet: Some(crate::config::FleetFile {
            fleet_id: fleet.into(),
            node_key_file: "node.key".into(),
            secret_file: secret,
            legacy_peers: false,
            transports: vec!["tailscale".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    gateway.client_relay = ClientRelay::from_config(&config)
        .unwrap()
        .map(|relay| relay.with_links(gateway.store.clone()));
    assert!(gateway.client_relay.as_ref().unwrap().peers.is_empty());
    let credential = "scratch-raw-paired-credential";
    gateway
        .store
        .append_claim(&ClaimInput {
            subject: "custom/client/pairing-raw-fixture".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: None,
            fields: BTreeMap::from([
                (
                    "credential_hash".into(),
                    serde_json::json!(hex::encode(Sha256::digest(credential.as_bytes()))),
                ),
                (
                    "session_actor".into(),
                    serde_json::json!("client/raw-scratch"),
                ),
                ("person_id".into(), serde_json::json!("person/avery")),
                (
                    "scopes".into(),
                    serde_json::json!(["terminal.read", "terminal.control"]),
                ),
                ("expires_at_unix_ms".into(), serde_json::json!(u64::MAX)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served = gateway_socket.clone();
    let gateway_task = tokio::spawn(async move {
        crate::api::serve_unix(&served, crate::api::fabric_router(gateway))
            .await
            .unwrap();
    });
    for socket in [&owner_socket, &gateway_socket] {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::net::UnixStream::connect(socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let client = st3_client::Client::unix_gateway(&gateway_socket, credential);
    let terminal = "terminal/agent/raw-seat";
    assert!(
        client
            .raw_terminal_attachment(terminal, "wrong-incarnation", RawTerminalMode::Attach)
            .await
            .is_err()
    );
    let attachment = client
        .raw_terminal_attachment(terminal, &incarnation, RawTerminalMode::Attach)
        .await
        .unwrap();
    let mut first = client.raw_terminal_stream(&attachment).await.unwrap();
    assert!(
        client.raw_terminal_stream(&attachment).await.is_err(),
        "capability must be consumed atomically"
    );
    first.write_all(&encode_attach(40, 120)).await.unwrap();
    raw_geometry(&mut first, 40, 120).await;
    let screen = raw_packet(&mut first).await;
    assert_eq!(screen.type_, MessageType::Screen);
    let second_attachment = client
        .raw_terminal_attachment(terminal, &incarnation, RawTerminalMode::Attach)
        .await
        .unwrap();
    let mut second = client
        .raw_terminal_stream(&second_attachment)
        .await
        .unwrap();
    second.write_all(&encode_attach(20, 150)).await.unwrap();
    raw_geometry(&mut first, 20, 120).await;
    raw_geometry(&mut second, 20, 120).await;
    assert_eq!(raw_packet(&mut second).await.type_, MessageType::Screen);
    first.write_all(&encode_resize(50, 100)).await.unwrap();
    raw_geometry(&mut first, 20, 100).await;
    raw_geometry(&mut second, 20, 100).await;
    let peek_attachment = client
        .raw_terminal_attachment(terminal, &incarnation, RawTerminalMode::Peek)
        .await
        .unwrap();
    let mut peek = client.raw_terminal_stream(&peek_attachment).await.unwrap();
    peek.write_all(&encode_peek(false, true)).await.unwrap();
    raw_geometry(&mut peek, 20, 100).await;
    assert_eq!(raw_packet(&mut peek).await.type_, MessageType::Screen);
    drop(second);
    raw_geometry(&mut first, 50, 100).await;
    raw_geometry(&mut peek, 50, 100).await;
    // A readonly capability cannot upgrade into a writer, even with raw framing.
    peek.write_all(&encode_resize(1, 1)).await.unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peek.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    // Recovery is a writable output-side operation through the actual gateway and peer.
    first
        .write_all(&pty_core::protocol::encode_packet(
            MessageType::ResetInputModes,
            &[],
        ))
        .await
        .unwrap();
    let recovered = raw_packet(&mut first).await;
    assert_eq!(recovered.type_, MessageType::Data);
    assert!(
        recovered
            .payload
            .windows(b"\x1b[?1003l".len())
            .any(|bytes| bytes == b"\x1b[?1003l")
    );
    first
        .write_all(&encode_data(b"raw-input-proof\n"))
        .await
        .unwrap();
    let mut output = Vec::new();
    for _ in 0..100 {
        let packet = raw_packet(&mut first).await;
        if packet.type_ == MessageType::Data {
            output.extend(packet.payload);
        }
        if output
            .windows(b"raw-input-proof".len())
            .any(|bytes| bytes == b"raw-input-proof")
        {
            break;
        }
    }
    assert!(
        output
            .windows(b"raw-input-proof".len())
            .any(|bytes| bytes == b"raw-input-proof")
    );
    drop(first);
    peer_task.abort();
    owner_task.abort();
    gateway_task.abort();
    drop(pty);
}

#[tokio::test]
async fn stale_dial_out_links_never_offer_reverse_routes() {
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let roots = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let names = ["cedar", "birch", "fern"];
    let keys = [(); 3].map(|()| Arc::new(MemberKey::generate().unwrap().0));
    let states = std::array::from_fn::<_, 3, _>(|i| {
        let root = roots[i].path();
        let store = Arc::new(Store::open(&root.join("graph.db"), names[i]).unwrap());
        store.bind_fleet(fleet).unwrap();
        store.pin_fleet_anchor(keys[0].public()).unwrap();
        store.set_member_key(Some(keys[i].clone())).unwrap();
        crate::api::AppState {
            owning_person: None,
            store,
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: names[i].into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: Some(fleet.into()),
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        }
    });
    states[0]
        .store
        .admit_fleet_anchor(fleet, keys[0].public(), "listening")
        .unwrap();
    for i in 1..3 {
        fleet_claim(
            &states[0].store,
            "fleet.member-admitted",
            &format!("host/{}", names[i]),
            serde_json::json!({
                "fleet_id": fleet, "member_key": keys[i].public(), "via": "invite",
                "mode": if i == 2 { "dial-out" } else { "listening" },
                "sponsor": "host/birch"
            }),
        );
    }
    for state in &states[1..] {
        let exchange = states[0]
            .store
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        state
            .store
            .receive_replication_exchange("cedar", fleet, &exchange)
            .unwrap();
        state.store.validate_replication_backlog().unwrap();
        state.store.project_replication_backlog().unwrap();
    }
    let old_success = smallclaims::store::now_ms()
        - smallclaims::store::TRANSPORT_LINK_MAX_AGE_MS
        - smallclaims::store::TRANSPORT_LINK_CLOCK_SKEW_MS;
    states[2]
        .store
        .record_transport_observation("birch", "up", None, Some(old_success))
        .unwrap();
    states[2]
        .store
        .append_claim(&ClaimInput {
            subject: "agent/fixture-seat".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/fixture-seat".into()),
            fields: serde_json::from_value(serde_json::json!({
                "runtime_id":"fern-seat", "incarnation_id":"fern:i1",
                "status":"running", "terminal":true
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();

    states[2].store.append_claim(&ClaimInput {
        subject: "message/fixture-attachment".into(), kind: "message.sent".into(),
        actor: Some("person/avery".into()),
        fields: serde_json::from_value(serde_json::json!({
            "from":"person/avery", "to":"agent/fixture-seat", "content":"fixture", "status":"sent",
            "attachments":[{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "media_type":"image/png", "size":1, "origin":"host/fern"}]
        })).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();

    let sockets = std::array::from_fn::<_, 3, _>(|i| roots[i].path().join("st3.sock"));
    let mut servers = Vec::new();
    for i in 0..3 {
        let socket = sockets[i].clone();
        let app = crate::api::router(states[i].clone());
        servers.push(tokio::spawn(async move {
            crate::api::serve_unix(&socket, app).await.unwrap()
        }));
    }
    for socket in &sockets {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists());
    }
    let auth = FleetAuth::test(fleet, &[7; 32]);
    let mut peers = Vec::new();
    // Fern has a daemon Unix socket but no inbound peer listener.
    for i in 0..2 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        peers.push(PeerConfig {
            name: names[i].into(),
            url: format!("http://{}", listener.local_addr().unwrap()),
        });
        let context = FleetContext::legacy(
            names
                .iter()
                .filter(|name| **name != names[i])
                .map(|name| (*name).into())
                .collect(),
        );
        let state = PeerState::new(
            MainBackend::new(sockets[i].clone()),
            names[i].into(),
            auth.clone(),
            context,
        );
        servers.push(tokio::spawn(async move {
            axum::serve(listener, peer_router(state, smalltalk_routes(crate::api::raw_terminal_owner_splice)))
                .await
                .unwrap()
        }));
    }
    let make_relay = |i: usize| {
        let secret = roots[i].path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let mut relay = ClientRelay::from_config(&Config {
            node: names[i].into(),
            fleet_id: Some(fleet.into()),
            shared_secret_file: Some(secret),
            peers: peers
                .iter()
                .filter(|peer| peer.name != names[i])
                .cloned()
                .collect(),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
        .with_links(states[i].store.clone());
        relay.fabric = None;
        relay
    };
    let request = ClientReadRequest {
        authority_actor: "person/avery".into(),
        request: ClientReadOperation::AgentWorkspace {
            identity: "agent/fixture-seat".into(),
        },
        relay: None,
    };
    let replicate_fern = |i: usize| {
        let backend = MainBackend::new(sockets[i].clone());
        let exchange = states[2]
            .store
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        async move {
            backend
                .receive("fern", fleet, &exchange, None, Default::default())
                .await
                .unwrap()
        }
    };
    for i in 0..2 {
        replicate_fern(i).await;
        assert!(
            states[i]
                .store
                .latest_claim("host/birch", Some("transport.observed"))
                .unwrap()
                .is_some(),
            "the historical claim replicated"
        );
        assert!(states[i].store.transport_links().unwrap().is_empty());
        let relay = make_relay(i);
        assert!(
            Arc::ptr_eq(&relay.fleet_view(), &relay.fleet_view()),
            "per-item reachability reuses sealed membership"
        );
        assert!(!relay.reaches("host/fern"));
        assert!(
            relay.next_hops("fern", &[]).is_empty(),
            "expiry must not enable unseen-owner fallback"
        );
        let error = relay
            .read("host/fern", &request)
            .await
            .unwrap_err()
            .downcast::<ClientReadRejected>()
            .unwrap();
        assert_eq!(error.reason(), Some("dial-out-owner"));
        assert_eq!(error.details["attempts"], serde_json::json!([]));
        assert_eq!(error.status, 503);
        assert!(error.message.contains("has no inbound route"));
        let terminal_error = relay
            .raw_terminal(
                "host/fern",
                "person/avery",
                "terminal/fixture-seat",
                "fixture:i1",
                st3_client::RawTerminalMode::Attach,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            terminal_error
                .downcast::<ClientReadRejected>()
                .unwrap()
                .reason(),
            Some("dial-out-owner")
        );
        let mut gateway = states[i].clone();
        gateway.client_relay = Some(relay.clone());
        let response = crate::api::router(gateway.clone())
            .oneshot(
                Request::get("/v1/hosts/fern/agent-workspace?identity=agent%2Ffixture-seat")
                    .header("x-st3-person", "person/avery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let error: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(error["code"], "remote-unavailable");
        assert_eq!(error["details"]["reason"], "dial-out-owner");
        assert_eq!(error["details"]["hops"], 0);
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("has no inbound route")
        );
        for (method, path, body) in [
            (
                "GET",
                "/v1/client/conversations/agent%2Ffixture-seat/changes",
                "",
            ),
            (
                "GET",
                "/v1/client/blobs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?message=message%2Ffixture-attachment",
                "",
            ),
            (
                "GET",
                "/v1/client/terminals/terminal%2Fagent%2Ffixture-seat/screen",
                "",
            ),
            (
                "POST",
                "/v1/client/terminals/terminal%2Fagent%2Ffixture-seat/raw-attachments",
                r#"{"runtime_incarnation":"fern:i1","mode":"attach"}"#,
            ),
        ] {
            let response = crate::api::router(gateway.clone())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("x-st3-person", "person/avery")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let error: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {error}");
            assert_eq!(
                error["details"]["reason"], "dial-out-owner",
                "{path}: {error}"
            );
            assert_eq!(error["details"]["hops"], 0);
        }
        let listening = format!("host/{}", names[1 - i]);
        assert!(relay.reaches(&listening));
        relay.read(&listening, &request).await.unwrap();
        assert!(states[i].store.observes_transport_to(names[1 - i]).unwrap());
        assert!(!states[i].store.observes_transport_to("fern").unwrap());
    }
    // Even a fresh observation left by an older member cannot advertise a reverse route.
    states[2]
        .store
        .record_transport_observation("birch", "up", None, None)
        .unwrap();
    for (i, state) in states.iter().enumerate().take(2) {
        replicate_fern(i).await;
        assert!(state.store.transport_links().unwrap().is_empty());
        assert!(!make_relay(i).reaches("host/fern"));
    }
    // A real listening-peer receive still publishes fresh route evidence and deduplicates it.
    for i in 0..2 {
        let other = 1 - i;
        let backend = MainBackend::new(sockets[i].clone());
        let exchange = states[other]
            .store
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        backend
            .receive(names[other], fleet, &exchange, None, Default::default())
            .await
            .unwrap();
        assert!(
            states[i]
                .store
                .transport_links()
                .unwrap()
                .contains(&(names[i].into(), names[other].into()))
        );
        let before = states[i]
            .store
            .claims_for(
                &format!("host/{}", names[other]),
                Some("transport.observed"),
            )
            .unwrap()
            .len();
        backend
            .receive(names[other], fleet, &exchange, None, Default::default())
            .await
            .unwrap();
        assert_eq!(
            states[i]
                .store
                .claims_for(
                    &format!("host/{}", names[other]),
                    Some("transport.observed")
                )
                .unwrap()
                .len(),
            before
        );
        let relay = make_relay(i);
        assert!(relay.reaches(&format!("host/{}", names[other])));
        relay
            .read(&format!("host/{}", names[other]), &request)
            .await
            .unwrap();
    }
    // The dial-out member can still read both listening owners in its outbound direction.
    let outbound = make_relay(2);
    for name in &names[..2] {
        assert!(outbound.reaches(&format!("host/{name}")));
        outbound
            .read(&format!("host/{name}"), &request)
            .await
            .unwrap();
    }
    // Published membership overrides a stale configured URL; correcting the owner mode
    // becomes visible when the shared five-second membership cache expires.
    let mut corrected = make_relay(0);
    corrected.peers.push(PeerConfig {
        name: "fern".into(),
        url: peers[1].url.clone(),
    });
    assert!(!corrected.reaches("host/fern"));
    states[2]
        .store
        .publish_fleet_endpoints("listening", &[], "test")
        .unwrap();
    replicate_fern(0).await;
    assert!(!corrected.reaches("host/fern"));
    corrected.membership.lock().unwrap().as_mut().unwrap().0 -= CLIENT_READ_LINKS_TTL;
    assert!(corrected.reaches("host/fern"));
    assert_eq!(
        corrected.next_hops("fern", &[]).first().unwrap().name,
        "fern"
    );
    for server in servers {
        server.abort();
    }
}

struct LeasePair {
    // Tear down the detached process before its registry directory is removed.
    _pty: ScratchRawPty,
    _owner_root: tempfile::TempDir,
    _gateway_root: tempfile::TempDir,
    owner: crate::api::AppState,
    gateway: crate::api::AppState,
    client: st3_client::Client,
    incarnation: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    drop_owner_watch: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for LeasePair {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn lease_claim(store: &Store, subject: &str, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: fields
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

async fn socket_ready(socket: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while tokio::net::UnixStream::connect(socket).await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn lease_pair() -> LeasePair {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let mut owner = raw_app(owner_root.path(), "lease-owner");
    let mut gateway = raw_app(gateway_root.path(), "lease-gateway");
    owner.configured_peers = vec!["lease-gateway".into()];
    gateway.configured_peers = vec!["lease-owner".into()];
    let fleet = "74fd8b91-bd35-4d3c-9bbd-ad5de1b7a7b4";
    owner.store.bind_fleet(fleet).unwrap();
    gateway.store.bind_fleet(fleet).unwrap();
    let pty = ScratchRawPty {
        binary: std::env::var_os("PTY_BINARY").unwrap_or_else(|| "pty".into()),
        root: owner.pty_root.clone(),
    };
    let started = std::process::Command::new(&pty.binary)
        .env("PTY_ROOT", &pty.root)
        .args([
            "run",
            "-d",
            "--force",
            "--id",
            "raw-seat",
            "--",
            "sh",
            "-c",
            "while :; do printf 'lease-live-tail\\r\\n'; sleep 0.05; done",
        ])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let metadata = pty_core::registry::read_metadata_in(&pty.root, "raw-seat").unwrap();
    let incarnation = format!("{}:{}", metadata.daemon_pid.unwrap(), metadata.created_at);
    lease_claim(
        &owner.store,
        "agent/raw-seat",
        "runtime.observed",
        serde_json::json!({"runtime_id":"raw-seat","incarnation_id":incarnation,"status":"running","terminal":true}),
    );
    let owner_socket = owner_root.path().join("st3.sock");
    let served = owner_socket.clone();
    let owner_app = crate::api::router(owner.clone());
    let owner_task = tokio::spawn(async move {
        crate::api::serve_unix(&served, owner_app).await.unwrap();
    });
    socket_ready(&owner_socket).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let auth = FleetAuth::test(fleet, &[9; 32]);
    let peer = PeerState::new(
        MainBackend::new(owner_socket),
        "lease-owner".into(),
        auth.clone(),
        smallclaims::sync::FleetContext::legacy(BTreeSet::from(["lease-gateway".into()])),
    );
    let peer_task = tokio::spawn(async move {
        axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice)))
            .await
            .unwrap();
    });
    let route = PeerConfig {
        name: "lease-owner".into(),
        url: format!("http://{address}"),
    };
    exchange(
        &replication_http_client(),
        &Local(gateway.store.clone()),
        "lease-gateway",
        &route,
        &auth,
        &smallclaims::sync::FleetContext::legacy(BTreeSet::from(["lease-owner".into()])),
    )
    .await
    .unwrap();
    let drop_owner_watch = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = WatchLossProxy {
        target: address.to_string(),
        drop_owner_watch: drop_owner_watch.clone(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(RAW_TERMINAL_PATH, get(lease_watch_proxy))
                .with_state(proxy),
        )
        .await
        .unwrap();
    });
    let route = PeerConfig {
        name: "lease-owner".into(),
        url: format!("http://{proxy_address}"),
    };
    let secret = gateway_root.path().join("secret");
    fs::write(&secret, [9_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let config = Config {
        node: "lease-gateway".into(),
        state_dir: gateway_root.path().into(),
        fleet_id: Some(fleet.into()),
        shared_secret_file: Some(secret),
        peers: vec![route],
        ..Default::default()
    };
    gateway.client_relay = ClientRelay::from_config(&config)
        .unwrap()
        .map(|relay| relay.with_links(gateway.store.clone()));
    let credential = "scratch-lease-credential";
    lease_claim(
        &gateway.store,
        "custom/client/lease-pairing",
        "custom.client.pairing-completed",
        serde_json::json!({"credential_hash":hex::encode(Sha256::digest(credential.as_bytes())),"session_actor":"client/lease-original","person_id":"person/avery","scopes":["terminal.read"],"expires_at_unix_ms":u64::MAX}),
    );
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served = gateway_socket.clone();
    let gateway_app = crate::api::fabric_router(gateway.clone());
    let gateway_task = tokio::spawn(async move {
        crate::api::serve_unix(&served, gateway_app).await.unwrap();
    });
    socket_ready(&gateway_socket).await;
    LeasePair {
        _owner_root: owner_root,
        _gateway_root: gateway_root,
        _pty: pty,
        owner,
        gateway,
        client: st3_client::Client::unix_gateway(&gateway_socket, credential),
        incarnation,
        tasks: vec![owner_task, peer_task, gateway_task, proxy_task],
        drop_owner_watch,
    }
}

async fn leased_peek(pair: &LeasePair) -> st3_client::RawTerminalStream {
    let attachment = pair
        .client
        .raw_terminal_attachment(
            "terminal/agent/raw-seat",
            &pair.incarnation,
            st3_client::RawTerminalMode::Peek,
        )
        .await
        .unwrap();
    let mut stream = pair
        .client
        .raw_terminal_stream_controlled(&attachment)
        .await
        .unwrap();
    stream
        .stream
        .write_all(&pty_core::protocol::encode_peek(false, true))
        .await
        .unwrap();
    let mut geometry = false;
    let mut screen = false;
    while !geometry || !screen {
        let packet = raw_packet(&mut stream.stream).await;
        match packet.type_ {
            pty_core::protocol::MessageType::Geometry => geometry = true,
            pty_core::protocol::MessageType::Screen => screen = true,
            _ => {}
        }
    }
    stream.activity.selected_use().await.unwrap();
    stream
}

async fn lease_eof(stream: &mut tokio::net::UnixStream) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = [0; 16 * 1024];
        loop {
            match stream.read(&mut bytes).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await
    .expect("revoked stream remained open beyond fail-closed deadline");
}

#[tokio::test]
async fn accepted_owner_proof_releases_peek_before_next_heartbeat() {
    let pair = lease_pair().await;
    let peek = tokio::time::timeout(Duration::from_millis(500), leased_peek(&pair))
        .await
        .expect("accepted authority proof must wake screen reads before the one-second heartbeat");
    drop(peek);
}

#[tokio::test]
async fn authenticated_and_capability_peek_have_no_commits_and_retain_revocation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let pair = lease_pair().await;
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = commits.clone();
    let _observer = pair.gateway.store.observe_commits(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
    });
    let observed = commits.clone();
    let _owner_observer = pair.owner.store.observe_commits(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
    });
    assert!(pair.client.raw_terminal_peek("terminal/agent/raw-seat", "stale").await.is_err());
    let mut peek = pair.client
        .raw_terminal_peek("terminal/agent/raw-seat", &pair.incarnation).await.unwrap();
    peek.stream.write_all(&pty_core::protocol::encode_peek(false, true)).await.unwrap();
    loop {
        let packet = raw_packet(&mut peek.stream).await;
        if packet.type_ == pty_core::protocol::MessageType::Screen {
            break;
        }
    }
    peek.activity.selected_use().await.unwrap();
    let attachment = pair.client.raw_terminal_attachment(
        "terminal/agent/raw-seat", &pair.incarnation, st3_client::RawTerminalMode::Peek,
    ).await.unwrap();
    let mut browser = pair.client.raw_terminal_stream_controlled(&attachment).await.unwrap();
    assert!(pair.client.raw_terminal_stream_controlled(&attachment).await.is_err(),
        "a browser PEEK capability must remain single-use");
    assert_eq!(commits.load(Ordering::Relaxed), 0, "PEEK wrote to a store");
    lease_claim(
        &pair.gateway.store,
        "custom/client/lease-pairing",
        "custom.client.pairing-revoked",
        serde_json::json!({"device_id":"lease-fixture"}),
    );
    lease_eof(&mut peek.stream).await;
    lease_eof(&mut browser.stream).await;
    assert!(pair.client.raw_terminal_peek("terminal/agent/raw-seat", &pair.incarnation).await.is_err());
}

#[tokio::test]
async fn raw_lease_revocation_crosses_two_daemons_without_graph_replication() {
    let pair = lease_pair().await;
    let mut first = leased_peek(&pair).await;
    let mut replacement = leased_peek(&pair).await;
    let packet = raw_packet(&mut first.stream).await;
    assert_eq!(packet.type_, pty_core::protocol::MessageType::Data);
    lease_claim(
        &pair.gateway.store,
        "custom/client/lease-pairing",
        "custom.client.pairing-revoked",
        serde_json::json!({"device_id":"lease-fixture"}),
    );
    lease_eof(&mut first.stream).await;
    lease_eof(&mut replacement.stream).await;
    assert!(first.activity.selected_use().await.is_err());
    assert!(
        pair.client
            .raw_terminal_attachment(
                "terminal/agent/raw-seat",
                &pair.incarnation,
                st3_client::RawTerminalMode::Peek
            )
            .await
            .is_err()
    );
    assert!(
        pair.owner
            .store
            .claims_for_subject_kind_at(
                "custom/client/lease-pairing",
                "custom.client.pairing-revoked",
                None,
                true,
                1
            )
            .unwrap()
            .claims
            .is_empty(),
        "test must not rely on replication for revocation"
    );
}

#[tokio::test]
async fn raw_lease_owner_revocation_cancels_blocked_output_and_preserves_read_only_gate() {
    let pair = lease_pair().await;
    let mut peek = leased_peek(&pair).await;
    // Leave both transports undrained while output continues. Cancellation has its own branch.
    lease_claim(
        &pair.owner.store,
        "person/avery",
        "principal.key-revoked",
        serde_json::json!({"key":"scratch-principal-epoch","reason":"scratch revocation"}),
    );
    lease_eof(&mut peek.stream).await;
    let mut new = leased_peek(&pair).await;
    new.stream
        .write_all(&pty_core::protocol::encode_resize(50, 100))
        .await
        .unwrap();
    lease_eof(&mut new.stream).await;
    assert!(
        pair.client
            .raw_terminal_attachment(
                "terminal/agent/raw-seat",
                &pair.incarnation,
                st3_client::RawTerminalMode::Attach
            )
            .await
            .is_err(),
        "PEEK grant must not promote to writer"
    );
}

#[derive(Clone)]
struct WatchLossProxy {
    target: String,
    drop_owner_watch: Arc<std::sync::atomic::AtomicBool>,
}

async fn lease_watch_proxy(
    websocket: WebSocketUpgrade,
    State(proxy): State<WatchLossProxy>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path_and_query().unwrap().as_str();
    let mut request = format!("ws://{}{path}", proxy.target)
        .into_client_request()
        .unwrap();
    for (key, value) in &headers {
        if key.as_str().starts_with("x-st3-") {
            request.headers_mut().insert(key.clone(), value.clone());
        }
    }
    let (upstream, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut outer = websocket.on_upgrade(move |socket| async move {
        let (mut sink, mut source) = socket.split();
        let (mut upstream_sink, mut upstream_source) = upstream.split();
        let upload = async {
            while let Some(Ok(message)) = source.next().await {
                let message = match message {
                    axum::extract::ws::Message::Binary(bytes) => {
                        tokio_tungstenite::tungstenite::Message::Binary(bytes)
                    }
                    axum::extract::ws::Message::Text(text) => {
                        tokio_tungstenite::tungstenite::Message::Text(
                            Bytes::from(text).try_into().unwrap(),
                        )
                    }
                    axum::extract::ws::Message::Ping(bytes) => {
                        tokio_tungstenite::tungstenite::Message::Ping(bytes)
                    }
                    axum::extract::ws::Message::Pong(bytes) => {
                        tokio_tungstenite::tungstenite::Message::Pong(bytes)
                    }
                    _ => break,
                };
                if upstream_sink.send(message).await.is_err() {
                    break;
                }
            }
        };
        let download = async {
            while let Some(Ok(message)) = upstream_source.next().await {
                let message = match message {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        if proxy
                            .drop_owner_watch
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            continue;
                        }
                        axum::extract::ws::Message::Text(Bytes::from(text).try_into().unwrap())
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                        axum::extract::ws::Message::Binary(bytes)
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(bytes) => {
                        axum::extract::ws::Message::Ping(bytes)
                    }
                    tokio_tungstenite::tungstenite::Message::Pong(bytes) => {
                        axum::extract::ws::Message::Pong(bytes)
                    }
                    _ => break,
                };
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! { () = upload => {}, () = download => {} }
    });
    for (key, value) in response.headers() {
        if key.as_str().starts_with("x-st3-") {
            outer.headers_mut().insert(key.clone(), value.clone());
        }
    }
    outer
}

#[tokio::test]
async fn raw_lease_watch_partition_fails_closed_while_pty_data_continues() {
    let pair = lease_pair().await;
    let mut peek = leased_peek(&pair).await;
    pair.drop_owner_watch
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        raw_packet(&mut peek.stream).await.type_,
        pty_core::protocol::MessageType::Data
    );
    let started = tokio::time::Instant::now();
    lease_eof(&mut peek.stream).await;
    assert!(started.elapsed() <= Duration::from_secs(5));
}

#[tokio::test]
async fn a_gateway_streams_a_remote_terminal_through_owner_long_polls() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let app_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = app_state(owner_root.path(), "owner-node");
    let mut gateway = app_state(gateway_root.path(), "gateway-node");
    owner
        .store
        .append_claim(&ClaimInput {
            subject: "agent/remote-shell".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/remote-shell".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("remote-runtime".into())),
                (
                    "incarnation_id".into(),
                    Value::String("remote-runtime:i1".into()),
                ),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("remote-shell-running".into()),
        })
        .unwrap();
    gateway
        .store
        .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
        .unwrap();

    // The owner's PTY session: a replay on PEEK, then whatever output the test writes.
    fs::create_dir_all(&owner.pty_root).unwrap();
    let sessions = tokio::net::UnixListener::bind(owner.pty_root.join("remote-runtime.sock"))
        .unwrap();
    let (output, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
    let session_output = output.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = sessions.accept().await {
            let mut output = session_output.subscribe();
            tokio::spawn(async move {
                let packet = |kind: u8, payload: &[u8]| {
                    let mut packet = vec![kind];
                    packet.extend((payload.len() as u32).to_be_bytes());
                    packet.extend(payload);
                    packet
                };
                let mut peek = [0_u8; 6];
                if stream.read_exact(&mut peek).await.is_err()
                    || stream.write_all(&packet(10, &[0, 24, 0, 80])).await.is_err()
                    || stream.write_all(&packet(5, b"owner shell\r\n$ ")).await.is_err()
                {
                    return;
                }
                while let Ok(bytes) = output.recv().await {
                    if stream.write_all(&packet(0, &bytes)).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let owner_socket = owner_root.path().join("st3.sock");
    let main_socket = owner_socket.clone();
    let owner_app = crate::api::router(owner.clone());
    tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });

    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "gateway-node".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "owner-node".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served_socket = gateway_socket.clone();
    let gateway_app = crate::api::router(gateway);
    tokio::spawn(async move { crate::api::serve_unix(&served_socket, gateway_app).await });
    for socket in [&owner_socket, &gateway_socket] {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    let client = st3_client::Client::unix_as(&gateway_socket, "person/avery");
    let capabilities = client.capabilities().await.unwrap();
    let attachment = client
        .terminal_attach(
            "action/remote-terminal-attach",
            "remote-terminal-attach-0000001",
            st3_client::Fence {
                snapshot_id: capabilities.snapshot.id,
                runtime_incarnation: Some("remote-runtime:i1".into()),
                terminal_sequence: Some(capabilities.snapshot.store_index),
                ..st3_client::Fence::default()
            },
            st3_client::TargetParameters {
                target_id: "terminal/agent/remote-shell".into(),
                ..st3_client::TargetParameters::default()
            },
        )
        .await
        .unwrap()
        .value
        .terminal_attachment
        .unwrap();
    assert_eq!(attachment.owner_host_id, "host/owner-node");
    let mut stream = client
        .terminal_stream(
            &attachment.terminal_id,
            Some("remote-runtime:i1"),
            attachment.stream_capability.as_deref().unwrap(),
        )
        .await
        .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.value.lines[0].text, "owner shell");
    assert!(
        tokio::time::timeout(Duration::from_millis(1_500), stream.next())
            .await
            .is_err(),
        "an idle remote terminal must send nothing"
    );
    output.send(b"echo remote".to_vec()).unwrap();
    let changed = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the owner long poll must return the change promptly")
        .unwrap()
        .unwrap();
    assert_eq!(changed.value.lines[1].text, "$ echo remote");
    assert_eq!(changed.value.runtime_incarnation, "remote-runtime:i1");
    assert_ne!(changed.value.revision, first.value.revision);
}

#[tokio::test]
async fn a_cli_attach_from_another_host_paints_screens_and_delivers_input() {
    use pty_core::protocol::{
        MessageType, PacketReader, decode_size, encode_geometry, encode_screen,
        encode_status_response,
    };
    use std::os::unix::io::AsRawFd as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let app_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = app_state(owner_root.path(), "owner-node");
    let mut gateway = app_state(gateway_root.path(), "gateway-node");
    // A live PTY session as its daemon publishes it: socket, daemon pid, and record.
    let incarnation = format!("{}:now", std::process::id());
    fs::create_dir_all(&owner.pty_root).unwrap();
    fs::write(
        owner.pty_root.join("remote-runtime.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    fs::write(
        owner.pty_root.join("remote-runtime.json"),
        r#"{"createdAt":"now"}"#,
    )
    .unwrap();
    owner
        .store
        .append_claim(&ClaimInput {
            subject: "agent/remote-shell".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/remote-shell".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("remote-runtime".into())),
                ("incarnation_id".into(), Value::String(incarnation.clone())),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("remote-shell-running".into()),
        })
        .unwrap();
    gateway
        .store
        .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
        .unwrap();

    // The session is 30x100. A viewer's PEEK gets its replay and then its output; typed input
    // is echoed as output; an ATTACH gets a replay, and the RESIZE after it is recorded.
    let sessions =
        tokio::net::UnixListener::bind(owner.pty_root.join("remote-runtime.sock")).unwrap();
    let (output, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
    let typed = Arc::new(Mutex::new(Vec::<u8>::new()));
    let resized = Arc::new(Mutex::new(Vec::<(u16, u16)>::new()));
    let (session_output, session_typed, session_resized) =
        (output.clone(), typed.clone(), resized.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = sessions.accept().await {
            let mut output = session_output.subscribe();
            let (typed, resized) = (session_typed.clone(), session_resized.clone());
            let echo = session_output.clone();
            tokio::spawn(async move {
                let (mut reader, mut writer) = stream.into_split();
                let mut packets = PacketReader::new();
                let mut buffer = vec![0_u8; 4096];
                loop {
                    tokio::select! {
                        read = reader.read(&mut buffer) => {
                            let Ok(count @ 1..) = read else { return };
                            for packet in packets.feed(&buffer[..count]).unwrap() {
                                let reply = match packet.type_ {
                                    MessageType::Peek => [
                                        encode_geometry(30, 100),
                                        encode_screen(b"owner shell\r\n$ "),
                                    ]
                                    .concat(),
                                    MessageType::Attach => encode_screen(b"owner shell\r\n$ "),
                                    MessageType::Status => encode_status_response("{}"),
                                    MessageType::Resize => {
                                        resized.lock().unwrap().push(decode_size(&packet.payload));
                                        continue;
                                    }
                                    MessageType::Data => {
                                        typed.lock().unwrap().extend(&packet.payload);
                                        let _ = echo.send(packet.payload);
                                        continue;
                                    }
                                    _ => continue,
                                };
                                if writer.write_all(&reply).await.is_err() {
                                    return;
                                }
                            }
                        }
                        bytes = output.recv() => {
                            let Ok(bytes) = bytes else { return };
                            let data = pty_core::protocol::encode_data(&bytes);
                            if writer.write_all(&data).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });

    let owner_socket = owner_root.path().join("st3.sock");
    let main_socket = owner_socket.clone();
    let owner_app = crate::api::router(owner.clone());
    tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "gateway-node".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "owner-node".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served_socket = gateway_socket.clone();
    let gateway_app = crate::api::router(gateway);
    tokio::spawn(async move { crate::api::serve_unix(&served_socket, gateway_app).await });
    for socket in [&owner_socket, &gateway_socket] {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // The attach client's terminal: stdin and stdout are sockets the test holds the far ends
    // of. Neither is a TTY, so the client asks for its default 24x80.
    let (mut keyboard, stdin) = tokio::net::UnixStream::pair().unwrap();
    let (stdout, mut display) = tokio::net::UnixStream::pair().unwrap();
    let (stderr, _errors) = std::os::unix::net::UnixStream::pair().unwrap();
    let (stdin, stdout) = (stdin.into_std().unwrap(), stdout.into_std().unwrap());
    stdin.set_nonblocking(false).unwrap();
    stdout.set_nonblocking(false).unwrap();
    let io = pty_client::ClientIo {
        stdin: stdin.as_raw_fd(),
        stdout: stdout.as_raw_fd(),
        stderr: stderr.as_raw_fd(),
    };
    let client = st3_client::Client::unix_as(&gateway_socket, "person/avery");
    let attach = tokio::spawn(async move {
        let code = crate::remote_terminal::attach_with_io(
            &client,
            "agent/remote-shell",
            "agent/remote-shell",
            io,
        )
        .await;
        drop((stdin, stdout, stderr));
        code
    });
    let mut shown = String::new();
    let mut wait_for = async |text: &str| {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut buffer = vec![0_u8; 65_536];
        while !shown.contains(text) {
            let count = tokio::time::timeout_at(deadline, display.read(&mut buffer))
                .await
                .unwrap_or_else(|_| panic!("`{text}` was never painted; painted {shown:?}"))
                .unwrap();
            assert!(count > 0, "the display closed before `{text}`");
            shown.push_str(&String::from_utf8_lossy(&buffer[..count]));
        }
    };
    wait_for("owner shell").await;

    keyboard.write_all(b"echo remote\r").await.unwrap();
    wait_for("echo remote").await;
    assert_eq!(typed.lock().unwrap().as_slice(), b"echo remote\r");
    assert_eq!(resized.lock().unwrap().as_slice(), &[(24, 80)]);
    let requested = owner
        .store
        .claims_for("agent/remote-shell", Some("terminal.input.requested"))
        .unwrap();
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0].actor.as_deref(), Some("person/avery"));

    // Ctrl+\ detaches once its double-tap window passes.
    keyboard.write_all(&[0x1c]).await.unwrap();
    let code = tokio::time::timeout(Duration::from_secs(10), attach)
        .await
        .expect("the attach ends after a detach")
        .unwrap()
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(typed.lock().unwrap().as_slice(), b"echo remote\r");
}

#[tokio::test]
async fn the_owning_host_names_a_new_agents_default_workspace() {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let app_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = app_state(owner_root.path(), "owner-node");
    let mut gateway = app_state(gateway_root.path(), "gateway-node");
    let owner_socket = owner_root.path().join("st3.sock");
    let main_socket = owner_socket.clone();
    let owner_app = crate::api::router(owner);
    tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "gateway-node".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "owner-node".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_app = crate::api::router(gateway);
    let expected = crate::config::default_agent_workspace("site")
        .unwrap()
        .display()
        .to_string();
    assert!(expected.ends_with("/st/agents/site"));
    let ask = |host: &str, person: Option<&str>, identity: &str| {
        let mut request = Request::get(format!(
            "/v1/hosts/{host}/agent-workspace?identity={identity}"
        ));
        if let Some(person) = person {
            request = request.header("x-st3-person", person);
        }
        let app = gateway_app.clone();
        let request = request.body(Body::empty()).unwrap();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&body).unwrap())
        }
    };

    let (status, body) = ask("owner-node", Some("person/avery"), "site").await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["value"]["workspace"], expected);
    assert_eq!(body["value"]["host_id"], "host/owner-node");
    let (status, body) = ask("gateway-node", None, "site").await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["value"]["workspace"], expected);

    let (status, body) = ask("owner-node", None, "site").await;
    assert!(!status.is_success());
    assert_eq!(body["code"], "missing-person", "{body}");
    let (status, body) = ask("owner-node", Some("person/avery"), "..%2Fescape").await;
    assert!(!status.is_success());
    assert_eq!(body["code"], "validation-failed", "{body}");
    assert!(body["message"].as_str().unwrap().contains("../escape"));
    let (status, body) = ask("elsewhere", Some("person/avery"), "site").await;
    assert!(!status.is_success());
    assert_eq!(body["code"], "remote-unavailable", "{body}");
    assert_eq!(body["details"]["reason"], "no-route", "{body}");
    assert_eq!(body["details"]["owner_host_id"], "host/elsewhere", "{body}");
    assert!(
        !body["message"].as_str().unwrap().contains("temporarily"),
        "{body}"
    );
}

#[tokio::test]
async fn an_attachment_travels_from_the_member_that_took_it_and_never_through_sync() {
    use crate::blobs::BlobDir;
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let app_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = app_state(owner_root.path(), "owner-node");
    let mut gateway = app_state(gateway_root.path(), "gateway-node");
    let gateway_store = gateway.store.clone();
    let owner_store = owner.store.clone();
    let owner_socket = owner_root.path().join("st3.sock");
    let main_socket = owner_socket.clone();
    let owner_app = crate::api::router(owner);
    tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "gateway-node".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "owner-node".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_app = crate::api::router(gateway);
    let gateway_socket = gateway_root.path().join("st3.sock");
    let serving = gateway_socket.clone();
    let served = gateway_app.clone();
    tokio::spawn(async move { crate::api::serve_unix(&serving, served).await });

    for _ in 0..100 {
        if owner_socket.exists() && gateway_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Pasted on the owner, 1.3 MiB: three relayed chunks.
    let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
    image.extend((0..1_300_000_u32).map(|index| (index % 251) as u8));
    let hash = hex::encode(Sha256::digest(&image));
    let uploaded = st3_client::Client::unix_as(&owner_socket, "person/avery")
        .upload_blob(image.clone(), "image/png")
        .await
        .unwrap()
        .value;
    assert_eq!(uploaded.blob, format!("blob/{hash}"));
    let message = crate::client::Client::unix(&owner_socket)
        .send_message(&crate::model::MessageSendRequest {
            idempotency_key: "pasted-image".into(),
            from: "person/avery".into(),
            to: "agent/gateway-node.seat".into(),
            content: "look at this".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: vec![crate::model::AttachmentInput {
                blob: uploaded.blob.clone(),
                media_type: "image/png".into(),
                name: Some("paste.png".into()),
            }],
        })
        .await
        .unwrap()
        .message;
    assert_eq!(message.attachments[0].origin, "host/owner-node");

    // Sync carries the claim to the gateway; the bytes are not part of it.
    for claim in owner_store.claims_for(&message.subject, Some("message.sent")).unwrap() {
        gateway_store
            .append_claim(&crate::model::ClaimInput {
                subject: message.subject.clone(),
                kind: "message.sent".into(),
                actor: Some("person/avery".into()),
                fields: claim.body["fields"]
                    .as_object()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("pasted-image".into()),
            })
            .unwrap();
    }
    assert!(!BlobDir::under(gateway_root.path()).path(&hash).exists());

    let read = |person: &'static str, message: String| {
        let app = gateway_app.clone();
        let hash = hash.clone();
        async move {
            let response = app
                .oneshot(
                    Request::get(format!("/v1/client/blobs/{hash}?message={message}"))
                        .header("x-st3-person", person)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            (status, to_bytes(response.into_body(), usize::MAX).await.unwrap())
        }
    };
    // A seat on the gateway gets the file written where its harness can open it.
    let seat_files = gateway_root.path().join("seat/attachments");
    let delivered = gateway_store.message(&message.subject).unwrap().unwrap();
    let notices = crate::blobs::materialize(
        &gateway_socket,
        "agent/gateway-node.seat",
        &seat_files,
        &delivered,
    )
    .await
    .unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].name.as_deref(), Some("paste.png"));
    assert_eq!(fs::read(notices[0].path.as_deref().unwrap()).unwrap(), image);
    assert!(notices[0].path.as_deref().unwrap().ends_with(&format!("{hash}.png")));
    fs::remove_file(BlobDir::under(gateway_root.path()).path(&hash)).unwrap();
    let (status, bytes) = read("person/avery", message.subject.clone()).await;
    assert!(status.is_success(), "{}", String::from_utf8_lossy(&bytes[..bytes.len().min(300)]));
    assert_eq!(bytes.as_ref(), image.as_slice());
    assert_eq!(
        BlobDir::under(gateway_root.path()).read(&hash).unwrap().unwrap(),
        image,
        "the gateway keeps a copy for its own readers"
    );
    // The gateway's graph holds a reference, never the bytes.
    assert!(gateway_store.get_blob(&hash).unwrap().is_none());

    // When the owner's copy is gone and the gateway's too, the read says so.
    fs::remove_file(BlobDir::under(owner_root.path()).path(&hash)).unwrap();
    fs::remove_file(BlobDir::under(gateway_root.path()).path(&hash)).unwrap();
    let (status, bytes) = read("person/avery", message.subject.clone()).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["code"], "blob-expired");
    // An owner that cannot be reached is a different answer.
    let message_elsewhere = {
        let mut fields = owner_store
            .claims_for(&message.subject, Some("message.sent"))
            .unwrap()[0]
            .body["fields"]
            .clone();
        fields["attachments"][0]["origin"] = "host/offline-node".into();
        gateway_store
            .append_claim(&crate::model::ClaimInput {
                subject: "message/offline".into(),
                kind: "message.sent".into(),
                actor: Some("person/avery".into()),
                fields: fields.as_object().unwrap().clone().into_iter().collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("pasted-image-elsewhere".into()),
            })
            .unwrap()
            .subject
    };
    let (status, bytes) = read("person/avery", message_elsewhere).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["code"], "remote-unavailable");
}

#[tokio::test]
async fn a_long_body_is_read_from_its_owner_per_request_and_kept_nowhere_else() {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let app_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = app_state(owner_root.path(), "owner-node");
    let mut gateway = app_state(gateway_root.path(), "gateway-node");
    let gateway_store = gateway.store.clone();
    let owner_store = owner.store.clone();
    let owner_socket = owner_root.path().join("st3.sock");
    let main_socket = owner_socket.clone();
    let owner_app = crate::api::router(owner);
    tokio::spawn(async move { crate::api::serve_unix(&main_socket, owner_app).await });
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "owner-node".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["gateway-node".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "gateway-node".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "owner-node".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_app = crate::api::router(gateway);
    let gateway_socket = gateway_root.path().join("st3.sock");
    let serving = gateway_socket.clone();
    let served = gateway_app.clone();
    tokio::spawn(async move { crate::api::serve_unix(&serving, served).await });
    for _ in 0..100 {
        if owner_socket.exists() && gateway_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Pasted on the owner: 256 KiB of multi-byte text.
    let mut body = String::new();
    while body.len() < crate::message_body::MAX_BYTES {
        body.push_str(&format!("note {} é — pasted\n", body.len()));
    }
    while body.len() > crate::message_body::MAX_BYTES {
        body.pop();
    }
    let hash = hex::encode(Sha256::digest(body.as_bytes()));
    let message = crate::client::Client::unix(&owner_socket)
        .send_message(&crate::model::MessageSendRequest {
            idempotency_key: "pasted-notes".into(),
            from: "person/avery".into(),
            to: "agent/gateway-node.seat".into(),
            content: body.clone(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .await
        .unwrap()
        .message;
    assert_eq!(message.body_origin.as_deref(), Some("host/owner-node"));
    assert_eq!(message.body_ref, Some(format!("blob/{hash}")));
    assert!(message.content.len() < 1100);
    // The owner holds the text as a file, and its database holds no copy of it.
    assert_eq!(
        crate::message_body::bodies(owner_root.path()).read("person/avery", &message.subject).unwrap().unwrap(),
        body.as_bytes()
    );
    assert!(owner_store.get_blob(&hash).unwrap().is_none());
    // Sync carries the claim to the gateway: a preview and a reference.
    let claim = owner_store.claims_for(&message.subject, Some("message.sent")).unwrap().remove(0);
    let carried = serde_json::to_string(&claim.body).unwrap();
    assert!(carried.len() < 2048, "{} bytes in the claim", carried.len());
    gateway_store
        .append_claim(&crate::model::ClaimInput {
            subject: message.subject.clone(),
            kind: "message.sent".into(),
            actor: Some("person/avery".into()),
            fields: claim.body["fields"].as_object().unwrap().clone().into_iter().collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("pasted-notes".into()),
        })
        .unwrap();
    let on_gateway = gateway_store.message(&message.subject).unwrap().unwrap();
    assert_eq!(on_gateway.content, message.content);
    assert_eq!(on_gateway.body_bytes, Some(body.len() as u64));

    // The gateway asks the owner each time, and keeps nothing.
    let gateway_client = crate::client::Client::unix(&gateway_socket);
    let read = |subject: String| {
        let client = gateway_client.clone();
        async move {
            client
                .get::<Value>(&format!("/v1/messages/body/{subject}"))
                .await
                .unwrap()
        }
    };
    let started = std::time::Instant::now();
    let whole = read(message.subject.clone()).await;
    eprintln!("cost: 256 KiB body read from its owner through the peer route: {:?}", started.elapsed());
    assert_eq!(whole["text"], body.as_str());
    assert_eq!(whole["complete"], true);
    // A 64 KiB body for comparison, over the same route.
    let mut end = 64 * 1024;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let smaller = &body[..end];
    let sent = crate::client::Client::unix(&owner_socket)
        .send_message(&crate::model::MessageSendRequest {
            idempotency_key: "pasted-notes-64".into(),
            from: "person/avery".into(),
            to: "agent/gateway-node.seat".into(),
            content: smaller.to_owned(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .await
        .unwrap()
        .message;
    if sent.body_ref.is_some() {
        let claim = owner_store.claims_for(&sent.subject, Some("message.sent")).unwrap().remove(0);
        gateway_store
            .append_claim(&crate::model::ClaimInput {
                subject: sent.subject.clone(),
                kind: "message.sent".into(),
                actor: Some("person/avery".into()),
                fields: claim.body["fields"].as_object().unwrap().clone().into_iter().collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("pasted-notes-64".into()),
            })
            .unwrap();
        let started = std::time::Instant::now();
        let small = read(sent.subject.clone()).await;
        eprintln!(
            "cost: {} B body read from its owner through the peer route: {:?}",
            smaller.len(),
            started.elapsed()
        );
        assert_eq!(small["text"], smaller);
    }
    assert!(gateway_store.get_blob(&hash).unwrap().is_none());
    assert!(!crate::message_body::bodies(gateway_root.path()).exists("person/avery", &message.subject));
    assert!(!crate::blobs::BlobDir::under(gateway_root.path()).path(&hash).exists());
    // The owner's raw blob routes do not hand a body out as an image or copy it.
    let (status, _) = {
        let response = gateway_app
            .clone()
            .oneshot(
                Request::get(format!("/v1/client/blobs/{hash}?message={}", message.subject))
                    .header("x-st3-person", "person/avery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        (response.status(), response.into_body())
    };
    assert!(!status.is_success());
    assert!(!crate::blobs::BlobDir::under(gateway_root.path()).path(&hash).exists());
    // A message to an agent is readable by the fleet's people; the participant rule is tested
    // where the message is between two people.
    let response = gateway_app
        .clone()
        .oneshot(
            Request::get(format!("/v1/client/message-bodies/{}", message.subject))
                .header("x-st3-person", "person/avery")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(value["value"]["text"], body.as_str());

    // The owner's file gone: the preview and a note saying where the text was.
    crate::message_body::bodies(owner_root.path()).remove("person/avery", &message.subject);
    let gone = read(message.subject.clone()).await;
    assert_eq!(gone["complete"], false);
    assert!(gone["text"].as_str().unwrap().starts_with(&on_gateway.content));
    assert!(gone["text"].as_str().unwrap().contains("host/owner-node"));
    // An owner that cannot be reached answers the same, and the client route says so.
    let mut fields = claim.body["fields"].clone();
    fields["attachments"][0]["origin"] = "host/offline-node".into();
    gateway_store
        .append_claim(&crate::model::ClaimInput {
            subject: "message/offline-notes".into(),
            kind: "message.sent".into(),
            actor: Some("person/avery".into()),
            fields: fields.as_object().unwrap().clone().into_iter().collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("offline-notes".into()),
        })
        .unwrap();
    let offline = read("message/offline-notes".into()).await;
    assert_eq!(offline["complete"], false);
    assert!(offline["text"].as_str().unwrap().contains("host/offline-node"));
    let response = gateway_app
        .oneshot(
            Request::get("/v1/client/message-bodies/message/offline-notes")
                .header("x-st3-person", "person/avery")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_read_reaches_an_owner_through_a_peer_that_can_dial_it() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    // A chain of isolated daemons: the gateway dials only the relay, and only the relay
    // dials the owner. The gateway knows the relay reaches the owner from the relay's
    // replicated transport observation.
    let roots = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let make_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let mut gateway = make_state(roots[0].path(), "chain-gateway");
    let mut relay = make_state(roots[1].path(), "chain-relay");
    let owner = make_state(roots[2].path(), "chain-owner");
    let agent = "agent/chain-worker";
    let incarnation = "chain-runtime:i1";
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("chain-runtime".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("chain-worker-running".into()),
        })
        .unwrap();
    relay
        .store
        .record_transport_observation("chain-owner", "up", None, None)
        .unwrap();
    gateway
        .store
        .import_replication("chain-owner", &owner.store.export_replication(0).unwrap())
        .unwrap();
    gateway
        .store
        .import_replication("chain-relay", &relay.store.export_replication(0).unwrap())
        .unwrap();
    // The transcript line is written after replication, so only the owner can answer it.
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "harness.timeline".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                ("operation".into(), serde_json::json!("append")),
                ("entry_id".into(), serde_json::json!("timeline-entry/chain-answer")),
                ("revision".into(), serde_json::json!(1)),
                ("role".into(), serde_json::json!("assistant")),
                ("entry_type".into(), serde_json::json!("content")),
                ("final".into(), serde_json::json!(true)),
                (
                    "body".into(),
                    serde_json::json!({"media_type":"text/plain","text":"answered two hops away"}),
                ),
                ("driver".into(), serde_json::json!("codex")),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("sequence".into(), serde_json::json!(1)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("chain-answer".into()),
        })
        .unwrap();

    // The owner's PTY session: a replay on PEEK, then whatever output the test writes.
    fs::create_dir_all(&owner.pty_root).unwrap();
    let sessions =
        tokio::net::UnixListener::bind(owner.pty_root.join("chain-runtime.sock")).unwrap();
    let (output, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
    let session_output = output.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = sessions.accept().await {
            let mut output = session_output.subscribe();
            tokio::spawn(async move {
                let packet = |kind: u8, payload: &[u8]| {
                    let mut packet = vec![kind];
                    packet.extend((payload.len() as u32).to_be_bytes());
                    packet.extend(payload);
                    packet
                };
                let mut peek = [0_u8; 6];
                if stream.read_exact(&mut peek).await.is_err()
                    || stream
                        .write_all(&packet(10, &[0, 24, 0, 80]))
                        .await
                        .is_err()
                    || stream
                        .write_all(&packet(5, b"far shell\r\n$ "))
                        .await
                        .is_err()
                {
                    return;
                }
                while let Ok(bytes) = output.recv().await {
                    if stream.write_all(&packet(0, &bytes)).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let secret = roots[0].path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let relay_config = |node: &str, peer: &str, address: SocketAddr| {
        ClientRelay::from_config(&Config {
            node: node.into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret.clone()),
            peers: vec![PeerConfig {
                name: peer.into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap()
        .unwrap()
    };
    // Each node: its daemon on a Unix socket, and a replication worker that accepts the
    // node before it in the chain.
    let serve_worker = |node: &str, accepts: &str, socket: &Path| {
        let peer = PeerState::new(MainBackend::new(PathBuf::from(socket)), node.into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from([accepts.into()])));
        async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
            address
        }
    };
    let sockets = roots.each_ref().map(|root| root.path().join("st3.sock"));
    let owner_worker = serve_worker("chain-owner", "chain-relay", &sockets[2]).await;
    let relay_worker = serve_worker("chain-relay", "chain-gateway", &sockets[1]).await;
    relay.client_relay = Some(relay_config("chain-relay", "chain-owner", owner_worker));
    gateway.client_relay = Some(
        relay_config("chain-gateway", "chain-relay", relay_worker)
            .with_links(gateway.store.clone()),
    );
    for (socket, state) in sockets.iter().zip([gateway, relay, owner.clone()]) {
        let socket = socket.clone();
        tokio::spawn(async move {
            crate::api::serve_unix(&socket, crate::api::router(state)).await
        });
    }
    for socket in &sockets {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    let client = st3_client::Client::unix_as(&sockets[0], "person/avery");
    let session_id = format!(
        "session/{}",
        &hex::encode(sha2::Sha256::digest(
            format!("{agent}:{incarnation}").as_bytes()
        ))[..24]
    );
    let timeline = client
        .timeline(&session_id, None, Some(20))
        .await
        .unwrap()
        .value;
    assert!(
        timeline.items.iter().any(
            |entry| serde_json::to_value(&entry.body).unwrap()["body"]["text"]
                == "answered two hops away"
        ),
        "the owner's transcript must reach the gateway through the relay"
    );

    let capabilities = client.capabilities().await.unwrap();
    let attachment = client
        .terminal_attach(
            "action/chain-terminal-attach",
            "chain-terminal-attach-00000001",
            st3_client::Fence {
                snapshot_id: capabilities.snapshot.id,
                runtime_incarnation: Some(incarnation.into()),
                terminal_sequence: Some(capabilities.snapshot.store_index),
                ..st3_client::Fence::default()
            },
            st3_client::TargetParameters {
                target_id: format!("terminal/{agent}"),
                ..st3_client::TargetParameters::default()
            },
        )
        .await
        .unwrap()
        .value
        .terminal_attachment
        .unwrap();
    assert_eq!(attachment.owner_host_id, "host/chain-owner");
    let mut stream = client
        .terminal_stream(
            &attachment.terminal_id,
            Some(incarnation),
            attachment.stream_capability.as_deref().unwrap(),
        )
        .await
        .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.value.lines[0].text, "far shell");
    output.send(b"echo far".to_vec()).unwrap();
    let changed = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the relayed long poll must return the change promptly")
        .unwrap()
        .unwrap();
    assert_eq!(changed.value.lines[1].text, "$ echo far");

    // The gateway says how it got a screen: through the relay, not straight from the owner.
    // This stand-in session never answers a stats query, so the owner has no facts to give
    // and the screen still arrives.
    let screen = client
        .terminal_screen_with_facts(&attachment.terminal_id)
        .await
        .unwrap()
        .value;
    let provenance = screen
        .relay
        .expect("a relayed screen carries its provenance");
    assert_eq!(provenance.owner_host_id, "host/chain-owner");
    assert_eq!(provenance.via, "host/chain-relay");
    assert!(!provenance.direct);
    assert_eq!(provenance.transport, "http");
    assert_eq!(provenance.capability_ttl_s, 300);
    assert_eq!(provenance.fence_conflicts, 0);
    assert!(screen.facts.is_none());
}

#[tokio::test]
async fn a_gateway_lists_a_remote_agents_messages_from_its_owner_and_says_when_it_cannot() {
    let roots = [(); 2].map(|()| tempfile::tempdir().unwrap());
    let make_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let mut gateway = make_state(roots[0].path(), "lag-gateway");
    let owner = make_state(roots[1].path(), "lag-owner");
    let agent = "agent/lag-worker";
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("lag-runtime".into())),
                (
                    "incarnation_id".into(),
                    Value::String("lag-runtime:i1".into()),
                ),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("lag-worker-running".into()),
        })
        .unwrap();
    gateway
        .store
        .import_replication("lag-owner", &owner.store.export_replication(0).unwrap())
        .unwrap();
    // The message reaches the owner, and has not replicated to the gateway yet.
    owner
        .store
        .append_claim(&ClaimInput {
            subject: "message/lag-first".into(),
            kind: "message.sent".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String(agent.into())),
                ("to".into(), Value::String("person/avery".into())),
                (
                    "content".into(),
                    Value::String("done, ready for review".into()),
                ),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String("done".into())),
                ("in_reply_to".into(), Value::Null),
                ("tags".into(), Value::Array(Vec::new())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("lag-first".into()),
        })
        .unwrap();

    let secret = roots[0].path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let sockets = roots.each_ref().map(|root| root.path().join("st3.sock"));
    let peer = PeerState::new(
        MainBackend::new(sockets[1].clone()),
        "lag-owner".into(),
        FleetAuth::test("fleet-test", &[7; 32]),
        FleetContext::legacy(BTreeSet::from(["lag-gateway".into()])),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await
    });
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "lag-gateway".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "lag-owner".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    for (socket, state) in sockets.iter().zip([gateway, owner]) {
        let socket = socket.clone();
        tokio::spawn(async move {
            crate::api::serve_unix(&socket, crate::api::router(state)).await
        });
    }
    for socket in &sockets {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    let client = st3_client::Client::unix_as(&sockets[0], "person/avery");
    let page = client
        .messages_list_for_peer(agent, None, Some(20), false)
        .await
        .unwrap()
        .value;
    assert_eq!(page.items.len(), 1, "the owner's list reaches the gateway");
    let replicated = page
        .replicated
        .expect("a remote agent's list says where it came from");
    assert_eq!(replicated.owner_host_id, "host/lag-owner");
    assert_eq!(replicated.source, "owner");
    assert!(replicated.complete);
    assert_eq!(replicated.state, "current");

    // A trusted intermediary can carry this invented owner's read through the internal
    // forwarding route. The owner still authenticates the gateway and supplies the page.
    let forwarded: Value = crate::client::Client::unix(&sockets[0])
        .post(
            CLIENT_READ_FORWARD_PATH,
            &ClientReadRequest {
                authority_actor: "person/avery".into(),
                request: ClientReadOperation::Messages {
                    actor: agent.into(),
                    history: false,
                    limit: Some(20),
                    cursor: None,
                },
                relay: Some(ClientReadRoute {
                    target: "host/lag-owner".into(),
                    path: vec!["lag-source".into()],
                    hops_left: 3,
                }),
            },
        )
        .await
        .unwrap();
    let items = forwarded["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "forwarded page has exact owner membership");
    assert_eq!(items[0]["id"], "message/lag-first");
    assert_eq!(items[0]["content"], "done, ready for review");
    assert_eq!(forwarded["page"]["has_more"], false);

    // With the owner out of reach the gateway shows what it has and says the list may be
    // missing items; it never passes an empty replica off as the whole list.
    worker.abort();
    let _ = worker.await;
    let page = client
        .messages_list_for_peer(agent, None, Some(20), false)
        .await
        .unwrap()
        .value;
    assert!(page.items.is_empty());
    let replicated = page
        .replicated
        .expect("a replica says it is not the owner's list");
    assert_eq!(replicated.source, "replica");
    assert!(!replicated.complete);
    assert_eq!(replicated.state, "unverified");
    assert!(replicated.reason.is_some(), "{replicated:?}");

    // An agent on this host, or any list that names no remote agent, carries no notice.
    let page = client
        .messages_list_for_recipient("person/avery", None, Some(20), false)
        .await
        .unwrap()
        .value;
    assert!(page.replicated.is_none());
}

#[tokio::test]
async fn a_relaying_worker_refuses_a_forged_path_and_a_spent_hop_limit() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let main = crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open_memory("middle").unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "middle".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let secret = root.path().join("fleet-secret");
    fs::write(&secret, [5_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut main = main;
    main.client_relay = ClientRelay::from_config(&Config {
        node: "middle".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "far".into(),
            url: "http://127.0.0.1:9".into(),
        }],
        ..Default::default()
    })
    .unwrap();
    let served = socket.clone();
    let server =
        tokio::spawn(
            async move { crate::api::serve_unix(&served, crate::api::router(main)).await },
        );
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let auth = FleetAuth::test("fleet-test", &[5; 32]);
    let peer = PeerState::new(MainBackend::new(socket.to_path_buf()), "middle".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["near".into()])));
    let send = |route: ClientReadRoute| {
        let body = serde_json::to_vec(&ClientReadRequest {
            authority_actor: "person/test".into(),
            request: ClientReadOperation::TerminalScreen {
                terminal_id: "terminal/agent/far".into(),
                facts: false,
            },
            relay: Some(route),
        })
        .unwrap();
        let mut request = Request::builder()
            .method("POST")
            .uri(CLIENT_READ_PATH)
            .body(Body::from(body.clone()))
            .unwrap();
        *request.headers_mut() = auth
            .request_headers_for(CLIENT_READ_PATH, "near", &body)
            .unwrap();
        let router = peer_router(peer.clone(), smalltalk_routes(crate::api::raw_terminal_owner_splice));
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), MAX_CLIENT_READ_BYTES)
                .await
                .unwrap();
            let value: ApiResponse<Value> = serde_json::from_slice(&bytes).unwrap();
            (status, value.value)
        }
    };
    // A path that does not end at the member that sent it is someone else's claim.
    let (status, _) = send(ClientReadRoute {
        target: "host/far".into(),
        path: vec!["elsewhere".into()],
        hops_left: 2,
    })
    .await;
    assert!(!status.is_success());
    // A read that has used its hops, or has passed this node before, goes no further.
    for route in [
        ClientReadRoute {
            target: "host/far".into(),
            path: vec!["near".into()],
            hops_left: 0,
        },
        ClientReadRoute {
            target: "host/far".into(),
            path: vec!["middle".into(), "near".into()],
            hops_left: 2,
        },
    ] {
        let (status, value) = send(route).await;
        assert_eq!(status, StatusCode::LOOP_DETECTED);
        assert_eq!(value["code"], "remote-unavailable");
    }
    // A read it can carry on reaches for the next hop; here nothing listens there.
    let (status, value) = send(ClientReadRoute {
        target: "host/far".into(),
        path: vec!["near".into()],
        hops_left: 2,
    })
    .await;
    assert!(!status.is_success());
    assert_eq!(value["code"], "remote-unavailable");
    server.abort();
}

#[tokio::test]
async fn a_followed_conversation_says_why_while_its_owner_cannot_be_reached() {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let make_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = make_state(owner_root.path(), "conversation-owner");
    let mut gateway = make_state(gateway_root.path(), "conversation-gateway");
    let agent = "agent/conversation-peer";
    let incarnation = "conversation-runtime:i1";
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                (
                    "runtime_id".into(),
                    serde_json::json!("conversation-runtime"),
                ),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("status".into(), serde_json::json!("running")),
                ("terminal".into(), serde_json::json!(false)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("conversation-peer-runtime".into()),
        })
        .unwrap();
    gateway
        .store
        .import_replication(
            "conversation-owner",
            &owner.store.export_replication(0).unwrap(),
        )
        .unwrap();
    // The owner is a configured peer, but nothing answers at its address.
    let address = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "conversation-gateway".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "conversation-owner".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served_gateway = gateway_socket.clone();
    tokio::spawn(async move {
        crate::api::serve_unix(&served_gateway, crate::api::router(gateway)).await
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&gateway_socket)
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = st3_client::Client::unix_as(&gateway_socket, "person/example");
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe_conversation("conversation", agent)
        .await
        .unwrap();
    // The subscription stays (st retries it) and says why, so a client showing its last
    // copy can say that copy is stale.
    let event = tokio::time::timeout(Duration::from_secs(20), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let st3_client::CollectionEvent::Resync { id, code, message } = event else {
        panic!("expected a resync, got {event:?}");
    };
    assert_eq!(id, "conversation");
    assert_eq!(code, Some(st3_client::ErrorCode::RemoteUnavailable));
    assert!(
        message.is_some_and(|message| message.contains("conversation-owner")),
        "the message names the owner"
    );
}

#[tokio::test]
async fn gateway_fetches_authenticated_owner_record_chunks_and_propagates_invalidation() {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let make_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let mut owner = make_state(owner_root.path(), "conversation-owner");
    let mut gateway = make_state(gateway_root.path(), "conversation-gateway");
    let agent = "agent/conversation-peer";
    let incarnation = "conversation-runtime:i1";
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                (
                    "runtime_id".into(),
                    serde_json::json!("conversation-runtime"),
                ),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("status".into(), serde_json::json!("running")),
                ("terminal".into(), serde_json::json!(false)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("conversation-peer-runtime".into()),
        })
        .unwrap();
    owner.native_session_home = Some(owner_root.path().to_path_buf());
    let native_path = owner_root.path().join("native.jsonl");
    let raw = serde_json::json!({"type":"future","token":"invented-forwarded-token","large":"x".repeat(10000)});
    fs::write(&native_path, format!("{}\n{}\n", serde_json::json!({"type":"session","id":"native-forwarded","cwd":"/work/invented","timestamp":"2026-10-06T12:00:00Z"}), serde_json::json!({"type":"message","message":{"role":"assistant","content":[raw.clone()]}}))).unwrap();
    for (kind, fields) in [
        (
            "harness.observed",
            BTreeMap::from([
                ("state".into(), serde_json::json!("idle")),
                ("driver".into(), serde_json::json!("omp")),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
            ]),
        ),
        (
            "harness.session-file",
            BTreeMap::from([
                ("harness".into(), serde_json::json!("omp")),
                ("agent".into(), serde_json::json!(agent)),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("path".into(), serde_json::json!(native_path)),
                ("session_id".into(), serde_json::json!("native-forwarded")),
            ]),
        ),
    ] {
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: kind.into(),
                actor: Some(agent.into()),
                fields,
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    gateway
        .store
        .import_replication(
            "conversation-owner",
            &owner.store.export_replication(0).unwrap(),
        )
        .unwrap();
    let session_id = format!(
        "session/{}",
        &hex::encode(sha2::Sha256::digest(
            format!("{agent}:{incarnation}").as_bytes()
        ))[..24]
    );
    let owner_socket = owner_root.path().join("st3.sock");
    let served_owner = owner_socket.clone();
    let owner_app = crate::api::router(owner.clone());
    tokio::spawn(async move { crate::api::serve_unix(&served_owner, owner_app).await });
    let peer = PeerState::new(
        MainBackend::new(owner_socket.to_path_buf()),
        "conversation-owner".into(),
        FleetAuth::test("fleet-test", &[7; 32]),
        FleetContext::legacy(BTreeSet::from(["conversation-gateway".into()])),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "conversation-gateway".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "conversation-owner".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served_gateway = gateway_socket.clone();
    tokio::spawn(async move {
        crate::api::serve_unix(&served_gateway, crate::api::router(gateway)).await
    });
    for socket in [&owner_socket, &gateway_socket] {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let client = st3_client::Client::unix_as(&gateway_socket, "person/example");
    let page = client
        .timeline(&session_id, None, None)
        .await
        .unwrap()
        .value;
    let token = page
        .items
        .iter()
        .find_map(|item| match &item.body {
            st3_client::TimelineBody::Content(body) => body
                .blocks
                .first()
                .and_then(|block| block.continuation.as_ref())
                .map(|continuation| continuation.reference.clone()),
            _ => None,
        })
        .expect("negotiated owner continuation");
    let chunk = client
        .conversation_content_chunk(&session_id, &token, 0)
        .await
        .unwrap()
        .value;
    use base64::Engine as _;
    let decoded: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(&chunk.data)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded, serde_json::json!({"raw":raw}));
    use std::io::Write as _;
    writeln!(fs::OpenOptions::new().append(true).open(&native_path).unwrap(), "{}", serde_json::json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"later"}]}})).unwrap();
    assert!(
        client
            .conversation_content_chunk(&session_id, &token, 0)
            .await
            .is_ok()
    );
    fs::write(&native_path, format!("{}\n{}\n", serde_json::json!({"type":"session","id":"native-forwarded","cwd":"/work/invented","timestamp":"2026-10-06T12:00:00Z"}), serde_json::json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"edited"}]}}))).unwrap();
    let error = client
        .conversation_content_chunk(&session_id, &token, 0)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            st3_client::ClientError::Api(
                st3_client::ErrorCode::ConversationContentInvalidated,
                _,
                _
            )
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_gateway_receives_remote_conversation_changes_without_idle_data() {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let make_state = |root: &Path, node: &str| crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = make_state(owner_root.path(), "conversation-owner");
    let mut gateway = make_state(gateway_root.path(), "conversation-gateway");
    let agent = "agent/conversation-peer";
    let incarnation = "conversation-runtime:i1";
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                (
                    "runtime_id".into(),
                    serde_json::json!("conversation-runtime"),
                ),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("status".into(), serde_json::json!("running")),
                ("terminal".into(), serde_json::json!(false)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("conversation-peer-runtime".into()),
        })
        .unwrap();
    gateway
        .store
        .import_replication(
            "conversation-owner",
            &owner.store.export_replication(0).unwrap(),
        )
        .unwrap();
    let session_id = format!(
        "session/{}",
        &hex::encode(sha2::Sha256::digest(
            format!("{agent}:{incarnation}").as_bytes()
        ))[..24]
    );
    let owner_socket = owner_root.path().join("st3.sock");
    let served_owner = owner_socket.clone();
    let owner_app = crate::api::router(owner.clone());
    tokio::spawn(async move { crate::api::serve_unix(&served_owner, owner_app).await });
    let peer = PeerState::new(MainBackend::new(owner_socket.to_path_buf()), "conversation-owner".into(), FleetAuth::test("fleet-test", &[7; 32]), FleetContext::legacy(BTreeSet::from(["conversation-gateway".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let secret = gateway_root.path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "conversation-gateway".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "conversation-owner".into(),
            url: format!("http://{address}"),
        }],
        ..Default::default()
    })
    .unwrap();
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served_gateway = gateway_socket.clone();
    tokio::spawn(async move {
        crate::api::serve_unix(&served_gateway, crate::api::router(gateway)).await
    });
    for socket in [&owner_socket, &gateway_socket] {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let client = st3_client::Client::unix_as(&gateway_socket, "person/example");
    let baseline = client
        .conversation_changes(&session_id, None, 0)
        .await
        .unwrap()
        .value;
    assert!(baseline.items.is_empty());
    let cursor = baseline.next_cursor;
    let mut stream = client.conversation_stream(&session_id, None).await.unwrap();
    let opened = stream.next().await.unwrap().unwrap();
    assert!(opened.value.items.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .is_err()
    );
    let idle = client
        .conversation_changes(&session_id, Some(&cursor), 100)
        .await
        .unwrap()
        .value;
    assert!(idle.items.is_empty());
    let waiting_client = client.clone();
    let waiting_id = session_id.clone();
    let waiting_cursor = cursor.clone();
    let waiting = tokio::spawn(async move {
        waiting_client
            .conversation_changes(&waiting_id, Some(&waiting_cursor), 1000)
            .await
            .unwrap()
            .value
    });
    tokio::task::yield_now().await;
    owner
        .store
        .append_claim(&ClaimInput {
            subject: "message/conversation-peer-first".into(),
            kind: "message.sent".into(),
            actor: Some("person/example".into()),
            fields: BTreeMap::from([
                ("from".into(), serde_json::json!("person/example")),
                ("to".into(), serde_json::json!(agent)),
                ("session_id".into(), serde_json::json!(session_id)),
                ("content".into(), serde_json::json!("hello")),
                ("status".into(), serde_json::json!("sent")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("conversation-peer-message".into()),
        })
        .unwrap();
    owner.event_notify.send_modify(|value| *value += 1);
    let first = tokio::time::timeout(Duration::from_millis(900), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.items.len(), 2);
    let resume = first.next_cursor;
    let streamed = tokio::time::timeout(Duration::from_millis(900), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(streamed.value.items.len(), 2);
    let stream_cursor = streamed.value.next_cursor;
    stream.close().await;
    owner
        .store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "harness.timeline".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                ("operation".into(), serde_json::json!("append")),
                (
                    "entry_id".into(),
                    serde_json::json!("timeline-entry/conversation-peer-reply"),
                ),
                ("revision".into(), serde_json::json!(1)),
                ("role".into(), serde_json::json!("assistant")),
                ("entry_type".into(), serde_json::json!("content")),
                ("final".into(), serde_json::json!(true)),
                (
                    "body".into(),
                    serde_json::json!({"media_type":"text/plain","text":"reply"}),
                ),
                ("driver".into(), serde_json::json!("codex")),
                ("incarnation_id".into(), serde_json::json!(incarnation)),
                ("sequence".into(), serde_json::json!(1)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("conversation-peer-reply".into()),
        })
        .unwrap();
    let replay = client
        .conversation_changes(&session_id, Some(&resume), 0)
        .await
        .unwrap()
        .value;
    assert_eq!(replay.items.len(), 1);
    let mut reconnected = client
        .conversation_stream(&session_id, Some(&stream_cursor))
        .await
        .unwrap();
    let resumed = reconnected.next().await.unwrap().unwrap();
    assert_eq!(resumed.value.items.len(), 1);
    reconnected.close().await;
}

#[tokio::test]
async fn signed_client_read_returns_an_owner_local_native_timeline() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("native-home");
    let transcript = home.join(".codex/sessions/2026/09/24/relay.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, format!(
        "{}\n{}\n",
        serde_json::json!({"type":"session_meta","timestamp":"2026-09-24T15:00:00Z","payload":{"id":"relay-native-id","cwd":root.path(),"source":"test"}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-09-24T15:00:01Z","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"Relay owner answer"}]}}),
    )).unwrap();
    let session = crate::external_sessions::discover_fresh(Some(&home), true)
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.native_id == "relay-native-id")
        .unwrap();
    let socket = root.path().join("st3.sock");
    let main = crate::api::AppState {
        owning_person: None,
        store: Arc::new(Store::open_memory("owner").unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "owner".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec!["source".into()],
        client_relay: None,
        native_session_home: Some(home),
        planner_default: crate::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, crate::api::router(main))
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists());
    let auth = FleetAuth::test("fleet-test", &[4; 32]);
    let peer = PeerState::new(
        MainBackend::new(socket.to_path_buf()),
        "owner".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["source".into()])),
    );
    let body = serde_json::to_vec(&ClientReadRequest {
        authority_actor: "person/test".into(),
        relay: None,
        request: ClientReadOperation::Timeline {
            session_id: session.id,
            limit: 20,
            cursor: None,
        },
    })
    .unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri(CLIENT_READ_PATH)
        .body(Body::from(body.clone()))
        .unwrap();
    *request.headers_mut() = auth
        .request_headers_for(CLIENT_READ_PATH, "source", &body)
        .unwrap();
    use tracing::instrument::WithSubscriber as _;
    let diagnostic = relay_trace::test_capture::Capture::default();
    let response = relay_trace::test_peer_selector(
        FleetAuth::body_digest(&body),
        "source".into(),
        peer_router(peer.clone(), smalltalk_routes(crate::api::raw_terminal_owner_splice)).oneshot(request),
    )
    .with_subscriber(diagnostic.dispatch())
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), MAX_CLIENT_READ_BYTES)
        .await
        .unwrap();
    auth.verify(
        &headers,
        "RESPONSE",
        CLIENT_READ_PATH,
        &bytes,
        Some("owner"),
        Some(&FleetAuth::body_digest(&body)),
    )
    .unwrap();
    let value: ApiResponse<Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value.value["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["body"]["text"] == "Relay owner answer")
    );

    let events = diagnostic.events();
    assert!(events.iter().any(|event| event["phase"] == "Authenticate" && event["state"] == "completed"));
    assert!(
        events
            .iter()
            .any(|event| event["phase"] == "OwnerCall" && event["state"] == "completed")
    );
    assert!(
        events
            .iter()
            .any(|event| event["phase"] == "ResponseBinding"
                && event["binding"].starts_with("request/"))
    );
    assert_eq!(events.last().unwrap()["phase"], "Terminal");
    assert_eq!(events.last().unwrap()["state"], "Completed");
    assert!(!format!("{events:?}").contains("Relay owner answer"));
    assert!(
        events
            .iter()
            .all(|event| event["correlation_id"] == FleetAuth::body_digest(&body))
    );

    let mut rejected = Request::builder()
        .method("POST")
        .uri(CLIENT_READ_PATH)
        .body(Body::from(body.clone()))
        .unwrap();
    *rejected.headers_mut() = auth
        .request_headers_for(CLIENT_READ_PATH, "unconfigured", &body)
        .unwrap();
    let refusal = relay_trace::test_capture::Capture::default();
    let response = relay_trace::test_peer_selector(
        FleetAuth::body_digest(&body),
        "unconfigured".into(),
        peer_router(peer, smalltalk_routes(crate::api::raw_terminal_owner_splice)).oneshot(rejected),
    )
    .with_subscriber(refusal.dispatch())
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        refusal.events().is_empty(),
        "fleet refusal cannot consume or trace a peer selector"
    );
    server.abort();
}

#[tokio::test]
async fn the_worker_uses_the_main_daemon_as_the_only_database_writer() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "target").unwrap());
    store.bind_fleet(fleet).unwrap();
    let app = crate::api::router(crate::api::AppState {
        owning_person: None,
        store: store.clone(),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "target".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: std::path::PathBuf::from("pty"),
        fleet_id: Some(fleet.into()),
        configured_peers: vec!["source".into()],
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    });
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists(), "the main daemon socket did not start");

    let source = Store::open_memory("source").unwrap();
    source.bind_fleet(fleet).unwrap();
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
    let backend = MainBackend::new(socket);
    let received = backend
        .receive(
            "source",
            fleet,
            &exchange,
            Some(Duration::from_millis(10)),
            smallclaims::replication::ReplicationCarrier::Fabric,
        )
        .await
        .unwrap();
    assert!(received.changed);
    assert!(received.receipt.received > 0);
    let exported = backend
        .export(fleet, &ReplicationInventory::default(), false, &[])
        .await
        .unwrap();
    assert!(!exported.exchange.inventory.envelopes.is_empty());
    let summary = backend
        .export(fleet, &ReplicationInventory::default(), true, &[])
        .await
        .unwrap();
    assert!(summary.exchange.inventory.envelopes.is_empty());
    assert!(!summary.exchange.inventory.digest.is_empty());
    backend
        .record_failure(
            "source",
            "down",
            "test outage",
            Some(smallclaims::replication::ReplicationCarrier::Tailscale),
        )
        .await
        .unwrap();
    let (success, failure) = store.replication_peer_transport("source");
    let outbound = success.clone().unwrap();
    let success = success.unwrap();
    assert_eq!(
        success.carrier,
        smallclaims::replication::ReplicationCarrier::Fabric
    );
    assert_eq!(success.fabric_route.as_deref(), Some("unknown"));
    let failed = failure.unwrap();
    assert_eq!(
        failed.carrier,
        smallclaims::replication::ReplicationCarrier::Tailscale
    );
    let still_up = store
        .latest_claim("host/source", Some("transport.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(still_up.body["fields"]["status"], "up");
    store.age_replication_peer_for_test("source");
    let before = store
        .claims_for("host/source", Some("transport.observed"))
        .unwrap()
        .len();
    for _ in 0..1_440 {
        backend
            .record_failure("source", "down", "test outage", Default::default())
            .await
            .unwrap();
    }
    let down = store
        .latest_claim("host/source", Some("transport.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(down.body["fields"]["status"], "unknown");
    assert_eq!(
        store
            .claims_for("host/source", Some("transport.observed"))
            .unwrap()
            .len(),
        before + 1
    );
    backend
        .receive("source", fleet, &exchange, None, Default::default())
        .await
        .unwrap();
    let (success, failure) = store.replication_peer_transport("source");
    assert_eq!(
        success,
        Some(outbound),
        "incoming receipt preserves original Fabric evidence and time"
    );
    assert_eq!(
        failure,
        Some(failed),
        "successful inbound exchange and liveness checks preserve failure evidence"
    );
    let recovered = store
        .latest_claim("host/source", Some("transport.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(recovered.body["fields"]["status"], "up");
    server.abort();
}

#[tokio::test]
async fn inbound_only_peer_syncs_both_ways_between_isolated_daemons() {
    let fleet = "1f91ca65-7793-48cc-866e-ac15690130e1";
    let auth = FleetAuth::test(fleet, &[8; 32]);
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let mut servers = Vec::new();
    let mut stores = Vec::new();
    let mut sockets = Vec::new();
    for (name, root) in [("source", source_dir.path()), ("target", target_dir.path())] {
        let store = Arc::new(Store::open(&root.join("graph.db"), name).unwrap());
        store.bind_fleet(fleet).unwrap();
        let socket = root.join("main.sock");
        let app = crate::api::router(crate::api::AppState {
            owning_person: None,
            store: store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: name.into(),
            state_dir: root.into(),
            pty_root: root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: Some(fleet.into()),
            configured_peers: vec![if name == "source" { "target" } else { "source" }.into()],
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        });
        let served = socket.clone();
        servers.push(tokio::spawn(async move {
            crate::api::serve_unix(&served, app).await
        }));
        stores.push(store);
        sockets.push(socket);
    }
    for socket in &sockets {
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists());
    }
    let source = &stores[0];
    let target = &stores[1];
    source
        .record_transport_observation("target", "up", None, None)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let inbound = PeerConfig {
        name: "source".into(),
        url: String::new(),
    };
    assert!(
        dial_targets(
            &FleetView::default(),
            "target",
            &[inbound],
            LocalTransports::default()
        )
        .is_empty()
    );
    let state = PeerState::new(MainBackend::new(sockets[1].to_path_buf()), "target".into(), auth.clone(), FleetContext::legacy(BTreeSet::from(["source".into()])));
    let peer_server =
        tokio::spawn(async move { axum::serve(listener, peer_router(state, smalltalk_routes(crate::api::raw_terminal_owner_splice))).await });
    let peer = PeerConfig {
        name: "target".into(),
        url: format!("http://{address}"),
    };
    let backend = MainBackend::new(sockets[0].to_path_buf());
    let context = FleetContext::legacy(BTreeSet::from(["target".into()]));
    let http = replication_http_client();
    exchange(
        &http,
        &backend,
        "source",
        &peer,
        &auth,
        &context,
    )
    .await
    .unwrap();
    assert!(
        target
            .latest_claim("host/target", Some("transport.observed"))
            .unwrap()
            .is_some()
    );
    target
        .record_transport_observation("source", "up", None, None)
        .unwrap();
    exchange(
        &http,
        &backend,
        "source",
        &peer,
        &auth,
        &context,
    )
    .await
    .unwrap();
    assert!(
        source
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        source
            .replication_status(true, Some(fleet), &[])
            .unwrap()
            .authority_digest,
        target
            .replication_status(true, Some(fleet), &[])
            .unwrap()
            .authority_digest
    );
    peer_server.abort();
    for server in servers {
        server.abort();
    }
}
