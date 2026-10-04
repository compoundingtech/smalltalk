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
    blocking_action(move || store.check_mailbox(&binding)).await?;
    // Wake the predecessor immediately, even when no graph content changed.
    signal_local_change(&state);
    Ok(websocket.on_upgrade(move |socket| stream(state, fence, socket)))
}

pub(super) async fn bind(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Fence>,
) -> Result<Json<Fence>, ApiError> {
    authorize(&request, peer.as_ref().map(|p| &p.0))?;
    let store = state.store.clone();
    let bound = blocking_action(move || store.bind_mailbox(&request)).await?;
    signal_local_change(&state);
    Ok(Json(bound))
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
    let kind = input.kind.clone();
    let (record, appended, work_wake) = blocking_action(move || {
        let (record, appended) = store.append_mailbox_receipt_outcome(&input, &request.fence)?;
        // A message this store cannot read is treated as a work wake.
        let work_wake = store
            .message(&input.subject)
            .ok()
            .flatten()
            .is_none_or(|message| super::is_work_wake(&message.tags));
        Ok((record, appended, work_wake))
    })
    .await?;
    // A repeated or already-settled receipt changes nothing; only a new claim wakes readers.
    if appended {
        super::signal_message_changed(&state, &kind, work_wake);
    }
    Ok(Json(record))
}

type Snapshot = (
    Option<crate::model::DesiredSubject>,
    Vec<crate::model::MessageView>,
);

fn snapshot(store: &Store, binding: &Fence) -> anyhow::Result<Snapshot> {
    store.check_mailbox(binding).map_err(anyhow::Error::new)?;
    let seat = store
        .desired_subjects_named(std::slice::from_ref(&binding.subject))?
        .into_iter()
        .next();
    let messages = if binding.component == "delivery" {
        let mut messages = Vec::new();
        for message in store
            .messages(Some(&binding.subject), false)?
            .into_iter()
            .filter(|message| matches!(message.status.as_str(), "sent" | "staged" | "delivered"))
        {
            // A watch never tells a seat about what it posted: a wake for a comment the seat
            // recorded closes here, and a thread's wakes wait while the seat's post is in flight.
            if let Some((locator, kind, id)) = crate::github_watch::named_object(&message) {
                if crate::github_watch::post_in_flight(&binding.subject, &message) {
                    continue;
                }
                if store.github_post_agent(&locator, &kind, id)?.as_deref()
                    == Some(binding.subject.as_str())
                {
                    store.close_own_post_wakes(&binding.subject)?;
                    continue;
                }
            }
            if store.rollout_message_allowed(&message)? {
                messages.push(message);
            }
        }
        messages
    } else {
        Vec::new()
    };
    Ok((seat, messages))
}

/// The longest a mailbox stream goes without reading its seat's mailbox in full.
const MAILBOX_FULL_SNAPSHOT: Duration = Duration::from_secs(60);

async fn stream(state: AppState, fence: Fence, socket: WebSocket) {
    stream_with_reader(state, fence, socket, snapshot).await;
}

async fn stream_with_reader<F>(state: AppState, fence: Fence, mut socket: WebSocket, read: F)
where
    F: Fn(&Store, &Fence) -> anyhow::Result<Snapshot> + Clone + Send + 'static,
{
    // Subscribe before reading to close the replay-to-live race. Watch coalesces writes; every
    // wake recomputes the durable state, so lag needs no lossy event cursor.
    let mut changed = state.event_notify.subscribe();
    let mut previous_seat = Vec::new();
    let mut previous_mailbox = Vec::new();
    let mut previous_drain = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    let mut dirty = true;
    // What the last snapshot read, and when the last full snapshot ran. A wake reads the seat's
    // mailbox again only when something it depends on changed, or a minute has passed.
    let mut last: Option<(
        crate::store::MailboxWatermark,
        Vec<String>,
        tokio::time::Instant,
    )> = None;
    loop {
        if dirty {
            changed.borrow_and_update();
            if let Some((mark, subjects, at)) = last.clone()
                && at.elapsed() < MAILBOX_FULL_SNAPSHOT
            {
                let store = state.store.clone();
                let binding = fence.clone();
                let unchanged = tokio::task::spawn_blocking(move || {
                    crate::profile::task("task mailbox-change-check", || {
                        store.mailbox_changed_since(&binding, &mark, &subjects)
                    })
                })
                .await
                .is_ok_and(|changed| changed.is_ok_and(|changed| !changed));
                if unchanged {
                    dirty = false;
                }
            }
        }
        if dirty {
            let store = state.store.clone();
            let binding = fence.clone();
            let read = read.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::profile::task("task mailbox-snapshot", || {
                    let mark = store.mailbox_watermark(&binding);
                    (mark, read(&store, &binding))
                })
            })
            .await;
            let (mark, result) = match result {
                Ok((mark, result)) => (mark.ok(), Ok(result)),
                Err(error) => (None, Err(error)),
            };
            let (seat, mut messages) = match result {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => {
                    if error
                        .downcast_ref::<St3Error>()
                        .is_some_and(|error| error.code == "stale-mailbox-session")
                    {
                        finish_fenced(
                            &mut socket,
                            &Frame::Fenced {
                                reason: error.to_string(),
                            },
                        )
                        .await;
                    }
                    // Transient reads and join failures close only this connection. The current
                    // owner reconnects under the same epoch and replays without a receipt.
                    return;
                }
                Err(_) => return,
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
            // A watch's wake shows its comment's opening words, read from GitHub now and never
            // stored in the graph.
            crate::github_watch::add_excerpts(&mut messages).await;
            last = mark.map(|mark| {
                let subjects = messages
                    .iter()
                    .map(|message| message.subject.clone())
                    .collect();
                (mark, subjects, tokio::time::Instant::now())
            });
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
            if fence.component == "delivery" {
                let drain = state
                    .store
                    .rollout(&fence.subject)
                    .ok()
                    .flatten()
                    .filter(|o| o.holds_intake() && o.old_incarnation == fence.incarnation)
                    .map(|o| o.id);
                if drain != previous_drain {
                    if send(
                        &mut socket,
                        &Frame::Drain {
                            operation: drain.clone(),
                        },
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    previous_drain = drain;
                }
            }
            dirty = false;
        }
        tokio::select! {
            event = changed.changed() => { if event.is_err() { return; } dirty = true; },
            incoming = socket.recv() => match incoming {
                Some(Ok(WsMessage::Text(report))) => {
                    if fence.component == "delivery" && state.store.check_mailbox(&fence).is_ok() {
                        delivery_presence::record(&fence.subject, &report);
                        if let Ok(value) = serde_json::from_str::<Value>(&report)
                            && let Some(id) = value["drain_operation"].as_str()
                            && let Ok(Some(operation)) = state.store.rollout(&fence.subject)
                            && operation.id == id && operation.old_incarnation == fence.incarnation && operation.drain_ack.is_none()
                        {
                            let _ = crate::rollout::phase(&state.store, &fence.subject, &operation, "drain-ack", None, &[]);
                            signal_changed(&state);
                        }
                    }
                },
                Some(Ok(WsMessage::Pong(_))) => {},
                Some(Ok(WsMessage::Ping(bytes))) => { if socket.send(WsMessage::Pong(bytes)).await.is_err() { return; } },
                _ => return,
            },
            _ = heartbeat.tick() => {
                if let Err(error) = state.store.check_mailbox(&fence) {
                    if error.code == "stale-mailbox-session" {
                        finish_fenced(&mut socket, &Frame::Fenced { reason: error.to_string() }).await;
                    }
                    return;
                }
                if socket.send(WsMessage::Ping(Vec::new().into())).await.is_err() { return; }
            },
        }
    }
}

async fn finish_fenced(socket: &mut WebSocket, frame: &Frame) {
    if send(socket, frame).await.is_err() {
        return;
    }
    // Keep the read side alive through the close handshake: the peer may still be
    // answering an already queued heartbeat before it sees the fencing frame.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        socket.send(WsMessage::Close(None)).await?;
        while let Some(message) = socket.recv().await {
            if matches!(message, Ok(WsMessage::Close(_)) | Err(_)) {
                break;
            }
        }
        Ok::<(), axum::Error>(())
    })
    .await;
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
    async fn a_restarted_native_mailbox_delivers_a_watch_wake_queued_while_the_seat_was_stopped() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        let seat = "agent/eval.worker";
        let source = "version 2\nagent \"eval.worker\" { workspace \"/tmp\"; command \"true\"; }\n";
        let apply = |source: &str, key: &str| {
            state
                .store
                .apply_internal(
                    &crate::graph::parse_test_intent(source, "node").unwrap(),
                    key,
                )
                .unwrap();
        };
        apply(source, "declare");
        crate::mailbox::tests::ready(&state.store, "before-stop");
        let thread = crate::github_watch::ThreadRef::parse("acme/garden#12").unwrap();
        state.store.declare_watch(&thread, seat, None).unwrap();
        let observe = |comments: Value| {
            let subscriptions = state
                .store
                .desired_subjects()
                .unwrap()
                .into_iter()
                .filter_map(|desired| {
                    crate::graph::subscription_spec(&desired.desired)
                        .filter(|spec| !spec.stopped)
                        .map(|spec| (desired.subject, spec))
                })
                .collect::<Vec<_>>();
            state.store.record_resource_observation(
                &thread.observer(), &state.store.selected_desired_revision(&thread.observer()).unwrap().unwrap(),
                None, &thread.resource(), None,
                &json!({"repository_id": 7, "issues": [{"number": 12, "new": false, "state": "open", "recent_comments": comments}]}),
                0, &subscriptions,
            ).unwrap();
        };
        observe(json!([]));
        apply("version 2\nstop \"agent/eval.worker\"\n", "stop");
        let reconciler = crate::reconcile::Reconciler::new(
            state.store.clone(),
            Arc::new(crate::reconcile::NativeRuntime::new(
                root.path(),
                None,
                Path::new("pty"),
            )),
            "node".into(),
            state.notify.clone(),
        );
        let reconcile = || {
            reconciler
                .reconcile_github_watches(&state.store.desired_subjects().unwrap())
                .unwrap()
        };
        reconcile();
        observe(
            json!([{"kind": "comment", "id": 91, "author": "fern-example", "at": chrono::Utc::now().to_rfc3339()}]),
        );
        let queued = state.store.messages(Some(seat), false).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, "sent");
        apply(source, "start");
        crate::mailbox::tests::ready(&state.store, "after-start");
        reconcile();
        assert_eq!(
            state
                .store
                .watch_view(&thread.watch(seat))
                .unwrap()
                .unwrap()["state"],
            "active"
        );

        let peer = NativeDeliveryPeer {
            agent: seat.into(),
            transport: "claude-channel",
            pid: 37,
            archives_inbox: false,
        };
        let app = router(state.clone()).layer(Extension(peer));
        let path = root.path().join("daemon.sock");
        let server_path = path.clone();
        let server = tokio::spawn(async move { serve_unix(&server_path, app).await.unwrap() });
        let client = Client::new(Endpoint::Unix(path.clone()));
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let fence: Fence = client
            .post(
                "/v1/mailbox/bind",
                &Fence::new(seat, "after-start", "delivery"),
            )
            .await
            .unwrap();
        let mut mailbox = client.open_mailbox(&fence).await.unwrap();
        assert!(matches!(next(&mut mailbox).await, Frame::Seat { .. }));
        assert!(
            matches!(next(&mut mailbox).await, Frame::Mailbox { messages } if messages.len() == 1 && messages[0].subject == queued[0].subject)
        );
        let _: ClaimRecord = client
            .post(
                "/v1/mailbox/receipts",
                &Receipt {
                    fence,
                    message: queued[0].subject.clone(),
                    lifecycle: "delivered".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            state.store.messages(Some(seat), false).unwrap()[0].status,
            "delivered"
        );
        server.abort();
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
            let mut state = super::super::tests::state(root.path());
            // Exercise the daemon's WAL database, rather than SQLite's shared-cache
            // in-memory fixture, whose concurrent reads can reject a writer immediately.
            state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
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
                                let lost = matches!(request.uri().path(), "/v1/mailbox/receipts" | "/v1/mailbox/bind")
                                    && injection.swap(false, std::sync::atomic::Ordering::SeqCst);
                                let response = next.run(request).await;
                                if lost {
                                    if !response.status().is_success() {
                                        let status = response.status();
                                        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                                        panic!("receipt must commit before losing its response: {status} {}", String::from_utf8_lossy(&body));
                                    }
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
            let request = Fence::new("agent/eval.worker", "session-1", "delivery");
            lose_response.store(true, std::sync::atomic::Ordering::SeqCst);
            let lost_bind: anyhow::Result<Fence> = client.post("/v1/mailbox/bind", &request).await;
            assert!(
                lost_bind.is_err(),
                "the binding committed but its response was discarded"
            );
            let fence: Fence = client.post("/v1/mailbox/bind", &request).await.unwrap();
            let retry: Fence = client.post("/v1/mailbox/bind", &request).await.unwrap();
            assert_eq!(retry.epoch, fence.epoch);
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
                let events = state.event_notify.subscribe();
                let retry: ClaimRecord =
                    client.post("/v1/mailbox/receipts", &receipt).await.unwrap();
                // A retried receipt appends nothing and wakes no mailbox reader (#1085).
                assert!(
                    !events.has_changed().unwrap(),
                    "retried {lifecycle} woke readers"
                );
                assert_eq!(
                    first.id, retry.id,
                    "lost {lifecycle} acknowledgement is idempotent"
                );
            }
            assert_eq!(
                state.store.message(&send.subject).unwrap().unwrap().status,
                "read"
            );
            let settled = Receipt {
                fence: fence.clone(),
                message: send.subject.clone(),
                lifecycle: "delivered".into(),
            };
            let events = state.event_notify.subscribe();
            let _: ClaimRecord = client.post("/v1/mailbox/receipts", &settled).await.unwrap();
            assert!(
                !events.has_changed().unwrap(),
                "a settled receipt woke readers"
            );
            assert_eq!(
                state
                    .store
                    .claims_for(&send.subject, Some("message.delivered"))
                    .unwrap()
                    .len(),
                1
            );
            let predecessor = tokio::spawn(async move {
                loop {
                    if matches!(next(&mut socket).await, Frame::Fenced { .. }) {
                        break;
                    }
                }
            });
            tokio::task::yield_now().await;
            let replacement: Fence = client
                .post(
                    "/v1/mailbox/bind",
                    &Fence::new("agent/eval.worker", "session-1", "delivery"),
                )
                .await
                .unwrap();
            assert_eq!(replacement.epoch, fence.epoch + 1);
            let mut successor = client.open_mailbox(&replacement).await.unwrap();
            assert!(matches!(next(&mut successor).await, Frame::Seat { .. }));
            assert!(
                matches!(next(&mut successor).await, Frame::Mailbox { messages } if messages.is_empty())
            );
            predecessor.await.unwrap();
            let retired: anyhow::Result<Fence> = client.post("/v1/mailbox/bind", &request).await;
            assert!(
                retired.is_err(),
                "a lost initial response cannot let a predecessor allocate another epoch"
            );
            // A new owner may replay staged after delivered/read, even with a different key.
            let late_stage = Receipt {
                fence: replacement.clone(),
                message: send.subject.clone(),
                lifecycle: "staged".into(),
            };
            let _: ClaimRecord = client
                .post("/v1/mailbox/receipts", &late_stage)
                .await
                .unwrap();
            assert_eq!(
                state.store.message(&send.subject).unwrap().unwrap().status,
                "read"
            );
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
    #[tokio::test]
    async fn current_owner_reconnects_and_replays_after_an_injected_snapshot_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        crate::mailbox::tests::ready(&state.store, "session-1");
        let input = ClaimInput {
            subject: "message/transient".into(),
            kind: "message.sent".into(),
            actor: Some("person/eval".into()),
            fields: BTreeMap::from([
                ("status".into(), json!("sent")),
                ("from".into(), json!("person/eval")),
                ("to".into(), json!("agent/eval.worker")),
                ("content".into(), json!("QUARTZ SIGNAL")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("transient-send".into()),
        };
        state.store.append_claim(&input).unwrap();
        let fence = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let injected = calls.clone();
        let app = Router::new()
            .route(
                "/v1/mailbox",
                get(
                    move |State(state): State<AppState>,
                          Query(fence): Query<Fence>,
                          websocket: WebSocketUpgrade| {
                        let calls = injected.clone();
                        async move {
                            state.store.check_mailbox(&fence).unwrap();
                            websocket.on_upgrade(move |socket| {
                                stream_with_reader(state, fence, socket, move |store, fence| {
                                    let call =
                                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    match call {
                                        0 => Err(anyhow::Error::new(St3Error::new(
                                            "internal",
                                            "injected SQLITE_BUSY",
                                        ))),
                                        1 => Err(anyhow::anyhow!(
                                            "injected seat/message snapshot read failure"
                                        )),
                                        2 => panic!("injected snapshot worker join failure"),
                                        _ => snapshot(store, fence),
                                    }
                                })
                            })
                        }
                    },
                ),
            )
            .with_state(state.clone());
        let path = root.path().join("daemon.sock");
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            serve_unix(&server_path, app).await.unwrap();
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut subscription = crate::mailbox::Subscription::start(
            Client::new(Endpoint::Unix(path)),
            fence.clone(),
            json!({}),
        );
        let frame = tokio::time::timeout(Duration::from_secs(6), subscription.receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(frame, Frame::Mailbox { messages } if messages.len() == 1 && messages[0].subject == input.subject)
        );
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) >= 4);
        state.store.check_mailbox(&fence).unwrap();
        assert_eq!(
            state.store.message(&input.subject).unwrap().unwrap().status,
            "sent",
            "reconnect writes no receipt"
        );
        assert!(!root.path().join("resources").exists());
        server.abort();
    }

    #[test]
    fn a_mailbox_stream_reads_again_only_for_what_its_snapshot_depends_on() {
        let store = Store::open_memory("node").unwrap();
        crate::mailbox::tests::ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let send = |subject: &str, to: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.sent".into(),
                    actor: Some("person/eval".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("person/eval")),
                        ("to".into(), json!(to)),
                        ("content".into(), json!("A note.")),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(subject.into()),
                })
                .unwrap();
        };
        let mark = store.mailbox_watermark(&fence).unwrap();
        let mine = vec!["message/mine".to_owned()];
        // Another seat's mail changes nothing this stream reads.
        send("message/other", "agent/eval.other");
        assert!(!store.mailbox_changed_since(&fence, &mark, &[]).unwrap());
        // Mail to this seat does.
        send("message/mine", "agent/eval.worker");
        assert!(store.mailbox_changed_since(&fence, &mark, &[]).unwrap());
        // So does a lifecycle claim of a message in its last snapshot.
        let mark = store.mailbox_watermark(&fence).unwrap();
        assert!(!store.mailbox_changed_since(&fence, &mark, &mine).unwrap());
        store
            .append_claim(&ClaimInput {
                subject: "message/mine".into(),
                kind: "message.staged".into(),
                actor: Some("agent/eval.worker".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("staged")),
                    ("recipient".into(), json!("agent/eval.worker")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("mine-staged".into()),
            })
            .unwrap();
        assert!(store.mailbox_changed_since(&fence, &mark, &mine).unwrap());
        // And a newer channel taking the seat over.
        let mark = store.mailbox_watermark(&fence).unwrap();
        crate::mailbox::tests::ready(&store, "session-2");
        let _newer = store.bind_mailbox(&Fence::new("agent/eval.worker", "session-2", "delivery"));
        assert!(store.mailbox_changed_since(&fence, &mark, &mine).unwrap());
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
