//! One local daemon stream per delivery component. Ownership and receipts are fenced in SQLite,
//! so an old channel reconnecting after a replacement cannot consume or acknowledge its mail.
use super::*;
use crate::mailbox::{Fence, Frame, Receipt};

pub(super) async fn subscribe(
    State(state): State<AppState>,
    Query(fence): Query<Fence>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    websocket: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    authorize(&fence, peer.as_ref().map(|p| &p.0))?;
    let store = state.store.clone();
    let binding = fence.clone();
    blocking_action(move || store.bind_mailbox(&binding)).await?;
    // Wake the predecessor immediately, even when no graph content changed.
    signal_local_change(&state);
    Ok(websocket.on_upgrade(move |socket| stream(state, fence, socket)))
}

fn authorize(fence: &Fence, peer: Option<&NativeDeliveryPeer>) -> Result<(), ApiError> {
    let Some(peer) = peer else {
        return Err(ApiError::bad(St3Error::new(
            "unbound-mailbox",
            "mailbox subscriptions require a local native driver",
        )));
    };
    if peer.agent != fence.subject || !matches!(fence.component.as_str(), "delivery" | "title") {
        return Err(ApiError::bad(St3Error::new(
            "foreign-mailbox",
            "the subscription must belong to this native seat",
        )));
    }
    Ok(())
}

pub(super) async fn receipt(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Receipt>,
) -> Result<Json<ClaimRecord>, ApiError> {
    authorize(&request.fence, peer.as_ref().map(|p| &p.0))?;
    if request.fence.component != "delivery"
        || !matches!(request.lifecycle.as_str(), "staged" | "delivered" | "read")
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mailbox-receipt",
            "only the delivery component can publish staged, delivered or read",
        )));
    }
    let input = ClaimInput {
        subject: request.message.clone(),
        kind: format!("message.{}", request.lifecycle),
        actor: Some(request.fence.subject.clone()),
        fields: if request.lifecycle == "staged" {
            BTreeMap::from([
                ("status".into(), json!(request.lifecycle)),
                ("recipient".into(), json!(request.fence.subject)),
                ("transport".into(), json!(peer.unwrap().0.transport)),
            ])
        } else {
            BTreeMap::from([("status".into(), json!(request.lifecycle))])
        },
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!(
            "mailbox:{}:{}:{}:{}:{}",
            request.fence.subject,
            request.fence.incarnation,
            request.fence.epoch,
            request.message,
            request.lifecycle
        )),
    };
    let store = state.store.clone();
    let record =
        blocking_action(move || store.append_mailbox_receipt(&input, &request.fence)).await?;
    signal_changed(&state);
    Ok(Json(record))
}

async fn stream(state: AppState, fence: Fence, mut socket: WebSocket) {
    // Subscribe before reading to close the replay-to-live race. Watch coalesces writes; every
    // wake recomputes the durable state, so lag needs no lossy event cursor.
    let mut changed = state.event_notify.subscribe();
    let mut previous_seat = Vec::new();
    let mut previous_mailbox = Vec::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    let mut dirty = true;
    loop {
        if dirty {
            changed.borrow_and_update();
            let store = state.store.clone();
            let binding = fence.clone();
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                store.check_mailbox(&binding).map_err(anyhow::Error::new)?;
                let seat = store
                    .desired_subjects_named(std::slice::from_ref(&binding.subject))?
                    .into_iter()
                    .next();
                let messages = if binding.component == "delivery" {
                    store.messages(Some(&binding.subject), false)?.into_iter()
                        .filter(|message| matches!(message.status.as_str(), "sent" | "staged" | "delivered")).collect()
                } else {
                    Vec::new()
                };
                Ok((seat, messages))
            })
            .await;
            let (seat, messages) = match result {
                Ok(Ok(snapshot)) => snapshot,
                error => {
                    let frame = Frame::Fenced {
                        reason: format!("subscription no longer owns this seat: {error:?}"),
                    };
                    let _ = send(&mut socket, &frame).await;
                    let _ = socket.close().await;
                    return;
                }
            };
            if let Some(seat) = seat {
                let bytes = serde_json::to_vec(&seat).unwrap_or_default();
                if bytes != previous_seat {
                    if send(
                        &mut socket,
                        &Frame::Seat {
                            seat: Box::new(seat),
                        },
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    previous_seat = bytes;
                }
            }
            let bytes = serde_json::to_vec(&messages).unwrap_or_default();
            if fence.component == "delivery" && bytes != previous_mailbox {
                if send(&mut socket, &Frame::Mailbox { messages })
                    .await
                    .is_err()
                {
                    return;
                }
                previous_mailbox = bytes;
            }
            dirty = false;
        }
        tokio::select! {
            event = changed.changed() => { if event.is_err() { return; } dirty = true; },
            incoming = socket.recv() => match incoming {
                Some(Ok(WsMessage::Text(report))) => {
                    if fence.component == "delivery" && state.store.check_mailbox(&fence).is_ok() {
                        delivery_presence::record(&fence.subject, &report);
                    }
                },
                Some(Ok(WsMessage::Pong(_))) => {},
                Some(Ok(WsMessage::Ping(bytes))) => { if socket.send(WsMessage::Pong(bytes)).await.is_err() { return; } },
                _ => return,
            },
            _ = heartbeat.tick() => {
                if state.store.check_mailbox(&fence).is_err() {
                    let _ = send(&mut socket, &Frame::Fenced { reason: "subscription replaced".into() }).await;
                    return;
                }
                if socket.send(WsMessage::Ping(Vec::new().into())).await.is_err() { return; }
            },
        }
    }
}

async fn send(socket: &mut WebSocket, frame: &Frame) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(15),
        socket.send(WsMessage::Text(serde_json::to_string(frame)?.into())),
    )
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, Endpoint};
    use tokio_tungstenite::tungstenite::Message;

    async fn next(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    ) -> Frame {
        loop {
            match tokio::time::timeout(Duration::from_secs(3), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.unwrap(),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    #[tokio::test]
    async fn every_harness_replays_and_receipts_over_a_real_unix_push_stream_without_files() {
        for transport in [
            "claude-channel",
            "pi-channel",
            "omp-channel",
            "app-server",
            "opencode-server",
        ] {
            let root = tempfile::tempdir().unwrap();
            let state = super::super::tests::state(root.path());
            crate::mailbox::tests::ready(&state.store, "session-1");
            let kdl = "version 2\nagent \"eval.worker\" { workspace \"/work\"; command \"sleep 60\"; name \"Quartz\"; }\n";
            let intent = crate::graph::parse_test_intent(kdl, "node").unwrap();
            let planned = state
                .store
                .mission(
                    &intent,
                    IntentInput {
                        kdl: kdl.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            state
                .store
                .apply(&intent, &planned.subject_tokens, "seat")
                .unwrap();
            let peer = NativeDeliveryPeer {
                agent: "agent/eval.worker".into(),
                transport,
                pid: 37,
                archives_inbox: false,
            };
            let lose_response = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let injection = lose_response.clone();
            let app =
                router(state.clone())
                    .layer(Extension(peer))
                    .layer(axum::middleware::from_fn(
                        move |request: axum::extract::Request, next: axum::middleware::Next| {
                            let injection = injection.clone();
                            async move {
                                let lost = request.uri().path() == "/v1/mailbox/receipts"
                                    && injection.swap(false, std::sync::atomic::Ordering::SeqCst);
                                let response = next.run(request).await;
                                if lost {
                                    Response::new(axum::body::Body::empty())
                                } else {
                                    response
                                }
                            }
                        },
                    ));
            let path = root.path().join("daemon.sock");
            let server_path = path.clone();
            let task = tokio::spawn(async move {
                serve_unix(&server_path, app).await.unwrap();
            });
            let client = Client::new(Endpoint::Unix(path.clone()));
            for _ in 0..100 {
                if path.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let fence = Fence::new("agent/eval.worker", "session-1", "delivery");
            let mut socket = client.open_mailbox(&fence).await.unwrap();
            assert!(matches!(next(&mut socket).await, Frame::Seat { .. }));
            assert!(
                matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty())
            );
            let send = ClaimInput {
                subject: "message/push-native".into(),
                kind: "message.sent".into(),
                actor: Some("person/eval".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/eval")),
                    ("to".into(), json!(fence.subject)),
                    ("content".into(), json!("QUARTZ SIGNAL")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("native-send".into()),
            };
            state.store.append_claim(&send).unwrap();
            signal_changed(&state);
            assert!(
                matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages[0].subject == send.subject)
            );
            // Disconnecting writes no receipt; reconnect is a replay of the same immutable ID.
            socket.close(None).await.unwrap();
            let mut socket = client.open_mailbox(&fence).await.unwrap();
            next(&mut socket).await;
            assert!(
                matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages[0].status == "sent")
            );
            for lifecycle in ["staged", "delivered", "read"] {
                let receipt = Receipt {
                    fence: fence.clone(),
                    message: send.subject.clone(),
                    lifecycle: lifecycle.into(),
                };
                let first: ClaimRecord = if lifecycle == "delivered" {
                    lose_response.store(true, std::sync::atomic::Ordering::SeqCst);
                    let lost: anyhow::Result<ClaimRecord> =
                        client.post("/v1/mailbox/receipts", &receipt).await;
                    assert!(
                        lost.is_err(),
                        "the daemon committed but its response was discarded"
                    );
                    assert_eq!(
                        state.store.message(&send.subject).unwrap().unwrap().status,
                        "delivered"
                    );
                    state
                        .store
                        .claims_for(&send.subject, Some("message.delivered"))
                        .unwrap()
                        .pop()
                        .unwrap()
                } else {
                    client.post("/v1/mailbox/receipts", &receipt).await.unwrap()
                };
                let retry: ClaimRecord =
                    client.post("/v1/mailbox/receipts", &receipt).await.unwrap();
                assert_eq!(
                    first.id, retry.id,
                    "lost {lifecycle} acknowledgement is idempotent"
                );
            }
            assert_eq!(
                state.store.message(&send.subject).unwrap().unwrap().status,
                "read"
            );
            let settled = Receipt { fence:fence.clone(),message:send.subject.clone(),lifecycle:"delivered".into() };
            let _: ClaimRecord = client.post("/v1/mailbox/receipts", &settled).await.unwrap();
            assert_eq!(state.store.claims_for(&send.subject, Some("message.delivered")).unwrap().len(), 1);
            let predecessor = tokio::spawn(async move {
                loop { if matches!(next(&mut socket).await, Frame::Fenced { .. }) { break; } }
            });
            tokio::task::yield_now().await;
            let replacement = Fence {
                epoch: fence.epoch + 1,
                ..fence.clone()
            };
            let mut successor = client.open_mailbox(&replacement).await.unwrap();
            assert!(matches!(next(&mut successor).await, Frame::Seat { .. }));
            assert!(
                matches!(next(&mut successor).await, Frame::Mailbox { messages } if messages.is_empty())
            );
            predecessor.await.unwrap();
            assert!(
                client.open_mailbox(&fence).await.is_err(),
                "the predecessor cannot reconnect"
            );
            let late = Receipt {
                fence: fence.clone(),
                message: send.subject.clone(),
                lifecycle: "read".into(),
            };
            let late: anyhow::Result<ClaimRecord> =
                client.post("/v1/mailbox/receipts", &late).await;
            assert!(
                late.is_err(),
                "predecessor receipts remain fenced after replacement"
            );
            assert!(!root.path().join("resources/inbox").exists());
            assert!(!root.path().join("resources/archive").exists());
            task.abort();
        }
    }
    #[test]
    fn mailbox_authority_refuses_remote_and_foreign_subscriptions() {
        let fence = Fence::new("agent/eval.worker", "session-1", "delivery");
        assert!(authorize(&fence, None).is_err());
        let peer = NativeDeliveryPeer {
            agent: "agent/eval.other".into(),
            transport: "omp-channel",
            pid: 37,
            archives_inbox: false,
        };
        assert!(authorize(&fence, Some(&peer)).is_err());
    }
}
