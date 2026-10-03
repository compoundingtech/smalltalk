//! Signed, per-lease authority proof. PTY traffic is never authorization-watch activity.
use super::*;
use crate::api::{RawTerminalLease, RawTerminalLeaseBinding};
use tokio::sync::mpsc;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const WATCH_PATH: &str = "/v1/peer/raw-terminal/authorization-watch";
const WATCH_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProofBody {
    binding: RawTerminalLeaseBinding,
    watcher_epoch: String,
    sequence: u64,
    issued_at_unix_ms: u128,
    challenge: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    body: ProofBody,
    headers: BTreeMap<String, String>,
}
fn now_ms() -> Result<u128> {
    Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis())
}
fn signed(auth: &FleetAuth, node: &str, binding: &RawTerminalLeaseBinding, epoch: &str, sequence: u64, challenge: &str, issued_at_unix_ms: u128) -> Result<String> {
    let body = ProofBody { binding: binding.clone(), watcher_epoch: epoch.into(), sequence, issued_at_unix_ms, challenge: challenge.into() };
    let bytes = serde_json::to_vec(&body)?;
    let headers = auth.request_headers_method("WATCH", WATCH_PATH, node, &bytes)?.iter().map(|(key,value)| Ok((key.to_string(),value.to_str()?.to_owned()))).collect::<Result<BTreeMap<_,_>>>()?;
    Ok(serde_json::to_string(&Proof {body,headers})?)
}
fn verify(auth: &FleetAuth, text: &str, node: &str, binding: &RawTerminalLeaseBinding, member_key: Option<&str>) -> Result<(ProofBody, crate::fleet::Sender)> {
    anyhow::ensure!(text.len() <= 16_384, "authorization proof exceeds bound");
    let proof: Proof = serde_json::from_str(text)?;
    anyhow::ensure!(proof.body.binding == *binding, "authorization binding changed");
    anyhow::ensure!(!proof.body.watcher_epoch.is_empty(), "authorization watcher has no epoch");
    let mut headers = HeaderMap::new();
    for (key,value) in proof.headers { headers.insert(axum::http::HeaderName::from_bytes(key.as_bytes())?, HeaderValue::from_str(&value)?); }
    let bytes = serde_json::to_vec(&proof.body)?;
    let sender = auth.verify_sender(&headers, "WATCH", WATCH_PATH, &bytes, Some(node), None)?;
    anyhow::ensure!(sender.member_key.as_deref() == member_key && (member_key.is_none() || sender.member_signature_valid), "authorization watcher member incarnation changed");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    anyhow::ensure!(now.abs_diff(proof.body.issued_at_unix_ms) <= 1_000, "authorization proof is not fresh");
    Ok((proof.body, sender))
}

pub(super) fn gateway_bridge<S>(socket: WebSocketStream<S>, lease: Option<Arc<RawTerminalLease>>, relay: ClientRelay, owner: String, member_key: Option<String>) -> Result<(tokio::net::UnixStream,mpsc::Sender<Value>)>
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
    let (client,bridge) = tokio::net::UnixStream::pair()?;
    let bridge = bridge.into_std()?;
    let monitor = tokio::io::unix::AsyncFd::new(bridge.try_clone()?)?;
    let bridge = tokio::net::UnixStream::from_std(bridge)?;
    let (control,mut controls) = mpsc::channel::<Value>(16);
    tokio::spawn(async move {
        let auth = &relay.auth;
        let node = &relay.node;
        let (mut sink,mut source) = socket.split();
        let (mut reader,mut writer) = bridge.into_split();
        let flush = tokio::sync::Notify::new();
        let challenge = watch::channel::<Option<String>>(None).0;
        let upload = async {
            let mut bytes = [0;16*1024];
            let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut sequence = 0_u64;
            let mut challenge_updates = challenge.subscribe();
            let mut proof_sent = lease.is_none();
            loop {
                tokio::select! {
                    biased;
                    _ = heartbeat.tick(), if lease.is_some() && challenge.borrow().is_some() => {
                        let lease = lease.as_ref().expect("watch requires lease");
                        if lease.revalidate().is_err() { break; }
                        sequence += 1;
                        let nonce = challenge.borrow().clone().expect("watch requires challenge");
                        let Ok(issued) = now_ms() else { break; };
                        let Ok(proof) = signed(&auth,&node,&lease.binding,&lease.binding.gateway_epoch,sequence,&nonce,issued) else { break; };
                        if sink.send(Message::Text(proof.into())).await.is_err() { break; }
                        proof_sent = true;
                    }
                    Some(control) = controls.recv(), if proof_sent => {
                        if sink.send(Message::Text(control.to_string().into())).await.is_err() { break; }
                    }
                    read = reader.read(&mut bytes), if proof_sent => {
                        let Ok(count) = read else { break; };
                        if count == 0 || lease.as_ref().is_some_and(|lease| lease.check().is_err()) { break; }
                        if sink.send(Message::Binary(bytes[..count].to_vec().into())).await.is_err() { break; }
                    }
                    result = challenge_updates.changed(), if lease.is_some() => { if result.is_err() { break; } }
                    () = flush.notified() => { if sink.flush().await.is_err() { break; } }
                }
            }
        };
        let download = async {
            let mut watcher_epoch = None;
            let mut sequence = 0_u64;
            while let Some(Ok(message)) = source.next().await {
                match message {
                    Message::Text(text) => {
                        let Some(lease) = &lease else { break; };
                        let Ok((proof, sender)) = verify(auth,&text,&owner,&lease.binding,member_key.as_deref()) else { break; };
                        if relay.accept_raw_watcher(&sender).is_err() { break; }
                        if watcher_epoch.is_none() {
                            if proof.sequence != 0 || proof.challenge.is_empty() { break; }
                            watcher_epoch = Some(proof.watcher_epoch);
                            challenge.send_replace(Some(proof.challenge));
                            continue;
                        }
                        if sequence >= proof.sequence || watcher_epoch.as_ref() != Some(&proof.watcher_epoch) || challenge.borrow().as_ref() != Some(&proof.challenge) { break; }
                        sequence = proof.sequence;
                        if lease.proof(&lease.binding.gateway_epoch, sequence).is_err() { break; }
                    }
                    Message::Binary(bytes) => {
                        if lease.as_ref().is_some_and(|lease| lease.check().is_err()) || writer.write_all(&bytes).await.is_err() { break; }
                    }
                    Message::Ping(_) => flush.notify_one(),
                    Message::Pong(_) => {},
                    _ => break,
                }
            }
        };
        let closed = async {
            loop {
                let Ok(mut ready) = monitor.readable().await else { break; };
                if ready.ready().is_read_closed() || ready.ready().is_error() { break; }
                ready.clear_ready();
            }
        };
        let expired = async { if let Some(lease) = &lease { lease.expired().await; } else { std::future::pending::<()>().await; } };
        tokio::select! { biased; () = expired => {}, () = closed => {}, () = upload => {}, () = download => {} }
    });
    Ok((client,control))
}

pub(super) async fn receive_owner(websocket: WebSocketUpgrade, state: PeerState, path: String, binding: RawTerminalLeaseBinding, member_key: Option<String>) -> Response {
    let client = st3_client::Client::unix_as(state.backend().socket(), &binding.person);
    let result = async {
        let attachment = client.raw_terminal_attachment(&binding.terminal,&binding.incarnation,st3_client::RawTerminalMode::Peek).await?;
        anyhow::ensure!(attachment.owner_host_id == binding.owner, "raw route is not owner-local");
        let raw_path = format!("/v1/client/terminals/{}/raw-stream?incarnation={}&mode=peek",urlencoding::encode(binding.terminal.trim_start_matches("terminal/")),urlencoding::encode(&binding.incarnation));
        let mut request = format!("ws://localhost{raw_path}").into_client_request()?;
        request.headers_mut().insert("x-st3-person",HeaderValue::from_str(&binding.person)?);
        request.headers_mut().insert(crate::api::RAW_ORIGIN_HEADER,HeaderValue::from_str(&serde_json::to_string(&binding)?)?);
        request.headers_mut().insert("sec-websocket-protocol",HeaderValue::from_str(&format!("st3.client.pty.v0, st3.cap.{}",attachment.stream_capability))?);
        let unix = tokio::net::UnixStream::connect(state.backend().socket()).await?;
        let unix = unix.into_std()?;
        let monitor = tokio::io::unix::AsyncFd::new(unix.try_clone()?)?;
        let unix = tokio::net::UnixStream::from_std(unix)?;
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(64*1024)).max_frame_size(Some(64*1024));
        let (socket,_) = tokio_tungstenite::client_async_with_config(request,unix,Some(config)).await?;
        Ok::<_,anyhow::Error>((socket,monitor))
    };
    let (backend,monitor) = match tokio::time::timeout(WATCH_DEADLINE,result).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(_)) => return (StatusCode::CONFLICT,"owner rejected raw lease binding or incarnation").into_response(),
        Err(_) => return (StatusCode::GATEWAY_TIMEOUT,"owner raw lease admission deadline exceeded").into_response(),
    };
    let signed = match state.auth().response_headers_for(&path,state.node(),&[],&FleetAuth::body_digest(&[])) { Ok(headers) => headers, Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    let mut response = websocket.max_message_size(64*1024).max_frame_size(64*1024).on_upgrade(move |socket| owner_bridge(socket,backend,monitor,state,binding,member_key));
    response.headers_mut().extend(signed);
    response
}

async fn owner_bridge<S>(socket: axum::extract::ws::WebSocket, backend: WebSocketStream<S>, monitor: tokio::io::unix::AsyncFd<std::os::unix::net::UnixStream>, state: PeerState, binding: RawTerminalLeaseBinding, member_key: Option<String>)
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
    let auth = state.auth();
    let node = state.node();
    let (mut sink,mut source) = socket.split();
    let (mut local_sink,mut local_source) = backend.split();
    let deadline = watch::channel(tokio::time::Instant::now()+WATCH_DEADLINE).0;
    let owner_epoch = uuid::Uuid::now_v7().to_string();
    let proof_ready = watch::channel(false).0;
    let upload = async {
        let mut sequence = 0_u64;
        while let Some(Ok(message)) = source.next().await {
            if tokio::time::Instant::now() >= *deadline.borrow() { break; }
            match message {
                axum::extract::ws::Message::Binary(bytes) => {
                    if !*proof_ready.borrow() { break; }
                    if local_sink.send(Message::Binary(bytes)).await.is_err() { break; }
                }
                axum::extract::ws::Message::Text(text) => {
                    let value: Value = match serde_json::from_str(&text) { Ok(value) => value, Err(_) => break };
                    if value.get("type").and_then(Value::as_str) == Some("selected-use") {
                        if !*proof_ready.borrow() { break; }
                        if local_sink.send(Message::Text(Bytes::from(text).try_into().expect("Axum validated UTF-8"))).await.is_err() { break; }
                    } else {
                        let Ok((proof, sender)) = verify(auth,&text,binding.gateway.trim_start_matches("host/"),&binding,member_key.as_deref()) else { break; };
                        if state.accept(&sender).is_err() { break; }
                        if proof.watcher_epoch != binding.gateway_epoch || proof.challenge != owner_epoch || sequence >= proof.sequence || tokio::time::Instant::now() >= *deadline.borrow() { break; }
                        sequence = proof.sequence;
                        deadline.send_replace(tokio::time::Instant::now()+WATCH_DEADLINE);
                        let control = serde_json::json!({"type":"authority-proof","lease_id":binding.lease_id,"watcher_epoch":binding.gateway_epoch,"sequence":sequence});
                        if local_sink.send(Message::Text(control.to_string().into())).await.is_err() { break; }
                        proof_ready.send_replace(true);
                    }
                }
                axum::extract::ws::Message::Ping(_) | axum::extract::ws::Message::Pong(_) => {},
                _ => break,
            }
        }
    };
    let download = async {
        let Ok(issued) = now_ms() else { return; };
        let Ok(challenge) = signed(&auth,&node,&binding,&owner_epoch,0,&owner_epoch,issued) else { return; };
        if sink.send(axum::extract::ws::Message::Text(challenge.into())).await.is_err() { return; }
        let mut sequence = 0_u64;
        while let Some(Ok(message)) = local_source.next().await {
            match message {
                Message::Binary(bytes) => {
                    if !*proof_ready.borrow() || tokio::time::Instant::now() >= *deadline.borrow() { break; }
                    if sink.send(axum::extract::ws::Message::Binary(bytes)).await.is_err() { break; }
                }
                Message::Text(text) => {
                    let value: Value = match serde_json::from_str(&text) { Ok(value) => value, Err(_) => break };
                    let next = value.get("sequence").and_then(Value::as_u64).unwrap_or_default();
                    if value.get("type").and_then(Value::as_str) != Some("owner-proof") || value.get("lease_id").and_then(Value::as_str) != Some(binding.lease_id.as_str()) || next <= sequence { break; }
                    sequence = next;
                    let Some(issued) = value.get("issued_at_unix_ms").and_then(Value::as_u64).map(u128::from) else { break; };
                    if !now_ms().is_ok_and(|now| now.abs_diff(issued) <= 1_000) { break; }
                    let Ok(proof) = signed(&auth,&node,&binding,&owner_epoch,sequence,&owner_epoch,issued) else { break; };
                    if sink.send(axum::extract::ws::Message::Text(proof.into())).await.is_err() { break; }
                }
                Message::Ping(_) | Message::Pong(_) => {},
                _ => break,
            }
        }
    };
    let expired = async {
        let mut changed = deadline.subscribe();
        loop {
            let until = *changed.borrow_and_update();
            tokio::select! { biased; result = changed.changed() => { if result.is_err() { break; } }, () = tokio::time::sleep_until(until) => break }
        }
    };
    let local_closed = async {
        loop {
            let Ok(mut ready) = monitor.readable().await else { break; };
            if ready.ready().is_read_closed() || ready.ready().is_error() { break; }
            ready.clear_ready();
        }
    };
    tokio::select! { biased; () = expired => {}, () = local_closed => {}, () = upload => {}, () = download => {} }
}
