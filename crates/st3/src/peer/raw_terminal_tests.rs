// Real PTY integration: set PTY_BINARY when the executable is not on PATH.
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
    let peer = PeerState {
        backend: PeerBackend::Main(Client::unix(&owner_socket)),
        node: "raw-owner".into(),
        auth: FleetAuth::test(fleet, &[7; 32]).with_member_key(Some(owner_key.clone())),
        fleet: member_context(&owner.store, &owner_key, &[], &[], false),
        main_socket: owner_socket.clone(),
        outbound_notify: watch::channel(0).0,
    };
    let peer_task = tokio::spawn(async move {
        axum::serve(listener, peer_router(peer)).await.unwrap();
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
        &PeerBackend::Local(gateway.store.clone()),
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
        Path::new("/no/such/socket"),
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
