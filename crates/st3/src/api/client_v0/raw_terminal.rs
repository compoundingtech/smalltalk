//! Full-fidelity terminal transport. The PTY, not a gateway emulator, owns replay and geometry.
use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
mod lease;

const SUBPROTOCOL: &str = "st3.client.pty.v0";
const CHUNK: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachmentRequest {
    runtime_incarnation: String,
    mode: st3_client::RawTerminalMode,
}

fn mode_name(mode: st3_client::RawTerminalMode) -> &'static str {
    match mode {
        st3_client::RawTerminalMode::Attach => "attach",
        st3_client::RawTerminalMode::Peek => "peek",
    }
}

fn authorize(session: &ClientSession, mode: st3_client::RawTerminalMode) -> Result<(), ApiError> {
    require_scope(session, "terminal.read")?;
    if !session.authority_actor.starts_with("person/") {
        return Err(forbidden(
            "raw terminal transport requires a concrete person",
        ));
    }
    if mode == st3_client::RawTerminalMode::Attach {
        require_scope(session, "terminal.control")?;
    }
    Ok(())
}

pub(super) fn authorization_epoch(
    state: &AppState,
    session: &ClientSession,
) -> Result<String, ApiError> {
    lease::authorization_epoch(state, session)
}

pub(crate) async fn attachment(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<AttachmentRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&session, request.mode)?;
    let terminal_id = client_detail_id("terminal", &id);
    let live =
        remote_terminal_live_session(&state, &terminal_subject(&id), &request.runtime_incarnation)?;
    if !live.terminal {
        return Err(validation("raw attachment requires a terminal runtime"));
    }
    if live.owner_host_id != client_host_id(&state.node)
        && state
            .client_relay
            .as_ref()
            .is_none_or(|relay| !relay.reaches(&live.owner_host_id))
    {
        return Err(remote_unavailable_for_owner(&state, &live.owner_host_id));
    }
    let attachment_id = format!("terminal-attachment/{}", new_request_id());
    let capability =
        derive_terminal_capability(&state, &session.actor, &attachment_id, &live.owner_host_id)?;
    state
        .store
        .append_claim(&ClaimInput {
            subject: terminal_attachment_subject(&attachment_id)?,
            kind: "custom.client.terminal-attached".into(),
            actor: Some(session_claim_actor(&session)),
            fields: BTreeMap::from([
                ("attachment_id".into(), json!(attachment_id)),
                ("terminal_id".into(), json!(terminal_id)),
                ("runtime_incarnation".into(), json!(live.incarnation_id)),
                ("runtime_id".into(), json!(live.runtime_id)),
                ("owner_host_id".into(), json!(live.owner_host_id)),
                ("session_actor".into(), json!(session.actor)),
                ("person_id".into(), json!(session.authority_actor)),
                ("raw_mode".into(), json!(mode_name(request.mode))),
                (
                    "raw_authorization_epoch".into(),
                    if request.mode == st3_client::RawTerminalMode::Peek {
                        json!(authorization_epoch(&state, &session)?)
                    } else {
                        Value::Null
                    },
                ),
                (
                    "capability_hash".into(),
                    json!(credential_digest(&capability)),
                ),
                ("expires_at_unix_ms".into(), json!(client_now_ms() + 60_000)),
            ]),
            evidence: Vec::new(),
            expected_subject: Some(None),
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(json!({
        "terminal_id": terminal_id,
        "runtime_incarnation": live.incarnation_id,
        "owner_host_id": live.owner_host_id,
        "mode": mode_name(request.mode),
        "stream_capability": capability,
    })))
}

#[derive(Deserialize)]
pub(crate) struct StreamQuery {
    incarnation: String,
    mode: st3_client::RawTerminalMode,
}

pub(crate) async fn stream(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&session, query.mode)?;
    let protocols = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    let capabilities = protocols
        .iter()
        .filter_map(|protocol| protocol.strip_prefix(TERMINAL_CAPABILITY_PROTOCOL_PREFIX))
        .collect::<Vec<_>>();
    if protocols.len() != 2
        || protocols
            .iter()
            .filter(|protocol| **protocol == SUBPROTOCOL)
            .count()
            != 1
        || capabilities.len() != 1
    {
        return Err(validation(
            "raw terminal requires st3.client.pty.v0 plus one st3.cap.* protocol",
        ));
    }
    let live = remote_terminal_live_session(&state, &terminal_subject(&id), &query.incarnation)?;
    if !live.terminal {
        return Err(validation("raw attachment requires a terminal runtime"));
    }
    consume_terminal_attachment_mode(
        &state,
        &session,
        &client_detail_id("terminal", &id),
        &query.incarnation,
        Some(capabilities[0]),
        Some(mode_name(query.mode)),
    )?;
    // Open and fence before HTTP upgrade: a stale owner incarnation is a refusal, not a blank pane.
    let transport = if live.owner_host_id == client_host_id(&state.node) {
        let terminal = crate::model::LocalTerminal {
            subject: terminal_subject(&id),
            runtime_id: live.runtime_id,
            incarnation_id: live.incarnation_id,
            pty_root: state.pty_root.clone(),
        };
        let stream = crate::client::open_local_terminal(&terminal)
            .await
            .map_err(|error| stale(error.to_string()))?;
        stream.set_nonblocking(true).map_err(ApiError::internal)?;
        UnixStream::from_std(stream).map_err(ApiError::internal)?
    } else {
        state
            .client_relay
            .as_ref()
            .ok_or_else(|| remote_unavailable_for_owner(&state, &live.owner_host_id))?
            .raw_terminal(
                &live.owner_host_id,
                &session.authority_actor,
                &client_detail_id("terminal", &id),
                &query.incarnation,
                query.mode,
            )
            .await
            .map_err(|error| remote_read_error(&live.owner_host_id, error))?
    };
    Ok(websocket
        .protocols([SUBPROTOCOL])
        .max_message_size(CHUNK * 4)
        .max_frame_size(CHUNK * 4)
        .on_upgrade(move |socket| splice(socket, transport, Some(query.mode))))
}

/// One bounded byte splice; closing either direction drops the persistent owner connection.
/// A raw client may not use lifecycle/CAS/ancestry commands to escape terminal authority.
pub(crate) async fn splice(
    socket: WebSocket,
    transport: UnixStream,
    mode: Option<st3_client::RawTerminalMode>,
) {
    let (mut sink, mut source) = socket.split();
    let (mut reader, mut writer) = transport.into_split();
    let upload = async {
        let mut gate = FrameGate::new(mode);
        while let Some(Ok(message)) = source.next().await {
            match message {
                axum::extract::ws::Message::Binary(bytes) => {
                    if gate.write(&mut writer, &bytes).await.is_err() {
                        break;
                    }
                }
                axum::extract::ws::Message::Ping(_) | axum::extract::ws::Message::Pong(_) => {}
                _ => break,
            }
        }
    };
    let download = async {
        let mut bytes = [0_u8; CHUNK];
        loop {
            let Ok(count) = reader.read(&mut bytes).await else {
                break;
            };
            if count == 0
                || sink
                    .send(axum::extract::ws::Message::Binary(
                        bytes[..count].to_vec().into(),
                    ))
                    .await
                    .is_err()
            {
                break;
            }
        }
    };
    tokio::select! { () = upload => {}, () = download => {} }
}

/// Validate frame headers before forwarding them, without copying or buffering payloads.
struct FrameGate {
    mode: Option<st3_client::RawTerminalMode>,
    header: [u8; 5],
    used: usize,
    remaining: usize,
    attached: bool,
}

impl FrameGate {
    fn new(mode: Option<st3_client::RawTerminalMode>) -> Self {
        Self {
            mode,
            header: [0; 5],
            used: 0,
            remaining: 0,
            attached: false,
        }
    }

    fn accept_header(&mut self) -> std::io::Result<()> {
        use pty_core::protocol::MessageType;
        let tag = MessageType::from_u8(self.header[0]);
        let length =
            u32::from_be_bytes(self.header[1..].try_into().expect("four-byte header")) as usize;
        let allowed = match self.mode {
            None => true,
            Some(st3_client::RawTerminalMode::Attach) if !self.attached => {
                tag == MessageType::Attach
            }
            Some(st3_client::RawTerminalMode::Peek) if !self.attached => tag == MessageType::Peek,
            Some(st3_client::RawTerminalMode::Attach) => matches!(
                tag,
                MessageType::Data
                    | MessageType::Resize
                    | MessageType::Detach
                    | MessageType::Status
                    | MessageType::ResetInputModes
            ),
            Some(st3_client::RawTerminalMode::Peek) => {
                matches!(tag, MessageType::Detach | MessageType::Status)
            }
        };
        if !allowed
            || length > 16 * 1024 * 1024
            || (tag == MessageType::ResetInputModes && length != 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "PTY frame exceeds raw terminal capability",
            ));
        }
        self.attached = true;
        self.remaining = length;
        Ok(())
    }

    async fn write(
        &mut self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
        mut bytes: &[u8],
    ) -> std::io::Result<()> {
        while !bytes.is_empty() {
            if self.remaining == 0 {
                let count = (5 - self.used).min(bytes.len());
                self.header[self.used..self.used + count].copy_from_slice(&bytes[..count]);
                self.used += count;
                bytes = &bytes[count..];
                if self.used < 5 {
                    continue;
                }
                self.accept_header()?;
                writer.write_all(&self.header).await?;
                self.used = 0;
            } else {
                let count = self.remaining.min(bytes.len());
                writer.write_all(&bytes[..count]).await?;
                bytes = &bytes[count..];
                self.remaining -= count;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::protocol::{encode_attach, encode_data, encode_peek, encode_resize};
    use st3_client::RawTerminalMode;

    #[tokio::test]
    async fn peek_authority_rejects_writer_frames_before_forwarding_headers() {
        for prohibited in [
            encode_attach(1, 1),
            encode_data(b"must-not-reach-child"),
            encode_resize(1, 1),
            pty_core::protocol::encode_packet(
                pty_core::protocol::MessageType::ResetInputModes,
                &[],
            ),
        ] {
            let (mut bridge, mut owner) = tokio::io::duplex(128);
            let mut gate = FrameGate::new(Some(RawTerminalMode::Peek));
            let peek = encode_peek(false, true);
            gate.write(&mut bridge, &peek).await.unwrap();
            let mut forwarded = vec![0; peek.len()];
            owner.read_exact(&mut forwarded).await.unwrap();
            assert_eq!(forwarded, peek);
            // Even a header split over several WebSocket messages cannot bypass the mode gate.
            gate.write(&mut bridge, &prohibited[..2]).await.unwrap();
            let error = gate.write(&mut bridge, &prohibited[2..]).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            drop(bridge);
            let mut forbidden = Vec::new();
            owner.read_to_end(&mut forbidden).await.unwrap();
            assert_eq!(
                forbidden, b"",
                "no unauthorized header or payload may reach the PTY"
            );
        }
    }

    #[tokio::test]
    async fn writer_rejects_lifecycle_management_commands() {
        let (mut bridge, mut owner) = tokio::io::duplex(128);
        let mut gate = FrameGate::new(Some(RawTerminalMode::Attach));
        let attach = encode_attach(24, 80);
        gate.write(&mut bridge, &attach).await.unwrap();
        let mut forwarded = vec![0; attach.len()];
        owner.read_exact(&mut forwarded).await.unwrap();
        let cas =
            pty_core::protocol::encode_packet(pty_core::protocol::MessageType::LifecycleCas, b"{}");
        assert_eq!(
            gate.write(&mut bridge, &cas).await.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        drop(bridge);
        let mut forbidden = Vec::new();
        owner.read_to_end(&mut forbidden).await.unwrap();
        assert_eq!(forbidden, b"");
    }
    #[tokio::test]
    async fn attached_writer_can_recover_modes_but_cannot_add_a_payload() {
        let (mut bridge, mut owner) = tokio::io::duplex(128);
        let mut gate = FrameGate::new(Some(RawTerminalMode::Attach));
        let attach = encode_attach(24, 80);
        let reset = pty_core::protocol::encode_packet(
            pty_core::protocol::MessageType::ResetInputModes,
            &[],
        );
        gate.write(&mut bridge, &attach).await.unwrap();
        gate.write(&mut bridge, &reset[..2]).await.unwrap();
        gate.write(&mut bridge, &reset[2..]).await.unwrap();
        let expected = [attach, reset].concat();
        let mut bytes = vec![0; expected.len()];
        owner.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, expected);
        let malformed = pty_core::protocol::encode_packet(
            pty_core::protocol::MessageType::ResetInputModes,
            b"child input",
        );
        assert_eq!(
            gate.write(&mut bridge, &malformed)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        drop(bridge);
        let mut forbidden = Vec::new();
        owner.read_to_end(&mut forbidden).await.unwrap();
        assert!(forbidden.is_empty());
    }
}
