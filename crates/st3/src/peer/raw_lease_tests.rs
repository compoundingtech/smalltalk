// Real scratch daemon endpoints, signed peer route, and an actual detached PTY process.
struct LeasePair {
    _owner_root: tempfile::TempDir,
    _gateway_root: tempfile::TempDir,
    _pty: ScratchRawPty,
    owner: crate::api::AppState,
    gateway: crate::api::AppState,
    client: st3_client::Client,
    incarnation: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    drop_owner_watch: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for LeasePair {
    fn drop(&mut self) { for task in &self.tasks { task.abort(); } }
}
fn lease_claim(store: &Store, subject: &str, kind: &str, fields: Value) {
    store.append_claim(&ClaimInput { subject: subject.into(), kind: kind.into(), actor: None,
        fields: fields.as_object().unwrap().iter().map(|(key,value)| (key.clone(),value.clone())).collect(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None }).unwrap();
}
async fn socket_ready(socket: &Path) {
    tokio::time::timeout(Duration::from_secs(5),async {
        while tokio::net::UnixStream::connect(socket).await.is_err() { tokio::task::yield_now().await; }
    }).await.unwrap();
}
async fn lease_pair() -> LeasePair {
    let owner_root = tempfile::tempdir().unwrap();
    let gateway_root = tempfile::tempdir().unwrap();
    let mut owner = raw_app(owner_root.path(),"lease-owner");
    let mut gateway = raw_app(gateway_root.path(),"lease-gateway");
    owner.configured_peers = vec!["lease-gateway".into()];
    gateway.configured_peers = vec!["lease-owner".into()];
    let fleet = "74fd8b91-bd35-4d3c-9bbd-ad5de1b7a7b4";
    owner.store.bind_fleet(fleet).unwrap();
    gateway.store.bind_fleet(fleet).unwrap();
    let pty = ScratchRawPty { binary: std::env::var_os("PTY_BINARY").unwrap_or_else(|| "pty".into()), root: owner.pty_root.clone() };
    let started = std::process::Command::new(&pty.binary).env("PTY_ROOT",&pty.root)
        .args(["run","-d","--force","--id","raw-seat","--","sh","-c","while :; do printf 'lease-live-tail\\r\\n'; sleep 0.05; done"]).output().unwrap();
    assert!(started.status.success(),"{}",String::from_utf8_lossy(&started.stderr));
    let metadata = pty_core::registry::read_metadata_in(&pty.root,"raw-seat").unwrap();
    let incarnation = format!("{}:{}",metadata.daemon_pid.unwrap(),metadata.created_at);
    lease_claim(&owner.store,"agent/raw-seat","runtime.observed",serde_json::json!({"runtime_id":"raw-seat","incarnation_id":incarnation,"status":"running","terminal":true}));
    let owner_socket = owner_root.path().join("st3.sock");
    let served = owner_socket.clone();
    let owner_app = crate::api::router(owner.clone());
    let owner_task = tokio::spawn(async move { crate::api::serve_unix(&served,owner_app).await.unwrap(); });
    socket_ready(&owner_socket).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let auth = FleetAuth::test(fleet,&[9;32]);
    let peer = PeerState::new(MainBackend::new(owner_socket),"lease-owner".into(),auth.clone(),smallclaims::sync::FleetContext::legacy(BTreeSet::from(["lease-gateway".into()])));
    let peer_task = tokio::spawn(async move { axum::serve(listener,peer_router(peer,smalltalk_routes())).await.unwrap(); });
    let route = PeerConfig { name:"lease-owner".into(),url:format!("http://{address}") };
    exchange(&replication_http_client(),&Local(gateway.store.clone()),"lease-gateway",&route,&auth,&smallclaims::sync::FleetContext::legacy(BTreeSet::from(["lease-owner".into()]))).await.unwrap();
    let drop_owner_watch = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = WatchLossProxy { target:address.to_string(), drop_owner_watch:drop_owner_watch.clone() };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move {
        axum::serve(listener,Router::new().route(RAW_TERMINAL_PATH,get(lease_watch_proxy)).with_state(proxy)).await.unwrap();
    });
    let route = PeerConfig { name:"lease-owner".into(),url:format!("http://{proxy_address}") };
    let secret = gateway_root.path().join("secret");
    fs::write(&secret,[9_u8;32]).unwrap();
    fs::set_permissions(&secret,fs::Permissions::from_mode(0o600)).unwrap();
    let config = Config { node:"lease-gateway".into(),state_dir:gateway_root.path().into(),fleet_id:Some(fleet.into()),shared_secret_file:Some(secret),peers:vec![route],..Default::default() };
    gateway.client_relay = ClientRelay::from_config(&config).unwrap().map(|relay| relay.with_links(gateway.store.clone()));
    let credential = "scratch-lease-credential";
    lease_claim(&gateway.store,"custom/client/lease-pairing","custom.client.pairing-completed",serde_json::json!({"credential_hash":hex::encode(Sha256::digest(credential.as_bytes())),"session_actor":"client/lease-original","person_id":"person/avery","scopes":["terminal.read"],"expires_at_unix_ms":u64::MAX}));
    let gateway_socket = gateway_root.path().join("st3.sock");
    let served = gateway_socket.clone();
    let gateway_app = crate::api::fabric_router(gateway.clone());
    let gateway_task = tokio::spawn(async move { crate::api::serve_unix(&served,gateway_app).await.unwrap(); });
    socket_ready(&gateway_socket).await;
    LeasePair { _owner_root:owner_root,_gateway_root:gateway_root,_pty:pty,owner,gateway,client:st3_client::Client::unix_gateway(&gateway_socket,credential),incarnation,tasks:vec![owner_task,peer_task,gateway_task,proxy_task],drop_owner_watch }
}
async fn leased_peek(pair: &LeasePair) -> st3_client::RawTerminalStream {
    let attachment = pair.client.raw_terminal_attachment("terminal/agent/raw-seat",&pair.incarnation,st3_client::RawTerminalMode::Peek).await.unwrap();
    let mut stream = pair.client.raw_terminal_stream_controlled(&attachment).await.unwrap();
    stream.stream.write_all(&pty_core::protocol::encode_peek(false, true)).await.unwrap();
    let mut geometry = false;
    let mut screen = false;
    while !geometry || !screen {
        let packet = raw_packet(&mut stream.stream).await;
        match packet.type_ { pty_core::protocol::MessageType::Geometry => geometry=true, pty_core::protocol::MessageType::Screen => screen=true, _ => {} }
    }
    stream.activity.selected_use().await.unwrap();
    stream
}
async fn lease_eof(stream: &mut tokio::net::UnixStream) {
    tokio::time::timeout(Duration::from_secs(5),async {
        let mut bytes = [0;16*1024];
        loop { match stream.read(&mut bytes).await { Ok(0) | Err(_) => break, Ok(_) => {} } }
    }).await.expect("revoked stream remained open beyond fail-closed deadline");
}
#[tokio::test]
async fn raw_lease_revocation_crosses_two_daemons_without_graph_replication() {
    let pair = lease_pair().await;
    let mut first = leased_peek(&pair).await;
    let mut replacement = leased_peek(&pair).await;
    let packet = raw_packet(&mut first.stream).await;
    assert_eq!(packet.type_,pty_core::protocol::MessageType::Data);
    lease_claim(&pair.gateway.store,"custom/client/lease-pairing","custom.client.pairing-revoked",serde_json::json!({"device_id":"lease-fixture"}));
    lease_eof(&mut first.stream).await;
    lease_eof(&mut replacement.stream).await;
    assert!(first.activity.selected_use().await.is_err());
    assert!(pair.client.raw_terminal_attachment("terminal/agent/raw-seat",&pair.incarnation,st3_client::RawTerminalMode::Peek).await.is_err());
    assert!(pair.owner.store.claims_for_subject_kind_at("custom/client/lease-pairing","custom.client.pairing-revoked",None,true,1).unwrap().claims.is_empty(),"test must not rely on replication for revocation");
}
#[tokio::test]
async fn raw_lease_owner_revocation_cancels_blocked_output_and_preserves_read_only_gate() {
    let pair = lease_pair().await;
    let mut peek = leased_peek(&pair).await;
    // Leave both transports undrained while output continues. Cancellation has its own branch.
    lease_claim(&pair.owner.store,"person/avery","principal.key-revoked",serde_json::json!({"key":"scratch-principal-epoch","reason":"scratch revocation"}));
    lease_eof(&mut peek.stream).await;
    let mut new = leased_peek(&pair).await;
    new.stream.write_all(&pty_core::protocol::encode_resize(50,100)).await.unwrap();
    lease_eof(&mut new.stream).await;
    assert!(pair.client.raw_terminal_attachment("terminal/agent/raw-seat",&pair.incarnation,st3_client::RawTerminalMode::Attach).await.is_err(),"PEEK grant must not promote to writer");
}

#[derive(Clone)]
struct WatchLossProxy {
    target: String,
    drop_owner_watch: Arc<std::sync::atomic::AtomicBool>,
}

async fn lease_watch_proxy(websocket: WebSocketUpgrade, State(proxy): State<WatchLossProxy>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let path = uri.path_and_query().unwrap().as_str();
    let mut request = format!("ws://{}{path}",proxy.target).into_client_request().unwrap();
    for (key,value) in &headers {
        if key.as_str().starts_with("x-st3-") { request.headers_mut().insert(key.clone(),value.clone()); }
    }
    let (upstream,response) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut outer = websocket.on_upgrade(move |socket| async move {
        let (mut sink,mut source) = socket.split();
        let (mut upstream_sink,mut upstream_source) = upstream.split();
        let upload = async {
            while let Some(Ok(message)) = source.next().await {
                let message = match message {
                    axum::extract::ws::Message::Binary(bytes) => tokio_tungstenite::tungstenite::Message::Binary(bytes),
                    axum::extract::ws::Message::Text(text) => tokio_tungstenite::tungstenite::Message::Text(Bytes::from(text).try_into().unwrap()),
                    axum::extract::ws::Message::Ping(bytes) => tokio_tungstenite::tungstenite::Message::Ping(bytes),
                    axum::extract::ws::Message::Pong(bytes) => tokio_tungstenite::tungstenite::Message::Pong(bytes),
                    _ => break,
                };
                if upstream_sink.send(message).await.is_err() { break; }
            }
        };
        let download = async {
            while let Some(Ok(message)) = upstream_source.next().await {
                let message = match message {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        if proxy.drop_owner_watch.load(std::sync::atomic::Ordering::Acquire) { continue; }
                        axum::extract::ws::Message::Text(Bytes::from(text).try_into().unwrap())
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(bytes) => axum::extract::ws::Message::Binary(bytes),
                    tokio_tungstenite::tungstenite::Message::Ping(bytes) => axum::extract::ws::Message::Ping(bytes),
                    tokio_tungstenite::tungstenite::Message::Pong(bytes) => axum::extract::ws::Message::Pong(bytes),
                    _ => break,
                };
                if sink.send(message).await.is_err() { break; }
            }
        };
        tokio::select! { () = upload => {}, () = download => {} }
    });
    for (key,value) in response.headers() {
        if key.as_str().starts_with("x-st3-") { outer.headers_mut().insert(key.clone(),value.clone()); }
    }
    outer
}

#[tokio::test]
async fn raw_lease_watch_partition_fails_closed_while_pty_data_continues() {
    let pair = lease_pair().await;
    let mut peek = leased_peek(&pair).await;
    pair.drop_owner_watch.store(true,std::sync::atomic::Ordering::Release);
    assert_eq!(raw_packet(&mut peek.stream).await.type_,pty_core::protocol::MessageType::Data);
    let started = tokio::time::Instant::now();
    lease_eof(&mut peek.stream).await;
    assert!(started.elapsed() <= Duration::from_secs(5));
}
