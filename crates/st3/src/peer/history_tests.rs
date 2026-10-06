// Real PTY history through the paired gateway and signed remote-owner read.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paired_history_pages_read_the_actual_remote_pty_owner() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(pty) = std::env::split_paths(&path).map(|dir| dir.join("pty")).find(|path| path.is_file()) else {
        assert!(std::env::var_os("CI").is_none(), "CI must provide pty");
        return;
    };
    let roots = [(); 2].map(|()| tempfile::tempdir().unwrap());
    let make_state = |root: &Path, node: &str| crate::api::AppState {
        store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: tokio::sync::watch::channel(0_u64).0,
        node: node.into(), state_dir: root.to_path_buf(), pty_root: root.join("pty"),
        pty_binary: pty.clone(), fleet_id: None, configured_peers: Vec::new(),
        client_relay: None, native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner = make_state(roots[0].path(), "history-owner");
    let mut gateway = make_state(roots[1].path(), "history-gateway");
    let runtime = st_runtime::PtyRuntime::new(owner.pty_root.clone()).with_binary(pty.to_string_lossy());
    struct Cleanup(st_runtime::PtyRuntime);
    impl Drop for Cleanup {
        fn drop(&mut self) { let _ = self.0.stop("history-smoke"); let _ = self.0.remove("history-smoke"); }
    }
    let _cleanup = Cleanup(runtime.clone());
    // The owner budgets generously above its 10,000-row promise; exceed the
    // byte budget, not merely the promised minimum, to force anchor eviction.
    let script = r#"stty -echo; i=0; while [ $i -lt 12 ]; do printf '\033[1;31m\033]8;;https://example.test/history\033\\L%s\033]8;;\033\\\033[0m\r\n' "$i"; i=$((i+1)); done; printf READY; while IFS= read -r command; do case $command in append) printf '\r\nL12\r\nL13\r\nAPPENDED';; evict) i=0; while [ $i -lt 100000 ]; do printf '\r\nE%s' "$i"; i=$((i+1)); done; printf '\r\nEVICTED\r\n\r\n\r\n';; oversize) pattern=''; i=0; while [ $i -lt 40 ]; do pattern="$pattern\033[31mA\033[32mB"; i=$((i+1)); done; i=0; while [ $i -lt 500 ]; do printf '\r\n\033]8;;https://example.test/history/long-link-retained-in-each-individually-styled-run-to-exercise-the-public-page-byte-bound\033\\%b\033]8;;\033\\\033[0m' "$pattern"; i=$((i+1)); done; printf '\r\nOVERSIZED';; alt) printf '\033[?1049hALT';; back) printf '\033[?1049l';; reset) printf '\033cRESET';; esac; done"#;
    let output = std::process::Command::new(&pty).env("PTY_ROOT", &owner.pty_root)
        .args(["run", "-d", "--force", "--id", "history-smoke", "--rows", "3", "--cols", "80", "--tag", "keep=true", "--", "/bin/sh", "-c", script])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let live = runtime.snapshot().unwrap().into_iter().find(|live| live.name == "history-smoke").unwrap();
    let incarnation = format!("{}:{}", live.pid.unwrap(), live.created_at.unwrap());
    fleet_claim(&owner.store, "runtime.observed", "agent/history-smoke", serde_json::json!({
        "runtime_id": "history-smoke", "incarnation_id": incarnation,
        "status": "running", "terminal": true,
    }));
    gateway.store.import_replication("history-owner", &owner.store.export_replication(0).unwrap()).unwrap();
    gateway.store.record_transport_observation("history-owner", "up", None, None).unwrap();
    let socket = roots[0].path().join("st3.sock");
    let owner_app = crate::api::router(owner.clone());
    let owner_socket = socket.clone();
    let mut unix = tokio::spawn(async move { crate::api::serve_unix(&owner_socket, owner_app).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { if tokio::net::UnixStream::connect(&socket).await.is_ok() { break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    let peer = PeerState::new(MainBackend::new(socket.clone()), "history-owner".into(), FleetAuth::test("history-fleet", &[7; 32]), FleetContext::legacy(BTreeSet::from(["history-gateway".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let worker = tokio::spawn(async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await });
    let secret = roots[1].path().join("fleet-secret");
    fs::write(&secret, [7_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "history-gateway".into(), fleet_id: Some("history-fleet".into()), shared_secret_file: Some(secret),
        peers: vec![PeerConfig { name: "history-owner".into(), url: format!("http://{address}") }], ..Default::default()
    }).unwrap();
    let credential = "history-paired-secret";
    let pair = |scopes: Value| fleet_claim(&gateway.store, "custom.client.pairing-completed", "custom/client/history-device", serde_json::json!({
        "credential_hash": hex::encode(Sha256::digest(credential.as_bytes())),
        "session_actor": "client/history-device", "person_id": "person/avery", "scopes": scopes,
        "expires_at_unix_ms": registry_now_ms() + 60_000,
    }));
    pair(serde_json::json!([]));
    let app = crate::api::fabric_router(gateway.clone());
    let read_limit = |before: Option<String>, fence: String, limit: u16| {
        let app = app.clone();
        let mut uri = format!("/v1/client/terminals/agent%2Fhistory-smoke/history?runtime_incarnation={fence}&limit={limit}");
        if let Some(before) = before { uri.push_str(&format!("&before={before}")); }
        async move {
            let response = app.oneshot(Request::builder().uri(uri).header("Authorization", format!("Bearer {credential}")).body(Body::empty()).unwrap()).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), MAX_CLIENT_READ_BYTES).await.unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let read = |before, fence| read_limit(before, fence, 2);
    assert_eq!(read(None, incarnation.clone()).await.0, StatusCode::FORBIDDEN);
    pair(serde_json::json!(["terminal.read"]));
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (status, page) = read(None, incarnation.clone()).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            if page["value"]["lines"][1]["text"] == "L9" { break page; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(first["value"]["lines"][0]["text"], "L8");
    assert_eq!(first["value"]["lines"][0]["runs"], serde_json::json!([{
        "text": "L8", "cells": 2, "fg": 1, "bold": true,
        "link": { "uri": "https://example.test/history" },
    }]));
    let cursor = first["value"]["next_before"].as_str().unwrap().to_owned();
    runtime.send_line("history-smoke", "append").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { let (_, page) = read(None, incarnation.clone()).await; if page["value"]["retained_rows"].as_u64().is_some_and(|rows| rows > 10) { break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    let (status, older) = read(Some(cursor.clone()), incarnation.clone()).await;
    assert_eq!(status, StatusCode::OK, "{older}");
    assert_eq!(older["value"]["lines"][0]["text"], "L6");
    assert_eq!(older["value"]["lines"][1]["text"], "L7");
    runtime.send_line("history-smoke", "alt").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { let (status, page) = read(Some(cursor.clone()), incarnation.clone()).await; if page["code"] == "history-alternate-screen" { assert_eq!(status, StatusCode::CONFLICT, "{page}"); break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    runtime.send_line("history-smoke", "back").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { let (status, page) = read(Some(cursor.clone()), incarnation.clone()).await; if status == StatusCode::OK { assert_eq!(page["value"]["lines"][0]["text"], "L6"); break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    runtime.send_line("history-smoke", "evict").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop { let (status, page) = read(Some(cursor.clone()), incarnation.clone()).await; if page["code"] == "history-cursor-gap" { assert_eq!(status, StatusCode::CONFLICT, "{page}"); break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    assert_eq!(read(None, "stale-incarnation".into()).await.1["code"], "stale-fence");
    // Three final CRLFs move the child's completion marker out of the
    // three-row viewport into retained history. Observing its parsed row
    // through the signed API proves the entire burst was consumed.
    // This does not assert that the producer's bounded live-stream queue
    // survives bursts; that independent streaming issue remains unfixed.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, page) = read_limit(None, incarnation.clone(), 4).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            if page["value"]["lines"].as_array().unwrap().iter().any(|line| line["text"] == "EVICTED") {
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    runtime.send_line("history-smoke", "oversize").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, page) = read_limit(None, incarnation.clone(), 200).await;
            if page["code"] == "history-too-large" {
                assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{page}");
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let fresh = read(None, incarnation.clone()).await.1["value"]["next_before"].as_str().unwrap().to_owned();
    runtime.send_line("history-smoke", "reset").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop { let (status, page) = read(Some(fresh.clone()), incarnation.clone()).await; if page["code"] == "history-cursor-gap" { assert_eq!(status, StatusCode::CONFLICT, "{page}"); break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    // Capture a genuine old page, then prepare an actual same-root replacement
    // outside the consumer's fixed five-second response budget. The gate moves
    // between the two genuine identity snapshots only after the API's pre-fence.
    // This tests stale-page identity cutover, not live-daemon stop latency.
    use pty_core::protocol::{HistoryRequest, HistoryResponse};
    let gated_root = roots[0].path().join("gated-pty");
    fs::create_dir(&gated_root).unwrap();
    fs::copy(owner.pty_root.join("history-smoke.json"), gated_root.join("history-smoke.json")).unwrap();
    let old_metadata = pty_core::registry::read_metadata_in(&owner.pty_root, "history-smoke").unwrap();
    let captured_request = HistoryRequest {
        expected_generation: old_metadata.generation.unwrap(),
        limit: 2,
        before: None,
    };
    eprintln!("Q35_PHYSICAL old_page_begin unix_ms={}", registry_now_ms());
    let response = pty_client::history::read_in(
        &owner.pty_root, "history-smoke", &captured_request, Duration::from_secs(5),
    ).unwrap();
    assert!(matches!(&response, HistoryResponse::Page { .. }), "{response:?}");
    eprintln!("Q35_PHYSICAL old_page_captured_stop_begin unix_ms={}", registry_now_ms());
    runtime.stop("history-smoke").unwrap();
    eprintln!("Q35_PHYSICAL stopped_remove_begin unix_ms={}", registry_now_ms());
    runtime.remove("history-smoke").unwrap();
    eprintln!("Q35_PHYSICAL removed_spawn_begin unix_ms={}", registry_now_ms());
    let output = std::process::Command::new(&pty).env("PTY_ROOT", &owner.pty_root)
        .args(["run", "-d", "--force", "--id", "history-smoke", "--rows", "3", "--cols", "80", "--tag", "keep=true", "--", "/bin/sh", "-c", script])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    eprintln!("Q35_PHYSICAL replacement_spawned unix_ms={}", registry_now_ms());
    let replacement = runtime.snapshot().unwrap().into_iter().find(|live| live.name == "history-smoke").unwrap();
    let replacement_incarnation = format!("{}:{}", replacement.pid.unwrap(), replacement.created_at.unwrap());
    assert_ne!(replacement_incarnation, incarnation);
    let replacement_metadata = fs::read(owner.pty_root.join("history-smoke.json")).unwrap();
    let new_metadata: pty_core::registry::SessionMetadata = serde_json::from_slice(&replacement_metadata).unwrap();
    assert_eq!(format!("{}:{}", new_metadata.daemon_pid.unwrap(), new_metadata.created_at), replacement_incarnation);
    assert_ne!(new_metadata.generation.unwrap(), captured_request.expected_generation);
    let gate = std::os::unix::net::UnixListener::bind(gated_root.join("history-smoke.sock")).unwrap();
    let gate_metadata = gated_root.join("history-smoke.json");
    let gated = std::thread::spawn(move || {
        use std::io::{Read, Write};
        use pty_core::protocol::{encode_packet, MessageType, PacketReader};
        let (mut client, _) = gate.accept().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut packets = PacketReader::new();
        let mut bytes = [0_u8; 2048];
        let request = loop {
            let count = client.read(&mut bytes).unwrap();
            assert!(count > 0);
            if let Some(packet) = packets.feed(&bytes[..count]).unwrap().into_iter().find(|packet| packet.type_ == MessageType::History) {
                break serde_json::from_slice::<HistoryRequest>(&packet.payload).unwrap();
            }
        };
        assert_eq!(request.expected_generation, captured_request.expected_generation);
        assert_eq!(request.limit, captured_request.limit);
        assert_eq!(request.before, captured_request.before);
        eprintln!("Q35_PHYSICAL request_matched_identity_cutover unix_ms={}", registry_now_ms());
        // Publish exact pre-read native bytes: accelerated file-copy I/O can
        // block beyond the fixed RPC deadline even for tiny metadata files.
        eprintln!("Q35_PHYSICAL genuine_metadata_write_begin unix_ms={}", registry_now_ms());
        fs::write(gate_metadata, replacement_metadata).unwrap();
        eprintln!("Q35_PHYSICAL genuine_metadata_write_done unix_ms={}", registry_now_ms());
        client.write_all(&encode_packet(MessageType::History, &serde_json::to_vec(&response).unwrap())).unwrap();
        eprintln!("Q35_PHYSICAL actual_old_page_replied unix_ms={}", registry_now_ms());
    });
    unix.abort();
    let _ = unix.await;
    fs::remove_file(&socket).unwrap();
    let mut gated_owner = owner.clone();
    gated_owner.pty_root = gated_root;
    let gated_app = crate::api::router(gated_owner);
    let gated_socket = socket.clone();
    unix = tokio::spawn(async move { crate::api::serve_unix(&gated_socket, gated_app).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop { if tokio::net::UnixStream::connect(&socket).await.is_ok() { break; } tokio::task::yield_now().await; }
    }).await.unwrap();
    // Native lifecycle preparation can outlast the original 60-second grant.
    // Renew the same authorized pairing before the request, never after an error.
    pair(serde_json::json!(["terminal.read"]));
    eprintln!("Q35_PHYSICAL signed_request_begin unix_ms={}", registry_now_ms());
    let (status, raced) = read(None, incarnation.clone()).await;
    eprintln!("Q35_PHYSICAL signed_request_completed unix_ms={}", registry_now_ms());
    assert_eq!(status, StatusCode::CONFLICT, "{raced}");
    assert_eq!(raced["code"], "stale-fence");
    gated.join().unwrap();
    let (status, replaced) = read(None, incarnation.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{replaced}");
    assert_eq!(replaced["code"], "stale-fence");
    fleet_claim(&owner.store, "runtime.observed", "agent/history-smoke", serde_json::json!({
        "runtime_id": "history-smoke", "incarnation_id": replacement_incarnation,
        "status": "running", "terminal": true,
    }));
    gateway.store.import_replication("history-owner", &owner.store.export_replication(0).unwrap()).unwrap();
    runtime.stop("history-smoke").unwrap();
    fs::copy(owner.pty_root.join("history-smoke.json"), roots[0].path().join("gated-pty/history-smoke.json")).unwrap();
    let (status, unavailable) = read(None, replacement_incarnation).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{unavailable}");
    assert_eq!(unavailable["code"], "terminal-unavailable");
    fleet_claim(&gateway.store, "custom.client.pairing-revoked", "custom/client/history-device", serde_json::json!({}));
    assert_eq!(read(None, incarnation).await.0, StatusCode::FORBIDDEN);
    worker.abort(); unix.abort();
}

fn registry_now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}
