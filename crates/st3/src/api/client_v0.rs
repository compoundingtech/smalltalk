use super::*;
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use std::collections::BTreeSet;

pub(super) mod raw_terminal;
pub(super) mod resources;
pub(super) mod search;

const TERMINAL_SUBPROTOCOL: &str = "st3.client.terminal.v0";
const CONVERSATION_SUBPROTOCOL: &str = "st3.client.conversation.v0";
const COLLECTION_SUBPROTOCOL: &str = "st3.client.collections.v0";
const TERMINAL_CAPABILITY_PROTOCOL_PREFIX: &str = "st3.cap.";
pub(super) const LOCAL_PERSON_HEADER: &str = "x-st3-person";

pub(super) async fn request_latency(
    Extension(session): Extension<ClientSession>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    Ok(Json(json!({ "routes": super::request_latency_snapshot() })))
}

// A client holds one socket for all its current collection views. A subscription
// is a bounded window; history stays on the paged HTTP endpoints. A terminal is one
// more subscription on the same socket: whole screens, the latest only.
#[derive(Clone, Deserialize)]
struct CollectionSubscribe {
    kind: String,
    id: String,
    #[serde(default)]
    collection: String,
    limit: Option<usize>,
    person: Option<String>,
    actor: Option<String>,
    status: Option<String>,
    /// A terminal subscription names the terminal, the incarnation `terminal.attach` fenced,
    /// and the single-use stream capability that attach returned.
    terminal: Option<String>,
    incarnation: Option<String>,
    capability: Option<String>,
    /// A conversation subscription names an agent or a session.
    conversation: Option<String>,
}

struct CollectionSubscription {
    request: CollectionSubscribe,
    /// Whether the client has this subscription's first snapshot.
    delivered: bool,
    previous: BTreeMap<String, Value>,
    order: Vec<String>,
    has_more: bool,
}

const COLLECTION_MAX_SUBSCRIPTIONS: usize = 8;
/// The least time between two rereads of a socket's held windows. A window read can take a
/// few hundred milliseconds and the fleet commits about once a second, so rereading on every
/// commit kept a daemon busy for as long as a client stayed connected. Commits in between are
/// read together; a new subscription is still read at once.
const COLLECTION_REREAD_INTERVAL: Duration = Duration::from_millis(1_500);
// Observer grace periods and checkpoint waits can enter attention without a new claim.
const ATTENTION_CLOCK_INTERVAL: Duration = Duration::from_secs(30);

/// Claims that no collection window shows: rereading for them only costs.
fn collection_ignores(collection: &str, kind: &str) -> bool {
    if collection == "glasses" { return !kind.starts_with("glass."); }
    matches!(kind, "daemon.diagnostic" | "transport.observed")
        || (kind == "harness.usage" && collection != "agents")
}

pub(super) async fn collection_stream(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_scope(&session, "read.projections")?;
    let protocols = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(','))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if protocols != [COLLECTION_SUBPROTOCOL] {
        return Err(validation(
            "the collection WebSocket requires exactly st3.client.collections.v0",
        ));
    }
    Ok(websocket
        .protocols([COLLECTION_SUBPROTOCOL])
        .on_upgrade(move |socket| collection_stream_socket(socket, state, session)))
}

/// Read one bounded window. The whole read sees one SQLite snapshot, and the fence names
/// that snapshot's index, so commits landing meanwhile never tear or delay it.
async fn collection_items(
    state: &AppState,
    session: &ClientSession,
    request: &CollectionSubscribe,
) -> Result<(ClientSnapshot, Vec<Value>, bool), ApiError> {
    if !matches!(
        request.collection.as_str(),
        "missions" | "attention" | "agents" | "work" | "glasses"
    ) {
        return Err(validation("unknown collection subscription"));
    }
    if request.status.is_some() && request.collection != "agents" {
        return Err(validation("status filters are supported for agents only"));
    }
    let limit = request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS);
    if !(1..=CLIENT_MAX_PAGE_ITEMS).contains(&limit) {
        return Err(validation("collection limit must be 1 through 200"));
    }
    let person = if request.collection == "glasses" {
        if request.person.is_some() || request.actor.is_some() {
            return Err(validation("glasses select the session person"));
        }
        Some(glass_person(session, false)?)
    } else if request.collection == "attention" {
        person_filter(session, request.person.as_deref())?
    } else {
        None
    };
    let state = state.clone();
    let actor = request.actor.clone();
    let status = request.status.clone();
    let collection = request.collection.clone();
    let (snapshot, mut items, has_more) = super::blocking_store(move || {
        let store = state.store.clone();
        store.read_snapshot(|index| {
            let snapshot = client_snapshot_at(&state, index);
            let at = snapshot.created_at.clone();
            let mut items = match collection.as_str() {
                "missions" => {
                    let mut ids =
                        store.mission_collection_ids(false, 0, limit.saturating_add(1))?;
                    let mut has_more = ids.len() > limit;
                    ids.truncate(limit);
                    let mut items = mission_list_cards(&store, &ids)?;
                    has_more |= bound_mission_cards(&mut items)?;
                    return Ok((snapshot, items, has_more));
                }
                "glasses" => {
                    store.glasses(person.as_deref().expect("authenticated glass owner"), index)?
                }
                "attention" => client_attention_resources(&store, person.as_deref(), false)?,
                "agents" => client_agent_resources(&store, false, &at, index)?,
                "work" => client_work_resources(
                    &store,
                    actor.as_deref(),
                    false,
                    store.projection_time_at(index)?,
                    index,
                )?,
                _ => unreachable!(),
            };
            if let Some(status) = status {
                items.retain(|item| item["state"].as_str() == Some(status.as_str()));
            }
            let has_more = items.len() > limit;
            Ok((snapshot, items, has_more))
        })
    })
    .await?;
    items.truncate(limit);
    Ok((snapshot, items, has_more))
}

async fn send_collection(socket: &mut WebSocket, value: Value) -> bool {
    let Ok(payload) = serde_json::to_string(&value) else {
        return false;
    };
    if payload.len() > CLIENT_MAX_RESPONSE_BYTES {
        return false;
    }
    socket.send(WsMessage::Text(payload.into())).await.is_ok()
}

enum Refreshed {
    /// Up to date, whether or not anything was sent.
    Current,
    /// A retryable read failed; keep the subscription and schedule another read.
    Retry,
    /// A permanent refusal ended the subscription.
    Dropped,
    /// The socket closed.
    Closed,
}

/// Bring one subscription up to date from a fresh read: its snapshot first, then only what
/// changed.
async fn deliver_collection(
    socket: &mut WebSocket,
    subscription: &mut CollectionSubscription,
    read: Result<(ClientSnapshot, Vec<Value>, bool), ApiError>,
) -> Refreshed {
    let request = &subscription.request;
    let (snapshot, items, has_more) = match read {
        Ok(read) => read,
        Err(error) => {
            let retryable = client_error_retryable(error.status, Some(&error.code));
            let kind = if retryable { "resync" } else { "error" };
            let sent = send_collection(
                socket,
                json!({"kind":kind, "id":request.id,
                "code":client_error_code(Some(&error.code)), "message":error.message,
                "retryable":retryable}),
            )
            .await;
            return if !sent {
                Refreshed::Closed
            } else if retryable {
                Refreshed::Retry
            } else {
                Refreshed::Dropped
            };
        }
    };
    let order = items
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let current: BTreeMap<String, Value> = items
        .iter()
        .filter_map(|item| Some((item["id"].as_str()?.to_owned(), item.clone())))
        .collect();
    let sent = if !subscription.delivered {
        send_collection(socket, json!({"kind":"snapshot", "id":request.id, "collection":request.collection, "snapshot":snapshot, "items":items, "order":order, "has_more":has_more})).await
    } else {
        let upserts = current
            .iter()
            .filter(|(id, value)| subscription.previous.get(*id) != Some(*value))
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        let removes = subscription
            .previous
            .keys()
            .filter(|id| !current.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        if upserts.is_empty()
            && removes.is_empty()
            && order == subscription.order
            && has_more == subscription.has_more
        {
            true
        } else {
            send_collection(socket, json!({"kind":"changes", "id":request.id, "collection":request.collection, "snapshot":snapshot, "upserts":upserts, "removes":removes, "order":order, "has_more":has_more})).await
        }
    };
    if !sent {
        return Refreshed::Closed;
    }
    subscription.delivered = true;
    subscription.previous = current;
    subscription.order = order;
    subscription.has_more = has_more;
    Refreshed::Current
}

/// Wait for the next frame from any held terminal. `None` means its follower stopped.
async fn next_terminal_frame(
    terminals: &mut BTreeMap<String, watch::Receiver<TerminalFrame>>,
) -> (String, Option<TerminalFrame>) {
    let changes = terminals.iter_mut().map(|(id, receiver)| {
        Box::pin(async move {
            let frame = match receiver.changed().await {
                Ok(()) => Some(receiver.borrow_and_update().clone()),
                Err(_) => None,
            };
            (id.clone(), frame)
        })
    });
    futures_util::future::select_all(changes).await.0
}

async fn open_terminal_subscription(
    state: &AppState,
    session: &ClientSession,
    request: &CollectionSubscribe,
) -> Result<watch::Receiver<TerminalFrame>, ApiError> {
    let id = request
        .terminal
        .as_deref()
        .map(|terminal| terminal.trim_start_matches("terminal/"))
        .filter(|terminal| !terminal.is_empty())
        .ok_or_else(|| validation("a terminal subscription names its terminal"))?
        .to_owned();
    let follow = {
        let state = state.clone();
        let session = session.clone();
        let incarnation = request.incarnation.clone();
        let capability = request.capability.clone();
        tokio::task::spawn_blocking(move || {
            prepare_terminal_follow(
                &state,
                &session,
                &id,
                incarnation.as_deref(),
                capability.as_deref(),
            )
        })
        .await
        .map_err(ApiError::internal)??
    };
    let (sender, receiver) = watch::channel(TerminalFrame::Waiting);
    let state = state.clone();
    tokio::spawn(follow.run(state, TerminalSink::Subscription(sender)));
    Ok(receiver)
}

/// Where a conversation is read: here, or on the host that owns its session.
fn conversation_owner_host(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
) -> Result<Option<String>, ApiError> {
    let remote = super::managed_session_owner_at(
        &state.store,
        new_client_snapshot(state).store_index,
        session_id,
    )
    .map_err(ApiError::internal)?
    .and_then(|(_, _, origin)| origin)
    .filter(|origin| origin != state.store.origin())
    .map(|origin| client_host_id(&origin));
    if let Some(owner) = &remote {
        if !acting_party(session) {
            return Err(forbidden(
                "a remote conversation requires a concrete person or agent",
            ));
        }
        if state
            .client_relay
            .as_ref()
            .is_none_or(|relay| !relay.reaches(owner))
        {
            return Err(remote_unavailable(owner));
        }
    }
    Ok(remote)
}

/// A conversation's newest page, read here or relayed from its owner.
async fn conversation_page(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
    remote: Option<&str>,
) -> Result<Value, ApiError> {
    const PAGE: usize = 200;
    if let Some(owner) = remote {
        let relay = state
            .client_relay
            .as_ref()
            .ok_or_else(|| remote_unavailable(owner))?;
        return relay
            .read(
                owner,
                &crate::peer::ClientReadRequest {
                    authority_actor: session.authority_actor.clone(),
                    relay: None,
                    request: crate::peer::ClientReadOperation::Timeline {
                        session_id: session_id.to_owned(),
                        limit: PAGE,
                        cursor: None,
                    },
                },
            )
            .await
            .map_err(|error| remote_read_error(owner, error));
    }
    let (state, session, session_id) = (state.clone(), session.clone(), session_id.to_owned());
    tokio::task::spawn_blocking(move || {
        timeline_value(
            &state,
            &new_client_snapshot(&state),
            &session,
            &session_id,
            &ClientListQuery {
                limit: Some(PAGE),
                ..Default::default()
            },
        )
        .map(|page| page.0)
    })
    .await
    .map_err(ApiError::internal)?
}

/// What changed in a conversation after `after`, waiting up to `wait_ms` for something to.
async fn conversation_changes_value(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
    remote: Option<&str>,
    after: Option<&str>,
    wait_ms: u64,
) -> Result<Value, ApiError> {
    if let Some(owner) = remote {
        let relay = state
            .client_relay
            .as_ref()
            .ok_or_else(|| remote_unavailable(owner))?;
        return relay
            .read(
                owner,
                &crate::peer::ClientReadRequest {
                    authority_actor: session.authority_actor.clone(),
                    relay: None,
                    request: crate::peer::ClientReadOperation::ConversationChanges {
                        session_id: session_id.to_owned(),
                        after: after.map(str::to_owned),
                        wait_ms,
                    },
                },
            )
            .await
            .map_err(|error| remote_read_error(owner, error));
    }
    conversation_changes_local(state, session, session_id, after, wait_ms).await
}

/// Follow one conversation for a collection socket: its newest page, then each change, until
/// the socket stops listening. A change the server can no longer replay sends the page again.
fn conversation_stream_error(id: &str, error: &ApiError) -> Value {
    json!({"kind":"error", "id":id, "collection":"conversation", "code":client_error_code(Some(&error.code)), "message":error.message,
        "retryable":client_error_retryable(error.status, Some(&error.code))})
}

async fn follow_conversation(
    state: AppState,
    session: ClientSession,
    id: String,
    session_id: String,
    remote: Option<String>,
    outbox: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
) {
    let remote = remote.as_deref();
    let failed = |error: &ApiError| conversation_stream_error(&id, error);
    loop {
        // The cursor first, so nothing that lands while the page is read is lost.
        let start = match conversation_changes_value(&state, &session, &session_id, remote, None, 0)
            .await
        {
            Ok(start) => start,
            Err(error) => {
                if client_error_retryable(error.status, Some(&error.code)) {
                    // Say why, so a client showing its last copy can say that copy is stale.
                    if outbox.send((id.clone(), json!({"kind":"resync", "id":id, "collection":"conversation", "retryable":true, "code":error.code, "message":error.message}))).is_err() { return; }
                    tokio::time::sleep(COLLECTION_REREAD_INTERVAL).await;
                    continue;
                }
                let _ = outbox.send((id.clone(), failed(&error)));
                return;
            }
        };
        let page = match conversation_page(&state, &session, &session_id, remote).await {
            Ok(page) => page,
            Err(error) => {
                if client_error_retryable(error.status, Some(&error.code)) {
                    // Say why, so a client showing its last copy can say that copy is stale.
                    if outbox.send((id.clone(), json!({"kind":"resync", "id":id, "collection":"conversation", "retryable":true, "code":error.code, "message":error.message}))).is_err() { return; }
                    tokio::time::sleep(COLLECTION_REREAD_INTERVAL).await;
                    continue;
                }
                let _ = outbox.send((id.clone(), failed(&error)));
                return;
            }
        };
        let mut frame = json!({"kind":"conversation", "id":id, "collection":"conversation", "session_id":session_id, "replace":true, "items":page["items"], "has_more":page["page"]["has_more"]});
        // A page of long tool output can outgrow one frame: keep its newest entries.
        while frame_bytes(&frame) > CLIENT_MAX_RESPONSE_BYTES {
            let Some(items) = frame["items"]
                .as_array_mut()
                .filter(|items| items.len() > 1)
            else {
                break;
            };
            let drop = items.len().div_ceil(4);
            items.drain(..drop);
            frame["has_more"] = Value::Bool(true);
        }
        if outbox.send((id.clone(), frame)).is_err() {
            return;
        }
        let mut after = start["next_cursor"].as_str().map(str::to_owned);
        loop {
            match conversation_changes_value(
                &state,
                &session,
                &session_id,
                remote,
                after.as_deref(),
                10_000,
            )
            .await
            {
                Ok(changes) => {
                    if changes["items"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                    {
                        let frame = json!({"kind":"conversation", "id":id, "collection":"conversation", "session_id":session_id, "replace":false, "items":changes["items"]});
                        // Too much changed for one frame: send the newest page instead.
                        if frame_bytes(&frame) > CLIENT_MAX_RESPONSE_BYTES {
                            break;
                        }
                        if outbox.send((id.clone(), frame)).is_err() {
                            return;
                        }
                    }
                    after = changes["next_cursor"].as_str().map(str::to_owned);
                }
                Err(error)
                    if matches!(error.code.as_str(), "cursor-gap" | "page-cursor-expired") =>
                {
                    break;
                }
                Err(error) if client_error_retryable(error.status, Some(&error.code)) => {
                    tokio::time::sleep(COLLECTION_REREAD_INTERVAL).await;
                    break;
                }
                Err(error) => {
                    let _ = outbox.send((id.clone(), failed(&error)));
                    return;
                }
            }
            if outbox.is_closed() {
                return;
            }
        }
    }
}

fn frame_bytes(frame: &Value) -> usize {
    serde_json::to_vec(frame).map_or(usize::MAX, |bytes| bytes.len())
}

/// The conversation followers a socket holds; they stop when it closes.
#[derive(Default)]
struct ConversationFollowers(BTreeMap<String, tokio::task::AbortHandle>);

impl ConversationFollowers {
    fn stop(&mut self, id: &str) {
        if let Some(follower) = self.0.remove(id) {
            follower.abort();
        }
    }
}

impl Drop for ConversationFollowers {
    fn drop(&mut self) {
        for follower in self.0.values() {
            follower.abort();
        }
    }
}

async fn collection_stream_socket(socket: WebSocket, state: AppState, session: ClientSession) {
    collection_stream_socket_with_reader(
        socket,
        state,
        session,
        |state, session, request| async move { collection_items(&state, &session, &request).await },
    )
    .await;
}

async fn collection_stream_socket_with_reader<F, Fut>(
    mut socket: WebSocket,
    state: AppState,
    session: ClientSession,
    read: F,
) where
    F: Fn(AppState, ClientSession, CollectionSubscribe) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<(ClientSnapshot, Vec<Value>, bool), ApiError>> + Send,
{
    // Subscribe before the first snapshot, so a commit while building it wakes
    // the next loop and is reflected in a following change frame.
    let mut changed = state.event_notify.subscribe();
    let mut subscriptions = BTreeMap::<String, CollectionSubscription>::new();
    let mut terminals = BTreeMap::<String, watch::Receiver<TerminalFrame>>::new();
    let mut conversations = ConversationFollowers::default();
    let (conversation_outbox, mut conversation_frames) =
        tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    // The commits already weighed for a reread, whether one is due, and when the last ran.
    let mut attention_clock = tokio::time::interval(ATTENTION_CLOCK_INTERVAL);
    attention_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut weighed = state.store.index().unwrap_or_default();
    let mut reread_due = false;
    let mut last_reread = tokio::time::Instant::now() - COLLECTION_REREAD_INTERVAL;
    loop {
        // The subscriptions to read after this wake-up.
        let mut refresh = Vec::<String>::new();
        // A command already waiting goes first: under steady commits a commit wake is almost
        // always ready too, and a fair pick could keep rereading the held windows while a new
        // subscription waits. Otherwise every source gets a fair pick.
        let waiting = futures_util::FutureExt::now_or_never(socket.recv());
        let command_waiting = waiting.is_some();
        tokio::select! {
            incoming = async { match waiting { Some(incoming) => incoming, None => socket.recv().await } } => {
                // Take every command already waiting, so subscriptions sent together are read
                // together below.
                let mut next = Some(incoming);
                while let Some(incoming) = next.take() {
                    'command: {
                        let Some(Ok(message)) = incoming else { return; };
                        let WsMessage::Text(payload) = message else {
                            if matches!(message, WsMessage::Close(_)) { return; }
                            break 'command;
                        };
                        let Ok(request) = serde_json::from_str::<CollectionSubscribe>(&payload) else {
                            if !send_collection(&mut socket, json!({"kind":"error", "message":"invalid collection command"})).await { return; }
                            break 'command;
                        };
                        if request.kind == "unsubscribe" {
                            subscriptions.remove(&request.id);
                            terminals.remove(&request.id);
                            conversations.stop(&request.id);
                            break 'command;
                        }
                        let held = subscriptions.contains_key(&request.id) || terminals.contains_key(&request.id) || conversations.0.contains_key(&request.id);
                        if request.kind != "subscribe" || request.id.is_empty() || request.id.len() > 128 || subscriptions.len() + terminals.len() + conversations.0.len() >= COLLECTION_MAX_SUBSCRIPTIONS && !held {
                            if !send_collection(&mut socket, json!({"kind":"error", "id":request.id, "message":"invalid subscription or subscription limit exceeded"})).await { return; }
                            break 'command;
                        }
                        // A subscription with a held ID replaces it.
                        subscriptions.remove(&request.id);
                        terminals.remove(&request.id);
                        conversations.stop(&request.id);
                        if request.collection == "conversation" {
                            let target = request.conversation.as_deref().unwrap_or_default();
                            let opened = conversation_session_id(&state, target).and_then(|session_id| {
                                let remote = conversation_owner_host(&state, &session, &session_id)?;
                                Ok((session_id, remote))
                            });
                            match opened {
                                Ok((session_id, remote)) => {
                                    let follower = tokio::spawn(follow_conversation(state.clone(), session.clone(), request.id.clone(), session_id, remote, conversation_outbox.clone()));
                                    conversations.0.insert(request.id.clone(), follower.abort_handle());
                                }
                                Err(error) => {
                                    if !send_collection(&mut socket, json!({"kind":"error", "id":request.id, "collection":"conversation", "code":error.code, "message":error.message})).await { return; }
                                }
                            }
                            break 'command;
                        }
                        if request.collection == "terminal" {
                            match open_terminal_subscription(&state, &session, &request).await {
                                Ok(receiver) => {
                                    terminals.insert(request.id.clone(), receiver);
                                }
                                Err(error) => {
                                    if !send_collection(&mut socket, json!({"kind":"error", "id":request.id, "collection":"terminal", "code":error.code, "message":error.message})).await { return; }
                                }
                            }
                            break 'command;
                        }
                        refresh.push(request.id.clone());
                        subscriptions.insert(request.id.clone(), CollectionSubscription { request, delivered: false, previous: BTreeMap::new(), order: Vec::new(), has_more: false });

                    }
                    next = futures_util::FutureExt::now_or_never(socket.recv());
                }
            }
            result = changed.changed(), if !command_waiting => {
                if result.is_err() { return; }
                // Weigh only the commits since the last look: a reread is due when one of them
                // can change a held window.
                let index = state.store.index().unwrap_or(weighed);
                if index > weighed {
                    let claims = state.store.claims_page(None, None, weighed, index.checked_add(1), false, 10_000).map(|page| page.claims).unwrap_or_default();
                    let glasses_changed = subscriptions.values().any(|s| s.request.collection == "glasses") && state.store.glasses_changed(weighed, index).unwrap_or(true);
                    reread_due |= glasses_changed || claims.len() >= 10_000 || subscriptions.values().any(|subscription| {
                        claims.iter().any(|claim| !collection_ignores(&subscription.request.collection, &claim.kind))
                    });
                    weighed = index;
                }
                if !reread_due || last_reread.elapsed() < COLLECTION_REREAD_INTERVAL { continue; }
                refresh.extend(subscriptions.keys().cloned());
            }
            () = tokio::time::sleep_until(last_reread + COLLECTION_REREAD_INTERVAL), if !command_waiting && reread_due => {
                refresh.extend(subscriptions.keys().cloned());
            }
            _ = attention_clock.tick(), if !command_waiting && subscriptions.values().any(|s| s.request.collection == "attention") => {
                refresh.extend(subscriptions.iter().filter(|(_, s)| s.request.collection == "attention").map(|(id, _)| id.clone()));
            }
            Some((id, frame)) = conversation_frames.recv(), if !command_waiting => {
                // A follower stopped by unsubscribe may still have had a frame on the way.
                if !conversations.0.contains_key(&id) { continue; }
                if frame["kind"] == "error" { conversations.0.remove(&id); }
                if !send_collection(&mut socket, frame).await { return; }
                continue;
            }
            (id, frame) = next_terminal_frame(&mut terminals), if !command_waiting && !terminals.is_empty() => {
                let message = match frame {
                    Some(TerminalFrame::Waiting) => continue,
                    Some(TerminalFrame::Screen(envelope)) => json!({"kind":"screen", "id":id, "collection":"terminal", "snapshot":envelope["snapshot"], "value":envelope["value"]}),
                    Some(TerminalFrame::Ended(error)) => {
                        terminals.remove(&id);
                        json!({"kind":"error", "id":id, "collection":"terminal", "code":error["code"], "message":error["message"], "retryable":error["retryable"]})
                    }
                    None => {
                        terminals.remove(&id);
                        json!({"kind":"error", "id":id, "collection":"terminal", "code":"internal", "message":"the terminal stream stopped"})
                    }
                };
                if !send_collection(&mut socket, message).await { return; }
                continue;
            }
        }
        if refresh.is_empty() {
            continue;
        }
        if refresh.len() >= subscriptions.len() && !subscriptions.is_empty() {
            reread_due = false;
            last_reread = tokio::time::Instant::now();
        }
        // Read every due window at once, each in its own snapshot, then send them in order:
        // one slow window never holds back the others' reads.
        let reads = futures_util::future::join_all(refresh.into_iter().filter_map(|id| {
            let request = subscriptions.get(&id)?.request.clone();
            let (state, session, read) = (state.clone(), session.clone(), read.clone());
            Some(async move { (id, read(state, session, request).await) })
        }))
        .await;
        for (id, read) in reads {
            let Some(subscription) = subscriptions.get_mut(&id) else {
                continue;
            };
            match deliver_collection(&mut socket, subscription, read).await {
                Refreshed::Current => {}
                Refreshed::Retry => {
                    reread_due = true;
                }
                Refreshed::Dropped => {
                    subscriptions.remove(&id);
                }
                Refreshed::Closed => return,
            }
        }
    }
}

#[derive(Deserialize)]
pub(super) struct AgentDeclarationQuery {
    revision: Option<String>,
    #[serde(default)]
    show_env_values: bool,
}

/// Both redacted and explicit environment-value reads require declaration scope.
pub(super) async fn agent_declaration(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<AgentDeclarationQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.declarations")?;
    let subject = format!("agent/{}", id.trim_start_matches("agent/"));
    let lookup_subject = subject.clone();
    let store = state.store.clone();
    let revision = query.revision;
    let (declaration, revisions) = blocking_store(move || {
        let declaration = store.agent_declaration(&lookup_subject, revision.as_deref())?;
        let revisions = store.agent_declaration_revisions(&lookup_subject)?;
        Ok((declaration, revisions))
    })
    .await?;
    let Some((revision, mut tree)) = declaration else {
        return Err(ApiError::not_found("managed agent declaration not found"));
    };
    if !query.show_env_values {
        crate::graph::redact_agent_env_values(&mut tree);
    }
    let kdl = crate::graph::render_agent_desired_kdl(&tree).map_err(ApiError::bad)?;
    Ok(Json(json!({
        "id": subject,
        "revision": revision,
        "tree": tree,
        "kdl": kdl,
        "revisions": revisions,
    })))
}

#[derive(Deserialize)]
pub(super) struct ClientDocumentQuery {
    name: String,
}

pub(super) async fn document_get(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientDocumentQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let (name, hash) = query.name.rsplit_once('@').ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "invalid-document-reference",
            "a document name needs `@HASH`",
        ))
    })?;
    if name.is_empty() || hash.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-document-reference",
            "a document name needs `@HASH`",
        )));
    }
    let store = state.store.clone();
    let name = name.to_owned();
    let hash = hash.to_owned();
    let bytes = blocking_store(move || store.get_document(&name, &hash))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("document `{}` is not stored", query.name)))?;
    Ok(Json(json!({ "reference": query.name, "bytes": bytes })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientUsageQuery {
    since_ms: Option<u64>,
    until_ms: Option<u64>,
}

/// Token spend and its API-equivalent cost over a period (the last 24 hours unless asked), one
/// row per agent, mission run, step, model, account and host, largest first. An identity st
/// does not know (a standing seat has no mission run or step) is left out of its row.
pub(super) async fn usage_period(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientUsageQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let until_ms = query.until_ms.unwrap_or(client_now_ms() as u64);
    let since_ms = query
        .since_ms
        .unwrap_or(until_ms.saturating_sub(86_400_000));
    if since_ms > until_ms {
        return Err(ApiError::bad(St3Error::new(
            "invalid-usage-period",
            "usage start must be before its end",
        )));
    }
    let store = state.store.clone();
    let (mut rows, limits) = blocking_store(move || {
        Ok((
            store.usage_period_rows(since_ms, until_ms)?,
            store.account_limits()?,
        ))
    })
    .await?;
    for row in &mut rows {
        if let Some(fields) = row.as_object_mut() {
            fields.retain(|_, value| value.as_str() != Some(""));
        }
    }
    // Each account's freshest limits reading; what a harness did not report is left out.
    let limits = limits
        .into_iter()
        .map(|limit| {
            let mut value = serde_json::to_value(limit).unwrap_or_default();
            if let Some(fields) = value.as_object_mut() {
                fields.retain(|_, value| !value.is_null());
            }
            value
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({ "since_ms": since_ms, "until_ms": until_ms, "rows": rows, "limits": limits }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubjectDefinitionQuery {
    subject: String,
    #[serde(default)]
    show_env_values: bool,
}

/// One agent's applied definition, reconstructed from its selected desired claim, not authored
/// source. Missions are published as compiled revisions and keep no canonical declaration AST.
/// Environment values are redacted unless the caller asks for them and holds declaration scope.
pub(super) async fn subject_definition(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<SubjectDefinitionQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    require_scope(&session, "read.projections")?;
    if query.show_env_values {
        require_scope(&session, "read.declarations")?;
    }
    if !query.subject.starts_with("agent/") {
        return Err(ApiError::bad(St3Error::new(
            "validation-failed",
            "a definition subject must start with `agent/`; missions are published revisions \
             without a canonical declaration",
        )));
    }
    let subject = query.subject.clone();
    let show_env_values = query.show_env_values;
    let result = blocking_store(move || {
        let store = state.store.clone();
        store.read_snapshot(|index| {
            let status = store.status_at(Some(&subject), None, Some(index))?;
            let Some(status) = status.subjects.into_iter().find(|item| item.subject == subject)
            else {
                return Ok(None);
            };
            let Some(mut desired) = status.desired else {
                return Ok(None);
            };
            if !show_env_values {
                crate::graph::redact_agent_env_values(&mut desired);
            }
            let kdl = crate::graph::render_agent_desired_kdl(&desired)?;
            let revision = status.desired_revision
                .ok_or_else(|| anyhow::anyhow!("an applied definition has no desired revision"))?;
            let token = status.desired_token
                .ok_or_else(|| anyhow::anyhow!("an applied definition has no desired token"))?;
            let value = json!({
                "kind": "subject-definition",
                "subject": subject,
                "desired": desired,
                "kdl": kdl,
                "desired_revision": revision,
                "desired_token": token,
                "conflicts": status.conflicts,
            });
            Ok(Some((client_snapshot_at(&state, index), value)))
        })
    }).await?;
    let (snapshot, value) = result.ok_or_else(|| ApiError::not_found(
        format!("subject `{}` has no applied definition", query.subject),
    ))?;
    // Reserve space for the snapshot and response envelope. Definitions are never truncated.
    if serde_json::to_vec(&value).map_err(ApiError::internal)?.len()
        > CLIENT_MAX_RESPONSE_BYTES - 4096
    {
        return Err(ApiError::bad(St3Error::new(
            "validation-failed",
            "the applied definition exceeds the client response limit",
        )));
    }
    Ok((Extension(snapshot), Json(value)))
}

const ALL_SCOPES: &[&str] = &[
    "read.projections",
    "read.declarations",
    "read.glasses",
    "control.glasses",
    "terminal.read",
    "terminal.control",
    "control.attention",
    "control.messages",
    "control.launches",
    "control.missions",
    "control.work",
    "control.runtimes",
    "control.pairing",
];
const LIMITED_PAIRING_SCOPES: &[&str] = &[
    "read.projections",
    "read.glasses",
    "control.glasses",
    "terminal.read",
    "control.attention",
    "control.launches",
];
const ACTIONS: &[&str] = &[
    "attention.resolve",
    "review.approve",
    "review.reject",
    "review.request-changes",
    "message.send",
    "message.read",
    "message.close",
    "launch.create",
    "launch.revise",
    "launch.preview",
    "launch.approve",
    "launch.cancel",
    "mission.start",
    "mission.revise",
    "mission.approve-revision",
    "mission.cancel-revision",
    "mission.cancel",
    "session.import",
    "work.ask",
    "work.done",
    "work.cancel-ask",
    "work.claim",
    "work.renew",
    "work.progress",
    "work.complete",
    "work.fail",
    "work.release",
    "work.retry",
    "work.publish-mission",
    "agent.create",
    "agent.stop",
    "agent.start",
    "agent.suspend",
    "agent.resume",
    "terminal.create",
    "terminal.end",
    "agent.queue-move",
    "lane.join",
    "lane.leave",
    "lane.move",
    "lane.mark",
    "lane.approve",
    "runtime.stop",
    "runtime.restart",
    "runtime.reset",
    "runtime.context-clear",
    "runtime.signal",
    "terminal.input",
    "terminal.resize",
    "terminal.attach",
    "terminal.detach",
    "pairing.revoke",
];
const AVAILABLE_ACTIONS: &[&str] = &[
    "review.approve",
    "review.reject",
    "review.request-changes",
    "message.send",
    "message.read",
    "message.close",
    "launch.create",
    "launch.revise",
    "launch.preview",
    "launch.approve",
    "launch.cancel",
    "mission.start",
    "mission.approve-revision",
    "mission.cancel-revision",
    "mission.cancel",
    "session.import",
    "work.ask",
    "work.done",
    "work.cancel-ask",
    "work.claim",
    "work.renew",
    "work.progress",
    "work.complete",
    "work.fail",
    "work.release",
    "work.retry",
    "agent.create",
    "agent.stop",
    "agent.start",
    "agent.suspend",
    "agent.resume",
    "terminal.create",
    "terminal.end",
    "agent.queue-move",
    "lane.join",
    "lane.leave",
    "lane.move",
    "lane.mark",
    "lane.approve",
    "runtime.stop",
    "runtime.restart",
    "runtime.reset",
    "runtime.context-clear",
    "runtime.signal",
    "terminal.input",
    "terminal.resize",
    "terminal.attach",
    "terminal.detach",
    "pairing.revoke",
];

#[derive(Clone, Debug)]
pub(super) struct ClientSession {
    /// The credential/session identity used for audit and idempotency isolation.
    pub(super) actor: String,
    /// The concrete graph person whose explicitly delegated authority is exercised.
    pub(super) authority_actor: String,
    pub(super) transport: &'static str,
    scopes: std::collections::BTreeSet<String>,
}

impl ClientSession {
    fn local(person: Option<&str>) -> Result<Self, ApiError> {
        if person.is_some_and(|person| {
            !(person.starts_with("person/") && person.matches('/').count() == 1
                || person.starts_with("agent/"))
        }) {
            return Err(forbidden(
                "the trusted Unix client must identify one concrete person or agent",
            ));
        }
        let Some(person) = person else {
            return Ok(Self {
                actor: "client/local/read-only".into(),
                authority_actor: "client/local/read-only".into(),
                transport: "unix",
                scopes: ["read.projections", "terminal.read"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            });
        };
        // Free mode: an agent's local session holds every scope a person's does. Its actions
        // still record the agent as their actor.
        Ok(Self {
            actor: person.into(),
            authority_actor: person.into(),
            transport: "unix",
            scopes: ALL_SCOPES.iter().map(|scope| (*scope).to_owned()).collect(),
        })
    }

    fn pairing() -> Self {
        Self {
            actor: "client/pairing/completion".into(),
            authority_actor: "client/pairing/completion".into(),
            transport: "fabric-loopback",
            scopes: std::collections::BTreeSet::new(),
        }
    }

    fn allows(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

/// Whether the session acts for a concrete person, or for a local agent seat. Within a fleet, an
/// agent may do whatever the person who runs the fleet may do (free mode).
pub(super) fn acting_party(session: &ClientSession) -> bool {
    let actor = session.authority_actor.as_str();
    actor.starts_with("person/") && actor.matches('/').count() == 1
        || session.transport == "unix" && actor.starts_with("agent/")
}

fn session_claim_actor(session: &ClientSession) -> String {
    if session.authority_actor.starts_with("person/")
        || session.authority_actor.starts_with("agent/")
    {
        session.authority_actor.clone()
    } else {
        "requester".into()
    }
}

pub(super) fn capabilities(session: &ClientSession) -> Vec<Value> {
    let mut capabilities = ALL_SCOPES
        .iter()
        .map(|scope| {
            json!({
                "id": scope,
                "version": 0,
                "state": if session.allows(scope) { "granted" } else { "ungranted" }
            })
        })
        .collect::<Vec<_>>();
    capabilities.push(json!({"id":"owned-sets", "version":1, "state":if session.allows("read.projections") {"granted"} else {"ungranted"}}));
    capabilities.push(json!({"id":"glasses", "version":2, "state":if glass_person(session, false).is_ok() && glass_person(session, true).is_ok() { "granted" } else { "ungranted" }}));
    capabilities.extend(ACTIONS.iter().map(|action| {
        let scope = action_scope(action).expect("registered client action has a scope");
        let state = if !AVAILABLE_ACTIONS.contains(action) {
            "unavailable"
        } else if session.allows(scope)
            && (!matches!(
                *action,
                "agent.create" | "agent.stop" | "agent.start" | "terminal.create" | "terminal.end"
            ) || require_creation_actor(session).is_ok())
        {
            "granted"
        } else {
            "ungranted"
        };
        json!({ "id": action, "version": 0, "state": state })
    }));
    capabilities
}

fn forbidden(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden".into(),
        message: message.into(),
        details: Box::default(),
    }
}

pub(super) fn fabric_boundary_forbidden() -> ApiError {
    forbidden("the client gateway exposes only the authenticated client-v0 boundary")
}

fn validation(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "validation-failed".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn stale(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        code: "stale-fence".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn credential_digest(credential: &str) -> String {
    hex::encode(Sha256::digest(credential.as_bytes()))
}

pub(super) fn authenticate(
    state: &AppState,
    request: &Request<Body>,
    transport: &'static str,
) -> Result<ClientSession, ApiError> {
    let Some(value) = request.headers().get(AUTHORIZATION) else {
        if transport == "unix" {
            let person = request
                .headers()
                .get(LOCAL_PERSON_HEADER)
                .and_then(|value| value.to_str().ok());
            return ClientSession::local(person);
        }
        let pairing_completion = request.method() == axum::http::Method::POST
            && request.uri().path().starts_with("/v1/client/pairings/")
            && request.uri().path().ends_with("/complete");
        return pairing_completion
            .then(ClientSession::pairing)
            .ok_or_else(|| forbidden("the Fabric-loopback client credential is required"));
    };
    let value = value
        .to_str()
        .map_err(|_| forbidden("the client authorization header is malformed"))?;
    let credential = value
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| forbidden("the client authorization scheme must be Bearer"))?;
    let digest = credential_digest(credential);
    let pairings = state
        .store
        .claims_for_kind_at("custom.client.pairing-completed", None, true, 10_000)
        .map_err(ApiError::internal)?;
    let paired = pairings.claims.iter().find(|claim| {
        claim
            .body
            .pointer("/fields/credential_hash")
            .and_then(Value::as_str)
            == Some(digest.as_str())
    });
    let Some(paired) = paired else {
        return Err(forbidden("the client credential is unknown or expired"));
    };
    let revoked = state
        .store
        .claims_for_subject_kind_at(
            &paired.subject,
            "custom.client.pairing-revoked",
            None,
            true,
            1,
        )
        .map_err(ApiError::internal)?
        .claims
        .first()
        .is_some_and(|claim| claim.store_index > paired.store_index);
    let expires_at = paired
        .body
        .pointer("/fields/expires_at_unix_ms")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or_default();
    if revoked || expires_at <= client_now_ms() {
        return Err(forbidden("the client credential was revoked or expired"));
    }
    let actor = paired
        .body
        .pointer("/fields/session_actor")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("a paired client has no derived actor"))?;
    let authority_actor = paired
        .body
        .pointer("/fields/person_id")
        .and_then(Value::as_str)
        .filter(|actor| actor.starts_with("person/") && actor.matches('/').count() == 1)
        .ok_or_else(|| ApiError::internal("a paired client has no concrete delegated person"))?;
    let scopes = paired
        .body
        .pointer("/fields/scopes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let session = ClientSession {
        actor: actor.into(),
        authority_actor: authority_actor.into(),
        transport,
        scopes,
    };
    if request.method() == axum::http::Method::GET {
        let scope = if request
            .uri()
            .path()
            .starts_with("/v1/client/agent-declarations/")
        {
            "read.declarations"
        } else if request.uri().path().starts_with("/v1/client/terminals/") {
            "terminal.read"
        } else {
            "read.projections"
        };
        require_scope(&session, scope)?;
    }
    Ok(session)
}

pub(super) fn require_scope(session: &ClientSession, scope: &str) -> Result<(), ApiError> {
    if session.allows(scope) {
        Ok(())
    } else {
        Err(forbidden(format!(
            "the authenticated client session is not granted `{scope}`"
        )))
    }
}

pub(super) fn person_filter(
    session: &ClientSession,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if session.authority_actor.starts_with("person/") {
        if requested.is_some_and(|person| person != session.authority_actor) {
            return Err(forbidden(
                "the authenticated client cannot read another person's private projection",
            ));
        }
        return Ok(Some(session.authority_actor.clone()));
    }
    Ok(requested.map(str::to_owned))
}

fn mission_visualization(
    store: &Store,
    mission_id: &str,
    revision: &str,
    state: &str,
) -> anyhow::Result<Option<Value>> {
    let Some(mission) =
        store.mission_spec(mission_id.trim_start_matches("mission/"), Some(revision))?
    else {
        return Ok(None);
    };
    let nodes = mission
        .display_order
        .iter()
        .filter_map(|id| mission.steps.get(id))
        .map(|step| json!({
            "id": format!("step/{}", step.path), "kind": "step",
            "label": step.title.as_deref().unwrap_or(&step.id), "path": step.path,
            "goals": step.goals, "constraints": step.constraints,
            "assignment": step.work_selector, "timeout_ms": step.timeout_ms,
            "retry": step.retry, "gates": step.gates, "loop": step.loop_spec,
            "resources": step.documents, "source_references": step.documents,
            "runtime": { "state": state, "attempt": null, "progress": null, "blockers": [], "attention": [], "errors": [] }
        }))
        .collect::<Vec<_>>();
    let edges = mission.steps.values().flat_map(|step| step.dependencies.iter().map(move |dependency| match dependency {
        crate::model::DependencySpec::Step { step: source, .. } => json!({"id": format!("edge/{source}/{}", step.path), "kind":"dependency", "from":format!("step/{source}"), "to":format!("step/{}", step.path), "gate":dependency}),
        crate::model::DependencySpec::Predicate { .. } => json!({"id":format!("edge/predicate/{}", step.path), "kind":"gate", "from":null, "to":format!("step/{}", step.path), "gate":dependency}),
    })).collect::<Vec<_>>();
    let timeline = mission.display_order.iter().enumerate().filter_map(|(ordinal, id)| mission.steps.get(id).map(|step| json!({"id":format!("timeline/{}", step.path), "node":format!("step/{}", step.path), "ordinal":ordinal, "dependencies":step.dependencies, "timeout_ms":step.timeout_ms}))).collect::<Vec<_>>();
    let mut decisions = Vec::new();
    for session in store.planning_sessions_for_mission(mission_id)? {
        decisions.extend(super::client_launch_decision_resources(store, &session)?);
    }
    Ok(Some(json!({
        "version":"st3.visualization.v0", "views":["graph","timeline","swimlane","revision","risk","live-progress"],
        "mission":mission_id, "nodes":nodes, "edges":edges, "groups":[],
        "timeline":{"entries":timeline}, "swimlanes":[], "goals":mission.goals,
        "constraints":mission.constraints, "gates":mission.gates, "resources":mission.products,
        "decisions":decisions, "revision":{"current":revision}, "diffs":[],
        "risk":{"blockers":[],"warnings":[]}, "live_progress":{"state":state}
    })))
}

pub(super) fn mission_resources(
    store: &Store,
    snapshot_index: u64,
    history: bool,
    selected_id: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    mission_resources_filtered(store, snapshot_index, history, selected_id, None)
}

/// Collection cards keep only three run headers, regardless of a mission's history size.
/// Full run and step detail stays on the detail endpoint.
fn mission_list_cards(store: &Store, ids: &[String]) -> anyhow::Result<Vec<Value>> {
    let attention = store.human_attention_runs()?;
    let definitions = store
        .mission_definitions_for_ids(ids)?
        .into_iter()
        .map(|d| (d.mission.subject.clone(), d))
        .collect::<BTreeMap<_, _>>();
    ids.iter().map(|id| {
        let overview = store.mission_overview(id, 3)?;
        let newest = overview["newest"].as_array().expect("overview previews");
        let latest = newest.first();
        let definition = definitions.get(id);
        let state = if overview["counts"]["running"].as_u64().unwrap_or(0)>0 {"running"}
            else if overview["counts"]["standing"].as_u64().unwrap_or(0)>0 {"standing"}
            else if definition.is_some_and(|d| d.mission.state==crate::model::MissionState::Retired) {"retired"}
            else if let Some(run)=latest {run["status"].as_str().unwrap_or("ready")}
            else {match definition.map(|d| &d.mission.state) {
                Some(crate::model::MissionState::Draft)=>"draft",
                Some(crate::model::MissionState::Retired)=>"retired", _=>"ready"}};
        let updated = latest.and_then(|r| r["updated_at_unix_ms"].as_u64()).map(u128::from)
            .or_else(|| definition.map(|d| d.updated_at_unix_ms)).unwrap_or(0);
        let revision = latest.and_then(|r| r["revision"].as_str())
            .or_else(|| definition.map(|d| d.mission.revision.as_str())).unwrap_or("unknown");
        let active = overview["counts"].as_object().unwrap().iter()
            .filter(|(state,_)| !matches!(state.as_str(),"completed"|"failed"|"cancelled"))
            .map(|(_,count)| count.as_u64().unwrap_or(0)).sum::<u64>();
        let details = newest.iter().rev().map(|run| {
            let run_id=run["id"].as_str().expect("run header id");
            let (total,done,steps)=store.mission_step_preview(run_id)?;
            let terminal=matches!(run["status"].as_str(),Some("completed"|"failed"|"cancelled"));
            let must_act=if terminal {"nobody"} else if attention.contains(run_id) {"you"}
                else if steps.iter().any(|s| matches!(s.status.as_str(),"ready"|"claimed"|"working")
                    && (s.assigned_to.is_some() || s.claimant.is_some() || !s.available_to.is_empty())) {"agent"}
                else if steps.iter().any(|s| s.status=="blocked") {"blocked"} else {"system"};
            let shown=steps.iter().map(|step| json!({
                "id":step.subject,"path":step.step,"title":step.title,"state":client_work_state(&step.status),
                "attempt":step.attempt,"assignee":step.assigned_to,"claimant":step.claimant,
                "agentless":step.agentless,"since":client_timestamp(step.updated_at_unix_ms),
                "blocked_reason":step.blocked_reason,"blockers":step.blockers,
                "goals":step.goals,"constraints":step.constraints
            })).collect::<Vec<_>>();
            let current=shown.iter().filter(|s| matches!(s["state"].as_str(),Some("ready"|"claimed"|"verifying"|"blocked")))
                .map(|s| json!({"id":s["id"],"title":s["title"],"assignee":s["assignee"],"claimant":s["claimant"],"state":s["state"],"since":s["since"]})).collect::<Vec<_>>();
            Ok::<Value,anyhow::Error>(json!({
                "id":run["id"],"generation_id":run["generation_id"],"requester":run["requester"],
                "status":run["status"],"phase":run["phase"],"progress":{"done":done,"total":total},
                "current_steps":current,"must_act":must_act,
                "state_since":client_timestamp(run["updated_at_unix_ms"].as_u64().unwrap_or(0) as u128),
                "steps":shown
            }))
        }).collect::<anyhow::Result<Vec<_>>>()?;
        let must_act=["you","agent","blocked","system"].into_iter()
            .find(|kind| details.iter().any(|run| run["must_act"]==*kind)).unwrap_or(if active>0 {"system"} else {"nobody"});
        let generations=newest.iter().map(|r| (r["id"].as_str().unwrap().to_owned(),r["generation_id"].clone()))
            .collect::<serde_json::Map<_,_>>();
        let historical = matches!(state,"completed"|"failed"|"cancelled"|"retired");
        Ok(json!({"id":id,"kind":"mission","revision":revision,"updated_at":client_timestamp(updated),
            "title":id.trim_start_matches("mission/"),"state":state,"mission_revision":revision,
            "runs":newest.iter().rev().map(|r| r["id"].clone()).collect::<Vec<_>>(),
            "run_details":details,"active_runs":active,"total_runs":overview["total_runs"],
            "run_counts":overview["counts"],"runs_truncated":overview["total_runs"].as_u64().unwrap_or(0)>newest.len() as u64,
            "run_generations":generations,"must_act":must_act,
            "operational":{"layer":if historical {"history"} else {"current"},"actionable":!historical,"reasons":[]}}))
    }).collect()
}

fn bound_mission_cards(items: &mut Vec<Value>) -> anyhow::Result<bool> {
    // Reserve room for the envelope, continuation cursor and fleet sync notice.
    let budget = CLIENT_MAX_RESPONSE_BYTES.saturating_sub(128_000);
    let mut used = 0;
    let mut keep = 0;
    for item in items.iter_mut() {
        let mut bytes = serde_json::to_vec(item)?.len() + 1;
        if keep == 0 && bytes > budget {
            item["runs"] = json!([]);
            item["run_details"] = json!([]);
            item["runs_truncated"] = json!(true);
            bytes = serde_json::to_vec(item)?.len() + 1;
        }
        if used + bytes > budget {
            break;
        }
        used += bytes;
        keep += 1;
    }
    let truncated = keep < items.len();
    items.truncate(keep);
    anyhow::ensure!(
        !truncated || keep > 0,
        "a mission identifier exceeds the response budget"
    );
    Ok(truncated)
}

fn mission_resources_filtered(
    store: &Store,
    snapshot_index: u64,
    history: bool,
    selected_id: Option<&str>,
    page_ids: Option<&[String]>,
) -> anyhow::Result<Vec<Value>> {
    let human_attention_runs = store.human_attention_runs()?;
    let mut missions = BTreeMap::<String, Vec<MissionRunView>>::new();
    // A detail or a page reads only its own missions' runs, definitions, seats and states.
    let scope = selected_id
        .map(|selected| vec![selected.to_owned()])
        .or_else(|| page_ids.map(|ids| ids.to_vec()));
    // A detail reads its runs' current steps with their headers, two reads for every run.
    let runs = if let Some(selected) = selected_id {
        store.mission_run_summaries_for_missions(&[selected.to_owned()])?
    } else if let Some(ids) = page_ids {
        store.mission_run_summaries_for_missions(ids)?
    } else {
        store.mission_run_summaries()?
    };
    for run in runs {
        missions.entry(run.mission.clone()).or_default().push(run);
    }
    let definitions = if let Some(ids) = &scope {
        store.mission_definitions_for_ids(ids)?
    } else {
        store.mission_definitions()?
    }
    .into_iter()
    .map(|definition| {
        (
            definition.mission.subject.clone(),
            (definition.mission, definition.updated_at_unix_ms),
        )
    })
    .collect::<BTreeMap<_, _>>();
    for mission in definitions.keys() {
        missions.entry(mission.clone()).or_default();
    }
    let page_runs = missions
        .values()
        .flatten()
        .map(|run| run.subject.as_str())
        .collect::<BTreeSet<_>>();
    let page_run_subjects = page_runs
        .iter()
        .map(|run| (*run).to_owned())
        .collect::<Vec<_>>();
    let desired = if scope.is_some() {
        store.desired_subjects_for_owner_runs(&page_run_subjects)?
    } else {
        store.desired_subjects()?
    };
    let usage_subjects = desired
        .iter()
        .filter(|seat| {
            seat.owner_run
                .as_deref()
                .is_some_and(|run| page_runs.contains(run))
        })
        .map(|seat| seat.subject.clone())
        .collect::<Vec<_>>();
    let usage_summaries = store.usage_summaries_at(&usage_subjects, Some(snapshot_index))?;
    let mut usage_by_run = BTreeMap::<&str, Vec<&crate::model::UsageSummary>>::new();
    for seat in &desired {
        if let (Some(run), Some(usage)) = (
            seat.owner_run.as_deref(),
            usage_summaries.get(&seat.subject),
        ) {
            usage_by_run.entry(run).or_default().push(usage);
        }
    }
    let run_states = if scope.is_some() {
        store.mission_run_states_for_runs(&page_run_subjects)?
    } else {
        store.mission_run_states()?
    };
    let mut values = missions
        .into_iter()
        .filter(|(mission, _)| selected_id.is_none_or(|selected| mission == selected))
        // st3 publishes each loop round as an internal definition that no one starts directly;
        // its runs belong to the parent mission. List them only with history or by ID.
        .filter(|(mission, _)| {
            history || selected_id.is_some() || !mission.starts_with("mission/__st3/")
        })
        .map(|(mission, mut runs)| {
            runs.sort_by_key(|run| run.created_at_unix_ms);
            let definition = definitions.get(&mission);
            let latest = runs.last();
            let state = if runs.is_empty() {
                match definition.map(|(definition, _)| &definition.state) {
                    Some(crate::model::MissionState::Draft) => "draft",
                    Some(crate::model::MissionState::Ready) => "ready",
                    Some(crate::model::MissionState::Retired) => "retired",
                    None => "ready",
                }
            } else if runs.iter().any(|run| run.status == "running") {
                "running"
            } else if runs.iter().any(|run| run.status == "standing") {
                "standing"
            } else if definition.is_some_and(|(definition, _)| {
                definition.state == crate::model::MissionState::Retired
            }) {
                "retired"
            } else {
                match latest
                    .expect("a nonempty run list has a latest run")
                    .status
                    .as_str()
                {
                    "completed" => "completed",
                    "failed" => "failed",
                    "cancelled" => "cancelled",
                    _ => "ready",
                }
            };
            let historical = matches!(state, "completed" | "failed" | "cancelled" | "retired");
            // A run that failed or was cancelled stays in the current view for a while.
            let recently_ended = matches!(state, "failed" | "cancelled")
                && latest.is_some_and(|run| {
                    run.updated_at_unix_ms >= crate::store::recently_ended_since()
                });
            if !history && historical && !recently_ended {
                return Ok(None);
            }
            let run_generations = runs
                .iter()
                .map(|run| (run.subject.clone(), Value::String(run.generation.clone())))
                .collect::<serde_json::Map<_, _>>();
            let run_ids = runs
                .iter()
                .map(|run| run.subject.as_str())
                .collect::<BTreeSet<_>>();
            let active_runs = runs
                .iter()
                .filter(|run| !matches!(run.status.as_str(), "completed" | "failed" | "cancelled"))
                .count();
            let run_details = runs
                .iter()
                .map(|header| {
                    // A detail shows each step's effective state and latest progress, and none of
                    // the timing and wake history a work view reads for every step.
                    let run = if selected_id.is_some() {
                        store.with_step_states(header.clone(), true)?
                    } else {
                        header.clone()
                    };
                    let done = run
                        .steps
                        .iter()
                        .filter(|step| step.status == "completed")
                        .count();
                    let current_steps = run
                        .steps
                        .iter()
                        .filter(|step| {
                            matches!(
                                step.status.as_str(),
                                "ready" | "claimed" | "working" | "verifying" | "blocked"
                            )
                        })
                        .map(|step| {
                            json!({
                                "id": step.subject,
                                "title": step.title,
                                "assignee": step.assigned_to,
                                "claimant": step.claimant,
                                "state": client_work_state(&step.status),
                                "since": client_timestamp(step.updated_at_unix_ms),
                            })
                        })
                        .collect::<Vec<_>>();
                    let last_progress = run
                        .steps
                        .iter()
                        .filter_map(|step| {
                            Some((step.progress_at_unix_ms?, step.progress_summary.as_ref()?))
                        })
                        .max_by_key(|(at, _)| *at)
                        .map(|(_, summary)| summary.clone());
                    let must_act =
                        if matches!(run.status.as_str(), "completed" | "failed" | "cancelled") {
                            "nobody"
                        } else if human_attention_runs.contains(run.subject.as_str()) {
                            "you"
                        } else if run.steps.iter().any(|step| {
                            matches!(step.status.as_str(), "ready" | "claimed" | "working")
                                && (step.assigned_to.is_some()
                                    || !step.available_to.is_empty()
                                    || step.claimant.is_some())
                        }) {
                            "agent"
                        } else if run.status == "blocked"
                            || run.steps.iter().any(|step| step.status == "blocked")
                        {
                            "blocked"
                        } else {
                            "system"
                        };
                    let run_state = run_states.get(&run.subject);
                    let state_since = run_state
                        .map(|state| state.since_unix_ms)
                        .unwrap_or(run.created_at_unix_ms);
                    // An outcome counts only while the run is still over.
                    let outcome = run_state
                        .and_then(|state| state.outcome.as_ref())
                        .filter(|_| run.phase == "terminal")
                        .map(|outcome| {
                            json!({
                                "status": outcome.status,
                                "previous_status": outcome.previous_status,
                                "reason": outcome.reason,
                                "actor": outcome.actor,
                                "at": client_timestamp(outcome.at_unix_ms),
                            })
                        });
                    let blocker =
                        run.steps.iter().find(|step| step.status == "blocked").map(
                            |step| json!({"step": step.subject, "reason": step.blocked_reason}),
                        );
                    // A mission carries the steps of its open runs and its latest run, and its
                    // detail carries every run's steps, so a client never joins work to missions.
                    let shows_steps = selected_id.is_some()
                        || !matches!(run.status.as_str(), "completed" | "failed" | "cancelled")
                        || latest.is_some_and(|latest| latest.subject == run.subject);
                    let steps = shows_steps.then(|| {
                        run.steps
                            .iter()
                            .map(|step| {
                                json!({
                                    "id": step.subject,
                                    "path": step.step,
                                    "title": step.title,
                                    "state": client_work_state(&step.status),
                                    "attempt": step.attempt,
                                    "assignee": step.assigned_to,
                                    "claimant": step.claimant,
                                    "agentless": step.agentless,
                                    "since": client_timestamp(step.updated_at_unix_ms),
                                    "last_progress": step.progress_summary,
                                    "blocked_reason": step.blocked_reason,
                                    "blockers": step.blockers,
                                    "goals": step.goals,
                                    "constraints": step.constraints,
                                })
                            })
                            .collect::<Vec<_>>()
                    });
                    Ok::<Value, anyhow::Error>(json!({
                        "id": run.subject,
                        "generation_id": run.generation,
                        "requester": run.requester,
                        "status": run.status,
                        "phase": run.phase,
                        "progress": {"done": done, "total": run.steps.len()},
                        "current_steps": current_steps,
                        "must_act": must_act,
                        "state_since": client_timestamp(state_since),
                        "outcome": outcome,
                        "last_progress": last_progress,
                        "blocker": blocker,
                        "after": run.after,
                        "deadline": run.deadline_at_unix_ms.map(client_timestamp),
                        "steps": steps,
                    }))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let must_act = ["you", "agent", "blocked", "system"]
                .into_iter()
                .find(|kind| run_details.iter().any(|run| run["must_act"] == *kind))
                .unwrap_or("nobody");
            let usage = aggregate_usage_values(
                run_ids
                    .iter()
                    .filter_map(|run| usage_by_run.get(run))
                    .flat_map(|summaries| summaries.iter().copied()),
            );
            let revision = latest
                .map(|run| run.revision.as_str())
                .or_else(|| definition.map(|(definition, _)| definition.revision.as_str()))
                .expect("a mission resource has a definition or a run");
            let updated_at_unix_ms = latest
                .map(|run| run.updated_at_unix_ms)
                .or_else(|| definition.map(|(_, updated_at)| *updated_at))
                .expect("a mission resource has a definition or a run timestamp");
            let visualization = if selected_id.is_some() {
                mission_visualization(store, &mission, revision, state)?
            } else {
                None
            };
            Ok::<Option<Value>, anyhow::Error>(Some(json!({
                "id": mission,
                "kind": "mission",
                "revision": revision,
                "updated_at": client_timestamp(updated_at_unix_ms),
                "title": mission.strip_prefix("mission/").unwrap_or(&mission),
                "state": state,
                "mission_revision": revision,
                "runs": runs.into_iter().map(|run| run.subject).collect::<Vec<_>>(),
                "run_details": run_details,
                "must_act": must_act,
                "active_runs": active_runs,
                "run_generations": run_generations,
                "visualization": visualization,
                "usage": usage,
                "operational": {
                    "layer": if historical { "history" } else { "current" },
                    "actionable": !historical,
                    "reasons": if historical { vec![state] } else { Vec::<&str>::new() }
                }
            })))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right["updated_at"]
            .as_str()
            .cmp(&left["updated_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(values)
}

fn runtime_resources(
    state: &AppState,
    history: bool,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
) -> anyhow::Result<Vec<Value>> {
    // Runtime authority must come from the same status reduction used by every
    // other control path. A raw claim ordered last by this replica's ingest
    // index is not necessarily the causally current runtime observation.
    let status = state.store.status_for_claim_kind_at(
        "runtime.observed",
        Some(snapshot.store_index),
        history,
    )?;
    // Each runtime's declaration and observation time, in one statement apiece for the list.
    let desired_tokens = state.store.selected_desired_tokens(
        &status
            .subjects
            .iter()
            .map(|selected| selected.subject.as_str())
            .collect::<Vec<_>>(),
    )?;
    let claim_times = state.store.claim_acceptance_times(
        &status
            .subjects
            .iter()
            .filter_map(|selected| selected.actual_claim.as_deref())
            .collect::<Vec<_>>(),
    )?;
    let mut values = Vec::new();
    for selected in status.subjects {
        let Some(actual) = selected.actual.as_ref() else {
            continue;
        };
        let fields = actual.get("fields").unwrap_or(actual);
        let Some(runtime_id) = fields.get("runtime_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(actual_claim) = selected.actual_claim.as_deref() else {
            continue;
        };
        let Some(actual_origin) = selected.actual_origin.as_deref() else {
            continue;
        };
        let observed = fields
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let mut runtime_state = match observed {
            "ready" | "working" | "idle" => "running",
            "absent" => "stopped",
            other @ ("pending" | "starting" | "running" | "stopping" | "stopped" | "exited"
            | "failed" | "unreachable") => other,
            _ => "pending",
        };
        let terminal = fields
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let owner_id = selected.subject.clone();
        let terminal_id = terminal.then(|| format!("terminal/{owner_id}"));
        let owner_host_id = client_host_id(actual_origin);
        let authoritative = matches!(selected.reachability.as_str(), "reachable" | "local");
        if !authoritative {
            runtime_state = "unreachable";
        }
        let local = authoritative && observed == "running" && actual_origin == state.store.origin();
        let incarnation_id = fields.get("incarnation_id").and_then(Value::as_str);
        let desired_revision = desired_tokens.get(&owner_id).cloned();
        let updated_at = claim_times
            .get(actual_claim)
            .map(|accepted_at| client_timestamp(*accepted_at))
            .unwrap_or_else(|| snapshot.created_at.clone());
        let reasons = selected.reason.into_iter().collect::<Vec<_>>();
        values.push(json!({
            "id": format!("runtime/{runtime_id}"),
            "kind": "runtime",
            "revision": actual_claim,
            "updated_at": updated_at,
            "runtime_kind": if terminal { "terminal" } else { "agent" },
            "owner_id": owner_id,
            "owner_host_id": owner_host_id,
            "state": runtime_state,
            "runtime_id": runtime_id,
            "incarnation_id": incarnation_id,
            "desired_revision": desired_revision,
            "owner_run_id": selected.owner_run,
            "terminal_id": terminal_id,
            // Screen fences come from terminal.screen, never from a graph projection.
            "terminal_sequence": null,
            "terminal_access": terminal.then(|| json!({
                "read": if local && session.allows("terminal.read") { "granted" } else { "unavailable" },
                "input": if local && session.allows("terminal.control") { "granted" } else { "unavailable" },
                "resize": if local && session.allows("terminal.control") { "granted" } else { "unavailable" }
            })),
            "operational": {
                "layer": selected.projection.layer,
                "actionable": authoritative,
                "reasons": reasons,
                "runtime_incarnation": incarnation_id
            }
        }));
    }
    values.sort_by(|left, right| {
        left["owner_id"]
            .as_str()
            .cmp(&right["owner_id"].as_str())
            .then_with(|| {
                left["runtime_kind"]
                    .as_str()
                    .cmp(&right["runtime_kind"].as_str())
            })
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(values)
}

fn observer_subscription_resources(
    state: &AppState,
    kind: &str,
    history: bool,
    snapshot: &ClientSnapshot,
) -> anyhow::Result<Vec<Value>> {
    let mut values = Vec::new();
    for desired in state.store.desired_subjects()? {
        if desired.kind != kind
            && !(desired.kind == "stop" && desired.subject.starts_with(&format!("{kind}/")))
        {
            continue;
        }
        let spec = if kind == "observer" {
            let Some(spec) = crate::graph::observer_spec(&desired.desired) else {
                continue;
            };
            serde_json::to_value(spec)?
        } else {
            let Some(spec) = crate::graph::subscription_spec(&desired.desired) else {
                continue;
            };
            serde_json::to_value(spec)?
        };
        let stopped = spec
            .get("stopped")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if stopped && !history {
            continue;
        }
        let claim_kind = if kind == "observer" {
            "observer.state"
        } else {
            "subscription.state"
        };
        let claim = state
            .store
            .claims_for_subject_kind_at(
                &desired.subject,
                claim_kind,
                Some(snapshot.store_index.saturating_add(1)),
                true,
                1,
            )?
            .claims
            .into_iter()
            .next();
        let observed_state = claim
            .as_ref()
            .and_then(|claim| claim.body.pointer("/fields/state"))
            .and_then(Value::as_str);
        let state_name = if stopped {
            "stopped"
        } else {
            observed_state.unwrap_or("pending")
        };
        let revision = state
            .store
            .selected_desired_revision(&desired.subject)?
            .unwrap_or_else(|| "unknown".into());
        let updated_at = claim
            .as_ref()
            .map(|claim| client_timestamp(claim.accepted_at_unix_ms))
            .unwrap_or_else(|| snapshot.created_at.clone());
        values.push(json!({
            "id": desired.subject,
            "kind": kind,
            "revision": revision,
            "updated_at": updated_at,
            "state": state_name,
            "spec": spec,
            "owner_run_id": desired.owner_run,
            "owner_generation_id": desired.owner_generation,
            "owner_step_id": desired.owner_step,
            "operational": {
                "layer": if stopped { "history" } else { "current" },
                "actionable": !stopped,
                "reasons": if stopped { vec!["stopped"] } else { Vec::<&str>::new() }
            }
        }));
    }
    values.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(values)
}

/// Every open lane, or every declared lane with history, in client form.
pub(super) async fn lanes(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let store = state.store.clone();
    let history = query.history;
    let items = super::blocking_store(move || {
        Ok(store.lanes(history)?.iter().map(lane_resource).collect())
    })
    .await?;
    client_page(&state, &snapshot, "lanes", items, &query).map(Json)
}

pub(super) async fn lane_detail(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let store = state.store.clone();
    let lane = client_detail_id("lane", &id);
    let view = super::blocking_store(move || store.lane(&lane)).await?;
    view.map(|view| Json(lane_resource(&view)))
        .ok_or_else(|| ApiError::not_found(format!("lane `{id}` does not exist")))
}

/// One lane as a client resource: its declaration, its entries in order, and recent changes.
pub(super) fn lane_resource(lane: &crate::model::LaneView) -> Value {
    let prefix = lane.entries_prefix.as_deref();
    let timestamp = |at: Option<u128>| at.map(client_timestamp);
    json!({
        "id": lane.subject,
        "kind": "lane",
        "revision": lane.revision,
        "updated_at": client_timestamp(lane.updated_at_unix_ms.unwrap_or_default()),
        "name": lane.name,
        "mission_run_id": lane.run,
        "mission_id": lane.mission,
        "entries_prefix": lane.entries_prefix,
        "approver_id": lane.approver,
        "state": if lane.open { "open" } else { "closed" },
        "entries": lane.entries.iter().map(|entry| json!({
            "entry_id": entry.entry,
            "label": crate::lane::short_entry(prefix, &entry.entry),
            "position": entry.position,
            "state": entry.state,
            "detail": entry.detail,
            "head": entry.head,
            "marked_by_id": entry.marked_by,
            "marked_at": timestamp(entry.marked_at_unix_ms),
            "joined_by_id": entry.joined_by,
            "joined_at": client_timestamp(entry.joined_at_unix_ms),
            "join_reason": entry.join_reason,
            "approved_by_id": entry.approved_by,
            "approved_at": timestamp(entry.approved_at_unix_ms),
        })).collect::<Vec<_>>(),
        "recent": lane.recent.iter().map(|recent| json!({
            "change": recent.kind,
            "entry_id": recent.entry,
            "label": crate::lane::short_entry(prefix, &recent.entry),
            "actor_id": recent.actor,
            "at": client_timestamp(recent.at_unix_ms),
            "outcome": recent.outcome,
            "placement": recent.placement,
            "anchor_id": recent.anchor,
            "reason": recent.reason,
        })).collect::<Vec<_>>(),
        "operational": {
            "layer": if lane.open { "current" } else { "history" },
            "actionable": lane.open,
            "reasons": if lane.open { Vec::<&str>::new() } else { vec!["closed"] }
        }
    })
}

pub(super) async fn observers(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let items = observer_subscription_resources(&state, "observer", query.history, &snapshot)
        .map_err(ApiError::internal)?;
    client_page(&state, &snapshot, "observers", items, &query).map(Json)
}

pub(super) async fn observer_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    client_detail(
        observer_subscription_resources(&state, "observer", true, &snapshot)
            .map_err(ApiError::internal)?,
        "observer",
        &id,
    )
}

pub(super) async fn subscriptions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let items = observer_subscription_resources(&state, "subscription", query.history, &snapshot)
        .map_err(ApiError::internal)?;
    client_page(&state, &snapshot, "subscriptions", items, &query).map(Json)
}

pub(super) async fn subscription_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    client_detail(
        observer_subscription_resources(&state, "subscription", true, &snapshot)
            .map_err(ApiError::internal)?,
        "subscription",
        &id,
    )
}

fn machine_resources(
    state: &AppState,
    history: bool,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
) -> anyhow::Result<Vec<Value>> {
    let runtimes = runtime_resources(state, history, snapshot, session)?;
    let mut host_runtime_ids = BTreeMap::<String, BTreeSet<String>>::new();
    let mut host_running_runtimes = BTreeMap::<String, usize>::new();
    let mut host_current_runtime_membership = BTreeSet::<String>::new();
    let mut runtime_owner_hosts = BTreeMap::<String, String>::new();
    let mut host_updated_at = BTreeMap::<String, String>::new();
    for runtime in &runtimes {
        let Some(host_id) = runtime["owner_host_id"].as_str() else {
            continue;
        };
        let Some(runtime_id) = runtime["id"].as_str() else {
            continue;
        };
        host_runtime_ids
            .entry(host_id.to_owned())
            .or_default()
            .insert(runtime_id.to_owned());
        if runtime["state"].as_str() == Some("running") {
            *host_running_runtimes.entry(host_id.to_owned()).or_default() += 1;
        }
        if runtime
            .pointer("/operational/layer")
            .and_then(Value::as_str)
            == Some("current")
        {
            host_current_runtime_membership.insert(host_id.to_owned());
        }
        if let Some(updated_at) = runtime["updated_at"].as_str() {
            host_updated_at
                .entry(host_id.to_owned())
                .and_modify(|current| {
                    if updated_at > current.as_str() {
                        *current = updated_at.to_owned();
                    }
                })
                .or_insert_with(|| updated_at.to_owned());
        }
        if let Some(owner_id) = runtime["owner_id"].as_str() {
            runtime_owner_hosts.insert(owner_id.to_owned(), host_id.to_owned());
        }
    }

    // Machines need only the claimant, subject, and update time of the steps a claimant holds.
    // Building full client work resources also reduces usage history, wake history and mission
    // annotations for every step, which makes this small host list expensive during the
    // startup burst when many seats connect at once.
    let work = state
        .store
        .client_work_claims_at_snapshot(history, client_snapshot_time(snapshot))?;
    let mut host_work = BTreeMap::<String, BTreeSet<String>>::new();
    for item in work {
        let Some(claimant) = item.claimant.as_deref() else {
            continue;
        };
        let Some(host_id) = runtime_owner_hosts.get(claimant) else {
            continue;
        };
        host_work
            .entry(host_id.clone())
            .or_default()
            .insert(item.subject);
        let updated_at = client_timestamp(item.updated_at_unix_ms);
        host_updated_at
            .entry(host_id.clone())
            .and_modify(|current| {
                if updated_at > *current {
                    *current = updated_at.clone();
                }
            })
            .or_insert(updated_at);
    }

    let local_host = client_host_id(&state.node);
    let mut host_ids = BTreeSet::from([local_host.clone()]);
    // Fleet members count as configured hosts; ended members are history. A config peer that
    // ended as a member is history too, even while its [[peers]] entry remains.
    let fleet = state.store.fleet_view_for_client()?;
    let ended_hosts = fleet
        .members
        .iter()
        .filter(|member| fleet.current(&member.name).is_empty())
        .map(|member| client_host_id(&member.name))
        .chain(fleet.legacy_removed.iter().map(|name| client_host_id(name)))
        .collect::<BTreeSet<_>>();
    let configured_hosts = state
        .configured_peers
        .iter()
        .map(|peer| client_host_id(peer))
        .chain(
            fleet
                .members
                .iter()
                .filter(|member| member.state != "ended")
                .map(|member| client_host_id(&member.name)),
        )
        .filter(|host| !ended_hosts.contains(host))
        .collect::<BTreeSet<_>>();
    host_ids.extend(configured_hosts.iter().cloned());
    host_ids.extend(host_runtime_ids.keys().cloned());
    let status =
        state
            .store
            .status_for_subject_prefix_at("host/", Some(snapshot.store_index), history)?;
    let mut host_statuses = status
        .subjects
        .into_iter()
        .filter(|subject| {
            subject.kind.as_deref() == Some("host") || subject.subject.starts_with("host/")
        })
        .map(|subject| (subject.subject.clone(), subject))
        .collect::<BTreeMap<_, _>>();
    if history {
        host_ids.extend(host_statuses.keys().cloned());
    }

    let mut machines = Vec::new();
    for host_id in host_ids {
        let name = host_id.strip_prefix("host/").unwrap_or(&host_id).to_owned();
        let current_member = host_id == local_host
            || configured_hosts.contains(&host_id)
            || host_current_runtime_membership.contains(&host_id);
        let mut updated_at = host_updated_at
            .get(&host_id)
            .cloned()
            .unwrap_or_else(|| client_timestamp(0));
        let (
            machine_state,
            transports,
            operational_layer,
            operational_actionable,
            operational_reasons,
        ) = if host_id != local_host && configured_hosts.contains(&host_id) {
            let (recent, last_success_at) = state.store.replication_peer_up(&name)?;
            (
                if recent { "reachable" } else { "last-seen" },
                vec![json!({
                    "protocol": "replication",
                    "status": if recent { "up" } else { "last-seen" },
                    "last_success_at": last_success_at.map(client_timestamp),
                })],
                "current".to_owned(),
                recent,
                vec!["replication-transport".to_owned()],
            )
        } else if host_id == local_host {
            (
                "local",
                vec![json!({
                    "protocol": "unix",
                    "status": "local",
                    "last_success_at": Value::Null,
                })],
                "current".to_owned(),
                true,
                vec!["authoritative-local-host".to_owned()],
            )
        } else if let Some(selected) = host_statuses.remove(&host_id) {
            let actual = selected.actual.as_ref();
            let fields = actual
                .and_then(|actual| actual.get("fields"))
                .or(actual)
                .unwrap_or(&Value::Null);
            let status = fields
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let conflicted = !selected.conflicts.is_empty()
                || !matches!(selected.reachability.as_str(), "reachable" | "local");
            let machine_state = match (conflicted, status) {
                (true, _) | (false, "unknown") => "indeterminate",
                (false, "up") => "reachable",
                (false, "down") => "unreachable",
                (false, _) => "indeterminate",
            };
            if let Some(claim) = selected.actual_claim.as_deref()
                && let Some(claim) = state.store.claim_by_id(claim)?
            {
                let claim_updated_at = client_timestamp(claim.accepted_at_unix_ms);
                if claim_updated_at > updated_at {
                    updated_at = claim_updated_at;
                }
            }
            let layer = if current_member {
                selected.projection.layer.clone()
            } else {
                "history".to_owned()
            };
            let mut reasons = selected.projection.reasons.clone();
            if current_member {
                reasons.push("replication-transport".to_owned());
            } else {
                reasons.push("discovered-history".to_owned());
            }
            if conflicted {
                reasons.push("authority-indeterminate".to_owned());
            }
            reasons.sort();
            reasons.dedup();
            // The transport claim changes only with the peer's status, so its success time
            // goes stale while the peer stays up. The peer row records every success.
            let last_success_at = fields
                .get("last_success_at")
                .and_then(Value::as_u64)
                .map(u128::from)
                .max(state.store.replication_peer_last_success(&name)?);
            (
                machine_state,
                vec![json!({
                    "protocol": fields.get("protocol").and_then(Value::as_str).unwrap_or("replication"),
                    "status": if matches!(status, "up" | "down") { status } else { "unknown" },
                    "last_success_at": last_success_at.map(client_timestamp),
                })],
                layer.clone(),
                layer == "current"
                    && selected.projection.actionable
                    && machine_state != "indeterminate",
                reasons,
            )
        } else {
            let reason = if configured_hosts.contains(&host_id) {
                "configured-unobserved"
            } else {
                "runtime-owner-host-unobserved"
            };
            (
                "indeterminate",
                vec![json!({
                    "protocol": "replication",
                    "status": "unknown",
                    "last_success_at": Value::Null,
                })],
                "current".to_owned(),
                false,
                vec![reason.to_owned()],
            )
        };
        let running_runtimes = host_running_runtimes.remove(&host_id).unwrap_or_default();
        let runtime_ids = host_runtime_ids
            .remove(&host_id)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let work = host_work
            .remove(&host_id)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let mut machine = json!({
            "host_id": host_id,
            "name": name.clone(),
            "state": machine_state,
            "fleet_id": state.fleet_id,
            "capacity": {
                "state": "unknown",
                "reason": "no capacity observation",
            },
            "occupancy": {
                "running_runtimes": running_runtimes,
            },
            "projects": [],
            "work": work,
            "transports": transports,
            "runtime_ids": runtime_ids,
            "operational": {
                "layer": operational_layer,
                "actionable": operational_actionable,
                "reasons": operational_reasons,
            }
        });
        let revision = format!(
            "machine:{}",
            hex::encode(Sha256::digest(
                serde_json::to_vec(&machine).expect("machine projection serializes")
            ))
        );
        let fields = machine
            .as_object_mut()
            .expect("machine projection is an object");
        fields.insert("id".into(), Value::String(format!("machine/{name}")));
        fields.insert("kind".into(), Value::String("machine".into()));
        fields.insert("revision".into(), Value::String(revision));
        fields.insert("updated_at".into(), Value::String(updated_at));
        machines.push(machine);
    }
    machines.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(machines)
}

/// How long page reads serve the daemon's last diagnostic report before one of them asks for a
/// new report.
const OPERATION_REPORT_REFRESH: Duration = Duration::from_secs(30);

struct OperationReport {
    store: std::sync::Weak<crate::store::Store>,
    /// `None` while the first report since the daemon started is being made.
    checks: Option<Arc<Vec<crate::model::DoctorCheck>>>,
    at: Instant,
    refreshing: bool,
}

/// The daemon's last diagnostic report for each store, which the operations collection lists.
/// Some checks compare the whole projection with the claim log, seconds of work on a busy host's
/// store, so page reads serve the last report and a new one is made off the request path.
static OPERATION_REPORTS: OnceLock<Mutex<BTreeMap<usize, OperationReport>>> = OnceLock::new();

fn operation_reports() -> std::sync::MutexGuard<'static, BTreeMap<usize, OperationReport>> {
    OPERATION_REPORTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Start the daemon's first diagnostic report off the request path, so no read waits for it. The
/// daemon calls this as its API starts to listen; until the report is made, the operations
/// collection says that it is being made.
pub(super) fn start_operation_report(state: &AppState) {
    let key = Arc::as_ptr(&state.store) as usize;
    let mut reports = operation_reports();
    if reports.get(&key).is_some_and(|report| {
        report
            .store
            .upgrade()
            .is_some_and(|store| Arc::ptr_eq(&store, &state.store))
    }) {
        return;
    }
    reports.insert(
        key,
        OperationReport {
            store: Arc::downgrade(&state.store),
            checks: None,
            at: Instant::now(),
            refreshing: true,
        },
    );
    let state = state.clone();
    std::thread::spawn(move || refresh_operation_report(&state, key));
}

/// The last diagnostic report, or `None` while the first one since the daemon started is being
/// made.
fn operation_checks(
    state: &AppState,
) -> Result<Option<Arc<Vec<crate::model::DoctorCheck>>>, ApiError> {
    let key = Arc::as_ptr(&state.store) as usize;
    {
        let mut reports = operation_reports();
        if let Some(report) = reports.get_mut(&key).filter(|report| {
            report
                .store
                .upgrade()
                .is_some_and(|store| Arc::ptr_eq(&store, &state.store))
        }) {
            if report.checks.is_some()
                && report.at.elapsed() >= OPERATION_REPORT_REFRESH
                && !report.refreshing
            {
                report.refreshing = true;
                let state = state.clone();
                std::thread::spawn(move || refresh_operation_report(&state, key));
            }
            return Ok(report.checks.clone());
        }
    }
    // A server that did not start a report, such as a test's, makes the first one on its first
    // read.
    let checks = Arc::new(doctor_report(state)?.0.checks);
    operation_reports().insert(
        key,
        OperationReport {
            store: Arc::downgrade(&state.store),
            checks: Some(checks.clone()),
            at: Instant::now(),
            refreshing: false,
        },
    );
    Ok(Some(checks))
}

fn refresh_operation_report(state: &AppState, key: usize) {
    let checks = doctor_report(state)
        .ok()
        .map(|report| Arc::new(report.0.checks));
    let mut reports = operation_reports();
    let Some(report) = reports.get_mut(&key) else {
        return;
    };
    report.refreshing = false;
    match checks {
        Some(checks) => {
            report.checks = Some(checks);
            report.at = Instant::now();
        }
        // The first report failed: the next read makes one and answers with its error.
        None if report.checks.is_none() => drop(reports.remove(&key)),
        None => {}
    }
}

fn operation_resources(state: &AppState, at: &str) -> Result<Vec<Value>, ApiError> {
    let Some(checks) = operation_checks(state)? else {
        return Ok(vec![json!({
            "id": "operation/diagnostic-report",
            "kind": "operation",
            "revision": "diagnostic-report:running",
            "updated_at": at,
            "component": "daemon",
            "severity": "info",
            "state": "running",
            "summary": "the daemon is making its first diagnostic report since it started",
            "targets": [],
            "operational": { "layer": "current", "actionable": false, "reasons": ["diagnostic"] }
        })]);
    };
    let mut values = checks
        .iter()
        .map(|check| {
            let digest = hex::encode(Sha256::digest(check.name.as_bytes()));
            json!({
                "id": format!("operation/diagnostic-{}", &digest[..16]),
                "kind": "operation",
                "revision": format!("{}:{}", check.name, check.status),
                "updated_at": at,
                "component": match check.name.as_str() { "replication" => "transport", "runtime-drift" | "runtime-ownership" | "pty-runtime" | "driver-readiness" => "runtime", _ => "daemon" },
                "severity": match check.status.as_str() { "fail" => "critical", "warn" => "warning", _ => "info" },
                "state": match check.status.as_str() { "fail" => "failed", "warn" => "degraded", _ => "healthy" },
                "summary": check.message,
                "targets": [],
                "operational": { "layer": "current", "actionable": false, "reasons": ["diagnostic"] }
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        let severity = |value: &Value| match value["severity"].as_str() {
            Some("critical") => 0,
            Some("warning") => 1,
            _ => 2,
        };
        severity(left)
            .cmp(&severity(right))
            .then_with(|| left["component"].as_str().cmp(&right["component"].as_str()))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(values)
}

pub(super) async fn missions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    let requested_limit = query
        .limit
        .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
        .clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let (offset, limit, expires_at_unix_ms, after_key) = if let Some(encoded) = &query.cursor {
        let cursor = decode_client_cursor(encoded)?;
        if cursor.collection != "missions"
            || cursor.snapshot.id != snapshot.id
            || cursor.snapshot.store_index != snapshot.store_index
            || cursor.history != query.history
            || cursor.person != query.person
            || cursor.actor != query.actor
            || cursor.owner_run != query.owner_run
            || cursor.status != query.status
            || cursor.native_only != query.native_only
            || cursor.items_digest != "sql-page"
            || query
                .limit
                .is_some_and(|limit| limit.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
        {
            return Err(client_page_expired(
                "the mission page cursor does not match this snapshot or filter",
            ));
        }
        if client_now_ms() > cursor.expires_at_unix_ms {
            return Err(client_page_expired("the mission page cursor expired"));
        }
        (
            if cursor.after_key.is_some() {
                0
            } else {
                cursor.offset
            },
            cursor.limit,
            cursor.expires_at_unix_ms,
            cursor.after_key,
        )
    } else {
        (
            0,
            requested_limit,
            client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
            None,
        )
    };
    // Each page reads one consistent SQLite view; continuation seeks after the last
    // update time and mission ID instead of rejecting unrelated store writes.
    let reader = state.clone();
    let history = query.history;
    let read = super::blocking_store(move || {
        let store = reader.store.clone();
        store.read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader, index);
            let mut ids = store.mission_collection_page(
                history,
                offset,
                limit.saturating_add(1),
                after_key.as_ref(),
            )?;
            let mut has_more = ids.len() > limit;
            ids.truncate(limit);
            let mut items = mission_list_cards(
                &store,
                &ids.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            )?;
            has_more |= bound_mission_cards(&mut items)?;
            let after_key = items.last().and_then(|item| {
                ids.iter()
                    .find(|(id, _)| Some(id.as_str()) == item["id"].as_str())
                    .map(|(id, time)| (*time, id.clone()))
            });
            Ok(Some((snapshot, items, has_more, after_key)))
        })
    })
    .await?;
    let Some((snapshot, items, has_more, after_key)) = read else {
        return Err(client_page_expired(
            "the mission collection changed; restart pagination",
        ));
    };
    let next_cursor = has_more
        .then(|| {
            encode_client_cursor(&ClientPageCursor {
                snapshot: snapshot.clone(),
                collection: "missions".into(),
                offset: offset.saturating_add(items.len()),
                limit,
                history: query.history,
                person: query.person.clone(),
                actor: query.actor.clone(),
                owner_run: query.owner_run.clone(),
                status: query.status.clone(),
                native_only: query.native_only,
                items_digest: "sql-page".into(),
                before_index: None,
                after_key: after_key.clone(),
                expires_at_unix_ms,
            })
        })
        .transpose()?;
    let page = ClientResourcePage {
        kind: "page".into(),
        collection: "missions".into(),
        filters: if history {
            BTreeMap::from([("history".into(), "all".into())])
        } else {
            BTreeMap::new()
        },
        items,
        page: ClientPageInfo {
            limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)),
        },
        sync: client_sync_notice(&state),
        replicated: None,
    };
    Ok((Extension(snapshot), Json(page)))
}

/// A single read of the projections used by mission show, agent tree, and seat queues.
pub(super) async fn missions_tree(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let store = state.store.clone();
    let at = snapshot.created_at.clone();
    let index = snapshot.store_index;
    let view = super::blocking_store(move || missions_tree_value(&store, &at, index)).await?;
    Ok(Json(json!({ "snapshot": snapshot, "value": view })))
}

/// Preserve the latest accepted observation even after its session or incarnation changes.
pub(super) fn agent_todo_value(
    claim: Option<&ClaimRecord>,
    session: Option<&ClaimRecord>,
    incarnation: Option<&str>,
) -> Value {
    let Some(claim) = claim else {
        return Value::Null;
    };
    let snapshot = match st3_schema::HarnessTodoSnapshot::deserialize(
        claim.body.get("fields").unwrap_or(&claim.body),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(subject = %claim.subject, claim_id = %claim.id, %error,
                "invalid harness todo observation");
            return Value::Null;
        }
    };
    let session_fields = session.map(|claim| claim.body.get("fields").unwrap_or(&claim.body));
    let stale = incarnation != Some(snapshot.incarnation_id.as_str())
        || session_fields.and_then(|fields| fields["session_id"].as_str())
            != Some(snapshot.session_id.as_str());
    json!({
        "snapshot": snapshot,
        "claim_id": claim.id,
        "accepted_at": client_timestamp(claim.accepted_at_unix_ms),
        "stale": stale,
    })
}

#[cfg(test)]
fn agent_todo(
    store: &Store,
    subject: &str,
    incarnation: Option<&str>,
    index: u64,
) -> anyhow::Result<Value> {
    let observations = store.agent_todo_observations_for(&[subject.to_owned()], index)?;
    let claims = observations.get(subject);
    Ok(agent_todo_value(
        claims.and_then(|claims| claims.get("harness.todo.observed")),
        claims.and_then(|claims| claims.get("harness.session-file")),
        incarnation,
    ))
}

fn desired_child_arg(value: &Value, name: &str) -> Option<String> {
    value
        .get("children")?
        .as_array()?
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some(name))?
        .get("arguments")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_owned)
}

/// The most items of each part the missions tree lists. A fleet with more shows the first ones
/// and says how many there are, instead of failing the whole tree.
const MISSIONS_TREE_ITEMS: usize = 200;
const MISSIONS_TREE_QUEUED_RUNS: usize = 1000;

fn missions_tree_value(store: &Store, at: &str, index: u64) -> anyhow::Result<Value> {
    missions_tree_value_within(
        store,
        at,
        index,
        MISSIONS_TREE_ITEMS,
        MISSIONS_TREE_QUEUED_RUNS,
    )
}

fn missions_tree_value_within(
    store: &Store,
    at: &str,
    index: u64,
    items: usize,
    queued_limit: usize,
) -> anyhow::Result<Value> {
    let mut truncated = serde_json::Map::new();
    let mut note = |part: &str, shown: usize, total: usize| {
        if total > shown {
            truncated.insert(part.into(), json!({ "shown": shown, "total": total }));
        }
    };
    let mut runs = store.open_mission_run_headers()?;
    runs.sort_by(|a, b| {
        a.mission
            .cmp(&b.mission)
            .then_with(|| a.subject.cmp(&b.subject))
    });
    note("runs", items, runs.len());
    runs.truncate(items);
    let mut run_values = Vec::with_capacity(runs.len());
    let (mut steps_shown, mut steps_total) = (0, 0);
    for run in runs {
        // The tree shows each step's state, not its timing, wake or progress.
        let full = store
            .mission_run_steps(&run.subject, false)?
            .ok_or_else(|| anyhow::anyhow!("run disappeared: {}", run.subject))?;
        steps_total += full.steps.len();
        steps_shown += full.steps.len().min(items);
        run_values.push(json!({
            "id": full.subject, "mission": full.mission, "state": full.status,
            "steps": full.steps.iter().take(items).map(|step| json!({
                "id": step.subject, "name": step.title.as_deref().unwrap_or(&step.step),
                "path": step.step, "state": client_work_state(&step.status)
            })).collect::<Vec<_>>()
        }));
    }
    // A mission that never ran and can start, as the current mission list shows it: its
    // definition is ready or a draft, and st3's internal loop definitions stay out.
    let mut unstarted = store
        .mission_definitions_without_runs()?
        .into_iter()
        .filter(|definition| !definition.mission.subject.starts_with("mission/__st3/"))
        .filter_map(|definition| {
            let state = match definition.mission.state {
                crate::model::MissionState::Draft => "draft",
                crate::model::MissionState::Ready => "ready",
                crate::model::MissionState::Retired => return None,
            };
            let id = definition.mission.subject;
            let title = id.strip_prefix("mission/").unwrap_or(&id).to_owned();
            Some(json!({ "id": id, "title": title, "state": state }))
        })
        .collect::<Vec<_>>();
    note("steps", steps_shown, steps_total);
    unstarted.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    note("unstarted_missions", items, unstarted.len());
    unstarted.truncate(items);

    let mut agents = client_agent_resources(store, false, at, index)?;
    note("agents", items, agents.len());
    agents.truncate(items);
    // The declarations of the agents shown, not of every seat the fleet ever declared.
    let shown = agents
        .iter()
        .filter_map(|agent| agent["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let desired = store
        .desired_subjects_named(&shown)?
        .into_iter()
        .map(|seat| (seat.subject.clone(), seat))
        .collect::<BTreeMap<_, _>>();
    for agent in &mut agents {
        let Some(id) = agent["id"].as_str() else {
            continue;
        };
        let Some(seat) = desired.get(id) else {
            continue;
        };
        let host = seat
            .member
            .as_ref()
            .map(|member| member.host.clone())
            .or_else(|| desired_child_arg(&seat.desired, "host"));
        agent["host_id"] = json!(host.as_deref().map(client_host_id));
        let harness = seat
            .desired
            .get("children")
            .and_then(Value::as_array)
            .and_then(|children| children.iter().find(|child| child["name"] == "harness"));
        agent["model"] = json!(harness.and_then(|harness| desired_child_arg(harness, "model")));
        agent["effort"] = json!(harness.and_then(|harness| desired_child_arg(harness, "effort")));
        agent["seat_kind"] = json!(if agent["owner_run_id"].is_string() {
            "mission"
        } else {
            "standing"
        });
    }
    let mut queues = Vec::new();
    let (mut queued_runs, mut queues_total) = (0, 0);
    for agent in &agents {
        if agent["seat_kind"] != "standing" {
            continue;
        }
        let Some(id) = agent["id"].as_str() else {
            continue;
        };
        queues_total += 1;
        if queued_runs >= queued_limit {
            continue;
        }
        let queue = store.seat_queue(id)?;
        queued_runs += queue.runs.len();
        queues.push(agent_queue_value(&queue));
    }
    note("standing_queues", queues.len(), queues_total);
    let lanes = store
        .lanes(false)?
        .iter()
        .map(lane_resource)
        .collect::<Vec<_>>();
    Ok(json!({ "runs": run_values, "standing_queues": queues,
        "unstarted_missions": unstarted, "agents": agents, "lanes": lanes,
        "truncated": truncated }))
}

pub(super) async fn mission_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let store = state.store.clone();
    let snapshot_index = snapshot.store_index;
    let selected = client_detail_id("mission", &id);
    let items = super::blocking_store(move || {
        mission_resources(&store, snapshot_index, true, Some(&selected))
    })
    .await?;
    client_detail(items, "mission", &id)
}

pub(super) async fn runtimes(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    let history = query.history;
    client_snapshot_page(
        &state,
        snapshot,
        "runtimes",
        &query,
        move |state, snapshot| runtime_resources(state, history, snapshot, &session),
    )
    .await
}

pub(super) async fn terminals(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    let history = query.history;
    client_snapshot_page(
        &state,
        snapshot,
        "terminals",
        &query,
        move |state, snapshot| {
            let mut items = runtime_resources(state, history, snapshot, &session)?;
            items.retain(|item| item.get("terminal_id").is_some_and(Value::is_string));
            Ok(items)
        },
    )
    .await
}

pub(super) async fn runtime_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    client_detail(
        runtime_resources(&state, true, &snapshot, &session).map_err(ApiError::internal)?,
        "runtime",
        &id,
    )
}

pub(super) async fn operations(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    client_snapshot_page(&state, snapshot, "operations", &query, |state, snapshot| {
        operation_resources(state, &snapshot.created_at)
            .map_err(|error| anyhow::anyhow!(error.message))
    })
    .await
}

pub(super) async fn now(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    let person = person_filter(&session, query.person.as_deref())?;
    let mut effective_query = query.clone();
    effective_query.person.clone_from(&person);
    let (history, actor, owner_run) = (query.history, query.actor.clone(), query.owner_run.clone());
    client_snapshot_page(
        &state,
        snapshot,
        "now",
        &effective_query,
        move |state, snapshot| {
            let mut items =
                super::client_attention_resources_with_previews(state, person.as_deref(), history)?;
            // The default Now view is the person's attention queue. Mission work belongs
            // in Control; only an explicit work filter opts it into this combined view.
            if actor.is_some() || owner_run.is_some() {
                let mut work = super::client_work_resources(
                    &state.store,
                    actor.as_deref(),
                    history,
                    client_snapshot_time(snapshot),
                    snapshot.store_index,
                )?;
                if let Some(owner_run) = owner_run.as_deref() {
                    work.retain(|item| item["mission_run_id"].as_str() == Some(owner_run));
                }
                items.extend(work);
            }
            // Attention is already ranked by the daemon. Keep that order when work is included.
            Ok(items)
        },
    )
    .await
}

pub(super) async fn machines(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    let history = query.history;
    client_snapshot_page(
        &state,
        snapshot,
        "machines",
        &query,
        move |state, snapshot| machine_resources(state, history, snapshot, &session),
    )
    .await
}

fn device_resources(
    state: &AppState,
    snapshot: &ClientSnapshot,
    person: &str,
) -> Result<Vec<Value>, ApiError> {
    // Unrelated traffic must not hide recent devices. Read only indexed pairing history,
    // in bounded pages, at the requested snapshot; keep every page so old names and later
    // revocations remain visible even after many pairings.
    let mut claims = Vec::new();
    for kind in [
        "custom.client.pairing-begun",
        "custom.client.pairing-completed",
        "custom.client.pairing-revoked",
    ] {
        let mut before = snapshot.store_index.checked_add(1);
        loop {
            let page = state
                .store
                .claims_for_kind_at(kind, before, true, 256)
                .map_err(ApiError::internal)?;
            claims.extend(page.claims);
            let Some(cursor) = page.next_cursor else {
                break;
            };
            before = Some(cursor);
        }
    }
    claims.sort_by_key(|claim| claim.store_index);
    let mut paired = BTreeMap::<String, (&ClaimRecord, Option<&ClaimRecord>)>::new();
    let mut names = BTreeMap::<String, String>::new();
    for claim in &claims {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        if claim.kind == "custom.client.pairing-begun"
            && let Some(name) = fields.get("device_name").and_then(Value::as_str)
        {
            names.insert(claim.subject.clone(), name.to_owned());
        }
        let Some(device_id) = fields.get("device_id").and_then(Value::as_str) else {
            continue;
        };
        match claim.kind.as_str() {
            "custom.client.pairing-completed"
                if fields.get("person_id").and_then(Value::as_str) == Some(person) =>
            {
                paired.insert(device_id.to_owned(), (claim, None));
            }
            "custom.client.pairing-revoked" => {
                if let Some(entry) = paired.get_mut(device_id) {
                    entry.1 = Some(claim);
                }
            }
            _ => {}
        }
    }
    let at = client_snapshot_time(snapshot);
    let mut resources = paired
        .into_iter()
        .map(|(device_id, (completed, revoked))| {
            let fields = completed.body.get("fields").unwrap_or(&completed.body);
            let expires_at = fields
                .get("expires_at_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from)
                .unwrap_or_default();
            let selected = revoked.unwrap_or(completed);
            let state_name = if revoked.is_some() {
                "revoked"
            } else if expires_at <= at {
                "expired"
            } else {
                "active"
            };
            json!({
                "id": device_id,
                "kind": "device",
                "revision": selected.id,
                "updated_at": client_timestamp(selected.accepted_at_unix_ms),
                "person_id": person,
                "name": names.get(&completed.subject),
                "session_actor": fields.get("session_actor").cloned().unwrap_or(Value::Null),
                "state": state_name,
                "scopes": fields.get("scopes").cloned().unwrap_or_else(|| json!([])),
                "expires_at": client_timestamp(expires_at),
                "operational": {
                    "layer": if state_name == "active" { "current" } else { "history" },
                    "actionable": state_name == "active",
                    "reasons": if state_name == "active" { Vec::<&str>::new() } else { vec![state_name] }
                }
            })
        })
        .collect::<Vec<_>>();
    resources.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(resources)
}

pub(super) async fn devices(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let person = person_filter(&session, query.person.as_deref())?
        .filter(|person| person.starts_with("person/"))
        .ok_or_else(|| forbidden("device inventory requires an explicitly authenticated person"))?;
    let mut effective_query = query.clone();
    effective_query.person = Some(person.clone());
    if effective_query.cursor.is_some() {
        return client_page(&state, &snapshot, "devices", Vec::new(), &effective_query).map(Json);
    }
    let mut items = device_resources(&state, &snapshot, &person)?;
    if !query.history {
        items.retain(|item| item["state"] == "active");
    }
    client_page(&state, &snapshot, "devices", items, &effective_query).map(Json)
}

/// One seat's current claim, its queued mission runs in order, and recent moves.
pub(super) async fn agent_queue(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let agent = client_detail_id("agent", &id);
    let store = state.store.clone();
    let lookup = agent.clone();
    let queue = blocking_store(move || {
        if store.latest_claim(&lookup, None)?.is_none() {
            return Ok(None);
        }
        store.seat_queue(&lookup).map(Some)
    })
    .await?
    .ok_or_else(|| ApiError::not_found(format!("agent `{agent}` does not exist")))?;
    Ok(Json(agent_queue_value(&queue)))
}

fn agent_queue_value(queue: &crate::model::SeatQueueView) -> Value {
    json!({
        "kind": "agent-queue",
        "agent_id": queue.agent,
        "current_work_ids": queue.current_work_ids,
        "next_work_id": queue.next_work_id,
        "runs": queue.runs.iter().map(|run| json!({
            "mission_run_id": run.run,
            "position": run.position,
            "state": run.state,
            "run_state": run.run_status,
            "joined_at": client_timestamp(run.joined_at_unix_ms),
            "claimed_work_ids": run.claimed_work_ids,
            "ready_work_ids": run.ready_work_ids,
            "waiting_work_ids": run.waiting_work_ids,
            "waiting_for_run_id": run.waiting_for,
        })).collect::<Vec<_>>(),
        "moves": queue.moves.iter().map(|moved| json!({
            "claim_id": moved.claim_id,
            "mission_run_id": moved.run,
            "placement": moved.placement,
            "anchor_run_id": moved.anchor,
            "actor_id": moved.actor,
            "reason": moved.reason,
            "moved_at": client_timestamp(moved.moved_at_unix_ms),
        })).collect::<Vec<_>>(),
        "move_count": queue.move_count,
    })
}

pub(super) async fn operation_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    client_detail(
        operation_resources(&state, &snapshot.created_at)?,
        "operation",
        &id,
    )
}

fn timeline_attribution(owner: &str, desired: &[crate::model::DesiredSubject]) -> Value {
    let ownership = desired.iter().find(|desired| desired.subject == owner);
    json!({
        "agent_id": owner,
        "mission_run_id": ownership.and_then(|value| value.owner_run.as_deref()),
        "generation_id": ownership.and_then(|value| value.owner_generation.as_deref()),
        "step_id": ownership.and_then(|value| value.owner_step.as_deref()),
    })
}

fn timeline_retention_is_explicit(claims: &[ClaimRecord], has_older: bool) -> bool {
    if !has_older {
        return true;
    }
    let earliest_retained = claims
        .iter()
        .filter_map(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            (fields.get("entry_type").and_then(Value::as_str) != Some("truncation"))
                .then(|| fields.get("sequence").and_then(Value::as_u64))
                .flatten()
        })
        .min();
    let Some(required_through) = earliest_retained.and_then(|sequence| sequence.checked_sub(1))
    else {
        return false;
    };
    let mut intervals = claims
        .iter()
        .filter_map(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            (fields.get("operation").and_then(Value::as_str) == Some("append")
                && fields.get("entry_type").and_then(Value::as_str) == Some("truncation"))
            .then(|| {
                fields
                    .pointer("/body/omitted_from_sequence")
                    .and_then(Value::as_u64)
                    .zip(
                        fields
                            .pointer("/body/omitted_to_sequence")
                            .and_then(Value::as_u64),
                    )
            })
            .flatten()
            .filter(|(from, to)| from <= to)
        })
        .collect::<Vec<_>>();
    intervals.sort_unstable();
    let mut covered_through = 0_u64;
    for (from, to) in intervals {
        if from > covered_through.saturating_add(1) {
            break;
        }
        covered_through = covered_through.max(to);
        if covered_through >= required_through {
            return true;
        }
    }
    false
}

fn normalized_timeline_usage_body(
    body: Value,
    attribution: &Value,
    source_driver: Option<&str>,
) -> Value {
    let mut body = body.as_object().cloned().unwrap_or_default();
    if !matches!(
        body.get("semantics").and_then(Value::as_str),
        Some("context_occupancy" | "session_cumulative" | "response")
    ) {
        // Legacy/provider events without declared cumulative semantics are one
        // response observation; treating them as cumulative would undercount.
        body.insert("semantics".into(), Value::String("response".into()));
    }
    if let Some(driver) = source_driver.filter(|driver| !driver.is_empty()) {
        body.insert("driver".into(), Value::String(driver.into()));
    } else if !body
        .get("driver")
        .and_then(Value::as_str)
        .is_some_and(|driver| !driver.is_empty())
    {
        body.insert("driver".into(), Value::String("unknown".into()));
    }
    body.insert("attribution".into(), attribution.clone());
    Value::Object(body)
}

/// The Small Talk in one session's conversation: messages sent for this session, and messages
/// to or from its agent that name no session, accepted while this incarnation was the agent's
/// current one. st joins them into the timeline so every client shows the same conversation.
fn session_messages(
    state: &AppState,
    owner: &str,
    session_id: &str,
    incarnation: Option<&str>,
    before: Option<u64>,
) -> Result<Vec<ClaimRecord>, ApiError> {
    // The incarnation's life: from its first runtime observation to the next incarnation's.
    let (mut started, mut ended) = (None::<u128>, None::<u128>);
    if let Some(incarnation) = incarnation {
        let observed = state
            .store
            .claims_for(owner, Some("runtime.observed"))
            .map_err(ApiError::internal)?;
        let of = |claim: &ClaimRecord| {
            claim
                .body
                .pointer("/fields/incarnation_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        started = observed
            .iter()
            .filter(|claim| of(claim).as_deref() == Some(incarnation))
            .map(|claim| claim.accepted_at_unix_ms)
            .min();
        if let Some(started) = started {
            ended = observed
                .iter()
                .filter(|claim| {
                    claim.accepted_at_unix_ms > started
                        && of(claim).is_some_and(|other| other != incarnation)
                })
                .map(|claim| claim.accepted_at_unix_ms)
                .min();
        }
    }
    let mut messages = state
        .store
        .claims_for_kind_at("message.sent", before, true, 10_000)
        .map_err(ApiError::internal)?
        .claims;
    messages.retain(|claim| {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let from = fields.get("from").and_then(Value::as_str);
        let to = fields.get("to").and_then(Value::as_str);
        if from != Some(owner) && to != Some(owner) {
            return false;
        }
        match fields.get("session_id").and_then(Value::as_str) {
            Some(message_session) => message_session == session_id,
            None => {
                started.is_none_or(|started| claim.accepted_at_unix_ms >= started)
                    && ended.is_none_or(|ended| claim.accepted_at_unix_ms < ended)
            }
        }
    });
    Ok(messages)
}

/// A message's timeline body: who wrote to whom, about what, so a client can draw Small Talk
/// apart from the harness's own turns.
fn session_message_body(claim: &ClaimRecord) -> Value {
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let mut body = json!({
        "message_id": claim.subject,
        "reply_to": fields.get("in_reply_to"),
        "from": fields.get("from"),
        "to": fields.get("to"),
    });
    if let Some(title) = fields.get("title").and_then(Value::as_str) {
        body["title"] = Value::String(title.to_owned());
    }
    if let Some(tags) = fields.get("tags").and_then(Value::as_array)
        && !tags.is_empty()
    {
        body["tags"] = Value::Array(tags.clone());
    }
    let attachments: Vec<crate::model::MessageAttachment> = fields
        .get("attachments")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    if !attachments.is_empty() {
        body["attachments"] = attachments.iter().map(super::client_attachment).collect();
    }
    body
}

fn native_timeline_page(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session_id: &str,
    query: &ClientListQuery,
    mut items: Vec<Value>,
) -> Result<Json<Value>, ApiError> {
    if let Some((owner, incarnation, _)) =
        super::managed_session_owner_at(&state.store, snapshot.store_index, session_id)
            .map_err(ApiError::internal)?
    {
        for claim in session_messages(
            state,
            &owner,
            session_id,
            incarnation.as_deref(),
            snapshot.store_index.checked_add(1),
        )? {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            let from = fields.get("from").and_then(Value::as_str);
            let role = if from == Some(owner.as_str()) {
                "assistant"
            } else {
                "user"
            };
            let stamp = client_timestamp(claim.accepted_at_unix_ms);
            let digest = hex::encode(Sha256::digest(claim.id.as_bytes()));
            let base = claim.store_index.saturating_mul(4);
            items.push(json!({"id":format!("timeline-entry/{}/{}-message", session_id.trim_start_matches("session/"), &digest[..16]), "sequence":base, "revision":1, "timestamp":stamp, "role":role, "type":"message", "final":true, "body":session_message_body(&claim)}));
            items.push(json!({"id":format!("timeline-entry/{}/{}-content", session_id.trim_start_matches("session/"), &digest[..16]), "sequence":base+1, "revision":1, "timestamp":stamp, "role":role, "type":"content", "final":true, "body":{"media_type":"text/plain","text":fields.get("content").and_then(Value::as_str).unwrap_or_default()}}));
        }
        items.sort_by(|a, b| {
            a["timestamp"]
                .as_str()
                .cmp(&b["timestamp"].as_str())
                .then_with(|| a["sequence"].as_u64().cmp(&b["sequence"].as_u64()))
        });
    }
    items.reverse();
    let mut page = client_page(
        state,
        snapshot,
        &format!("timeline/{session_id}"),
        items,
        query,
    )?;
    page.items.reverse();
    Ok(Json(json!({
        "kind": "timeline-page",
        "session_id": session_id,
        "items": page.items,
        "page": page.page
    })))
}

/// What st3 established about a managed seat's native transcript.
struct ManagedTranscript {
    /// The harness the seat's latest observation names.
    driver: String,
    /// The `harness.observed` claim the verdict rests on. A notice about a missing transcript is
    /// placed beside it in the timeline, so it moves forward when the observation changes.
    anchor: ClaimRecord,
    /// The seat's exact native session, or why st3 could not bind one.
    transcript: Result<crate::external_sessions::ExternalSession, Missing>,
}

/// Why a seat's transcript is not shown. `not_yet` means the harness has not written one for
/// this incarnation: the seat has said nothing since it started, which is not a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Missing {
    reason: String,
    not_yet: bool,
}

impl Missing {
    fn not_yet(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            not_yet: true,
        }
    }
}

impl From<String> for Missing {
    fn from(reason: String) -> Self {
        Self {
            reason,
            not_yet: false,
        }
    }
}

impl From<&str> for Missing {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

/// Bind a managed seat to its exact native transcript, when its harness keeps one st3 reads.
///
/// `None` means the seat runs no such harness (or has not reported one), and its timeline is
/// built from claims alone as before. Otherwise the result either carries the exact session or
/// says why none could be bound, so the timeline can say so rather than silently showing only
/// status entries. The binding is never widened to a guess: a transcript that cannot be tied to
/// this seat's current incarnation is not shown.
fn managed_transcript(
    state: &AppState,
    owner: &str,
    incarnation: &str,
) -> Result<Option<ManagedTranscript>, ApiError> {
    let Some(anchor) = state
        .store
        .latest_claim(owner, Some("harness.observed"))
        .map_err(ApiError::internal)?
    else {
        return Ok(None);
    };
    let fields = anchor.body.get("fields").unwrap_or(&anchor.body);
    let driver = fields["driver"].as_str().unwrap_or_default().to_owned();
    if !matches!(driver.as_str(), "codex" | "claude" | "omp") {
        return Ok(None);
    }
    let transcript = if fields["incarnation_id"] != incarnation {
        Err(Missing::not_yet(
            "the harness has not reported on the seat's current incarnation yet",
        ))
    } else {
        let evidence = fields["evidence_incarnation"].as_str();
        match driver.as_str() {
            "codex" => managed_codex_transcript(state, owner, evidence),
            "claude" => managed_claude_transcript(state, owner, evidence),
            _ => managed_omp_transcript(state, owner, incarnation),
        }
    };
    Ok(Some(ManagedTranscript {
        driver,
        anchor,
        transcript,
    }))
}

/// The timeline entry that says a managed seat's native transcript is not shown, and why.
/// A timeline entry saying why the seat's transcript is not shown. When st3 bound the transcript
/// but could not read it, the entry names the file, so the failure can be reported.
fn transcript_notice(session_id: &str, managed: &ManagedTranscript, reason: &str) -> Value {
    let anchor = &managed.anchor;
    let mut details = json!({ "driver": managed.driver, "claim_id": anchor.id });
    match &managed.transcript {
        Ok(external) => {
            details["transcript"] = Value::String(external.transcript.display().to_string());
        }
        // Nothing has gone wrong: the seat has said nothing since it started.
        Err(missing) if missing.not_yet => details["not_yet"] = Value::Bool(true),
        Err(_) => {}
    }
    let fields = anchor.body.get("fields").unwrap_or(&anchor.body);
    let digest = hex::encode(Sha256::digest(
        format!("{}:transcript-not-bound", anchor.id).as_bytes(),
    ));
    json!({
        "id": format!("timeline-entry/{}/{}", session_id.trim_start_matches("session/"), &digest[..24]),
        // Slot 2 of the observation's four sequence slots is otherwise unused.
        "sequence": anchor.store_index.saturating_mul(4).saturating_add(2),
        "revision": 1,
        "timestamp": client_timestamp(
            fields
                .get("observed_at_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from)
                .unwrap_or(anchor.accepted_at_unix_ms),
        ),
        "role": "system",
        "type": "error",
        "final": true,
        "body": {
            "code": "transcript-not-bound",
            "message": format!("transcript not bound: {reason}"),
            "retryable": true,
            "details": details
        }
    })
}

fn managed_codex_transcript(
    state: &AppState,
    owner: &str,
    evidence: Option<&str>,
) -> Result<crate::external_sessions::ExternalSession, Missing> {
    let Some(home) = state.native_session_home.as_deref() else {
        return Err("this daemon has no home directory to read native sessions from".into());
    };
    // The wrapper owns this path; never resolve a path from client input. A reused
    // driver directory is only authoritative when its runtime and a durable
    // observation both name the same exact provider incarnation.
    let root = state
        .state_dir
        .join("drivers")
        .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24]);
    let native = root.join("sessions/codex");
    let directory = if native.exists() {
        native
    } else {
        root.join("state")
    };
    let runtime = std::fs::read(directory.join("runtime.json"))
        .map_err(|_| "the Codex driver has not written its runtime record".to_owned())?;
    let binding = std::fs::read(directory.join("binding.json"))
        .map_err(|_| "the Codex driver has not bound a thread yet".to_owned())?;
    let (Ok(runtime), Ok(binding)) = (
        serde_json::from_slice::<Value>(&runtime),
        serde_json::from_slice::<Value>(&binding),
    ) else {
        return Err("the Codex driver's runtime or binding record is unreadable".into());
    };
    let identity = owner.strip_prefix("agent/").unwrap_or(owner);
    let Some(provider_incarnation) = runtime["incarnation"].as_str() else {
        return Err("the Codex runtime record names no incarnation".into());
    };
    let Some(native_id) = binding["threadId"].as_str() else {
        return Err("the Codex binding names no thread".into());
    };
    if runtime["agent"] != identity
        || binding["agent"] != identity
        || binding["runtimeIncarnation"] != provider_incarnation
    {
        return Err("the Codex binding belongs to a different runtime".into());
    }
    if evidence != Some(provider_incarnation) {
        return Err("the Codex binding is from a different provider incarnation".into());
    }
    // Codex names the rollout after its thread, so look it up directly first; a rollout whose
    // name does not follow that convention is still found by the thread ID inside it.
    let bound = match crate::external_sessions::find_bound_transcript(
        home,
        crate::external_sessions::ExternalDriver::Codex,
        native_id,
    ) {
        Ok(Some(session)) => Some(session),
        _ => crate::external_sessions::discover(Some(home), true)
            .map_err(|error| format!("listing Codex sessions failed: {error:#}"))?
            .sessions
            .into_iter()
            .find(|session| {
                session.driver == crate::external_sessions::ExternalDriver::Codex
                    && session.native_id == native_id
            }),
    };
    bound.ok_or_else(|| {
        Missing::not_yet(format!("Codex thread {native_id} has no rollout file yet"))
    })
}

fn managed_claude_transcript(
    state: &AppState,
    owner: &str,
    evidence: Option<&str>,
) -> Result<crate::external_sessions::ExternalSession, Missing> {
    let Some(home) = state.native_session_home.as_deref() else {
        return Err("this daemon has no home directory to read native sessions from".into());
    };
    let Some(evidence) = evidence else {
        return Err("the Claude driver has not reported which process owns the seat".into());
    };
    let directory = crate::hooks::claude_agent_dir(
        &state.state_dir.join("drivers"),
        owner,
        &st_drivers::run::detect_host(),
    );
    // The current wrapper's SessionStart hook binds the Claude session it started. A previous
    // provider's binding can survive a restart, so it counts only when it names the same
    // provider incarnation as the seat's current observation; neither its presence nor the
    // newest transcript in a workspace is sufficient.
    let hook_binding = std::fs::read(directory.join("claude-native-session"))
        .ok()
        .and_then(|binding| serde_json::from_slice::<Value>(&binding).ok())
        .and_then(|binding| {
            (binding["incarnation"].as_str() == Some(evidence))
                .then(|| binding["native_session_id"].as_str().map(str::to_owned))
                .flatten()
        });
    let native_id = match hook_binding {
        Some(native_id) => native_id,
        // Without the hook, prove the session from the live processes instead; see
        // `claude_session_of_managed_driver` for why that cannot pick another session.
        None => crate::external_sessions::claude_session_of_managed_driver(home, owner, evidence)
            .map_err(|reason| {
            format!("the SessionStart hook did not bind this incarnation, and {reason}")
        })?,
    };
    match crate::external_sessions::find_bound_transcript(
        home,
        crate::external_sessions::ExternalDriver::Claude,
        &native_id,
    ) {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(Missing::not_yet(format!(
            "Claude session {native_id} has no transcript file yet"
        ))),
        Err(error) => Err(format!("finding Claude session {native_id} failed: {error:#}").into()),
    }
}

fn managed_omp_transcript(
    state: &AppState,
    owner: &str,
    incarnation: &str,
) -> Result<crate::external_sessions::ExternalSession, Missing> {
    let started_at = incarnation
        .split_once(':')
        .and_then(|(_, started_at)| chrono::DateTime::parse_from_rfc3339(started_at).ok())
        .ok_or_else(|| {
            format!("the OMP incarnation `{incarnation}` does not carry its start time")
        })?;
    if let Some(claim) = state
        .store
        .latest_claim(owner, Some("harness.session-file"))
        .map_err(|error| format!("reading the OMP session record failed: {error:#}"))?
    {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        if fields["harness"] == "omp"
            && fields["agent"] == owner
            && fields["source_session"].as_str().is_some()
            && let (Some(path), Some(native_id)) =
                (fields["path"].as_str(), fields["session_id"].as_str())
        {
            return match crate::external_sessions::find_imported_omp_transcript(
                Path::new(path),
                native_id,
            ) {
                Ok(Some(session)) => Ok(session),
                Ok(None) => {
                    Err(format!("the imported OMP session {native_id} is not readable").into())
                }
                Err(error) => Err(format!(
                    "reading the imported OMP session {native_id} failed: {error:#}"
                )
                .into()),
            };
        }
    }
    let root = state
        .state_dir
        .join("drivers")
        .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24]);
    let native = root.join("sessions/omp/provider-sessions");
    let directory = if root.join("sessions/omp").exists() {
        native
    } else {
        crate::hooks::legacy_claude_agent_dir(
            &state.state_dir.join("drivers"),
            owner,
            &st_drivers::run::detect_host(),
        )
        .join("provider-sessions")
    };
    match crate::external_sessions::find_managed_omp_transcript(
        &directory,
        (started_at.timestamp_millis().max(0) as u128).saturating_sub(2_000),
    ) {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(Missing::not_yet(
            "OMP has not saved a session for this incarnation yet",
        )),
        Err(error) => Err(format!("reading the OMP session directory failed: {error:#}").into()),
    }
}

fn external_conversation_items(
    conversation: Option<crate::external_sessions::ExternalConversation>,
    session_id: &str,
) -> Result<Vec<Value>, ApiError> {
    match conversation {
        Some(crate::external_sessions::ExternalConversation::Readable(external)) => {
            crate::external_sessions::normalized_timeline(&external).map_err(ApiError::internal)
        }
        Some(crate::external_sessions::ExternalConversation::Unavailable(process)) => Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unsupported-capability".into(),
            message: "This agent was not started by st, and st could not identify its saved session. Its conversation is not available.".into(),
            details: Box::new(serde_json::Map::from_iter([
                ("session_id".into(), json!(process.id)),
                ("reason".into(), json!("native-session-unidentified")),
            ])),
        }),
        None => Err(ApiError::not_found(format!(
            "session `{session_id}` does not exist"
        ))),
    }
}

pub(super) fn timeline_value(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    id: &str,
    query: &ClientListQuery,
) -> Result<Json<Value>, ApiError> {
    require_scope(session, "read.projections")?;
    let session_id = client_detail_id("session", id);
    if query.cursor.is_some() {
        let mut page = client_page(
            state,
            snapshot,
            &format!("timeline/{session_id}"),
            Vec::new(),
            query,
        )?;
        page.items.reverse();
        return Ok(Json(json!({
            "kind": "timeline-page",
            "session_id": session_id,
            "items": page.items,
            "page": page.page
        })));
    }
    let managed = super::managed_session_owner_at(&state.store, snapshot.store_index, &session_id)
        .map_err(ApiError::internal)?;
    let Some((owner, incarnation, _)) = managed else {
        let conversation = crate::external_sessions::find_conversation(
            state.native_session_home.as_deref(),
            &session_id,
        )
        .map_err(ApiError::internal)?;
        let items = external_conversation_items(conversation, &session_id)?;
        return native_timeline_page(state, snapshot, &session_id, query, items);
    };
    let owner = owner.as_str();
    let incarnation = incarnation.as_deref();
    // When the seat's harness keeps a transcript st3 cannot bind or read, the claim timeline
    // below is shown with one entry that says why, never silently in its place.
    let mut transcript_notice_entry = None;
    if let Some(incarnation) = incarnation
        && let Some(managed) = managed_transcript(state, owner, incarnation)?
    {
        let read = managed
            .transcript
            .as_ref()
            .map_err(|missing| missing.reason.clone())
            .and_then(|external| {
                crate::external_sessions::normalized_timeline(external)
                    .map_err(|error| format!("the transcript could not be read: {error:#}"))
            });
        match read {
            Ok(items) => return native_timeline_page(state, snapshot, &session_id, query, items),
            Err(reason) => {
                transcript_notice_entry = Some(transcript_notice(&session_id, &managed, &reason));
            }
        }
    }
    let desired = state.store.desired_subjects().map_err(ApiError::internal)?;
    let attribution = timeline_attribution(owner, &desired);
    let before = snapshot.store_index.checked_add(1);
    let timeline_page = if let Some(incarnation) = incarnation {
        state
            .store
            .timeline_claims_for_incarnation_at(owner, incarnation, before, true, 4_096)
            .map_err(ApiError::internal)?
    } else {
        crate::model::ClaimsPage {
            claims: Vec::new(),
            next_cursor: None,
        }
    };
    let has_older_timeline = timeline_page.next_cursor.is_some();
    let mut timeline_claims = timeline_page.claims;
    timeline_claims.reverse();
    timeline_claims.retain(|claim| {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        incarnation.is_some_and(|expected| {
            fields.get("incarnation_id").and_then(Value::as_str) == Some(expected)
        })
    });
    if !timeline_retention_is_explicit(&timeline_claims, has_older_timeline) {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "timeline-history-incomplete".into(),
            message: "the retained transcript start is incomplete: older history has no truncation interval"
                .into(),
            details: Box::new(serde_json::Map::from_iter([
                ("full_resync".into(), Value::Bool(false)),
                ("retained_history_incomplete".into(), Value::Bool(true)),
            ])),
        });
    }
    let mut retained_entries = BTreeSet::new();
    for claim in &timeline_claims {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let Some(entry_id) = fields.get("entry_id").and_then(Value::as_str) else {
            continue;
        };
        let operation = fields.get("operation").and_then(Value::as_str);
        if !retained_entries.contains(entry_id) && operation != Some("append") {
            return Err(ApiError {
                status: StatusCode::GONE,
                code: "timeline-history-incomplete".into(),
                message: "the retained transcript start is incomplete: an entry's append operation is missing".into(),
                details: Box::new(serde_json::Map::from_iter([
                    ("full_resync".into(), Value::Bool(false)),
                    ("retained_history_incomplete".into(), Value::Bool(true)),
                ])),
            });
        }
        retained_entries.insert(entry_id.to_owned());
    }
    let mut owner_claims = state
        .store
        .claims_page(Some(owner), None, 0, before, true, 10_000)
        .map_err(ApiError::internal)?
        .claims;
    owner_claims.reverse();
    owner_claims.retain(|claim| claim.kind != "harness.timeline");
    let mut message_claims = session_messages(state, owner, &session_id, incarnation, before)?;
    message_claims.reverse();
    let mut claims = timeline_claims;
    claims.extend(owner_claims);
    claims.extend(message_claims);
    claims.sort_by_key(crate::store::claim_log_order);
    claims.dedup_by_key(|claim| claim.id.clone());
    let session_leaf = session_id.trim_start_matches("session/");
    let mut items = Vec::<Value>::new();
    let mut explicit = BTreeMap::<String, usize>::new();
    let mut tool_calls = BTreeSet::<String>::new();
    let entry_id = |claim: &ClaimRecord, suffix: &str| {
        let digest = hex::encode(Sha256::digest(format!("{}:{suffix}", claim.id).as_bytes()));
        format!("timeline-entry/{session_leaf}/{}", &digest[..24])
    };
    let applies_to_incarnation = |fields: &Value| {
        let observed = fields.get("incarnation_id").and_then(Value::as_str);
        observed.is_none() || incarnation.is_none() || observed == incarnation
    };
    for claim in claims {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let timestamp = client_timestamp(
            fields
                .get("observed_at_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from)
                .unwrap_or(claim.accepted_at_unix_ms),
        );
        let base_sequence = claim.store_index.saturating_mul(4);
        if claim.subject == owner && claim.kind == "harness.timeline" {
            if !incarnation.is_some_and(|expected| {
                fields.get("incarnation_id").and_then(Value::as_str) == Some(expected)
            }) {
                continue;
            }
            let Some(operation) = fields.get("operation").and_then(Value::as_str) else {
                continue;
            };
            let Some(id) = fields.get("entry_id").and_then(Value::as_str) else {
                continue;
            };
            let revision = fields.get("revision").and_then(Value::as_u64).unwrap_or(1);
            let role = fields
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("system");
            let entry_type = fields
                .get("entry_type")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let final_entry = fields
                .get("final")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let sequence = fields
                .get("sequence")
                .and_then(Value::as_u64)
                .unwrap_or(base_sequence + 3);
            let mut body = fields.get("body").cloned().unwrap_or_else(|| json!({}));
            if entry_type == "usage" {
                body = normalized_timeline_usage_body(
                    body,
                    fields.get("attribution").unwrap_or(&attribution),
                    fields.get("driver").and_then(Value::as_str),
                );
            }
            let transition_valid = match operation {
                "append" => !explicit.contains_key(id) && revision == 1,
                "replace" | "finalize" => explicit.get(id).is_some_and(|index| {
                    let current = &items[*index];
                    !current["final"].as_bool().unwrap_or(false)
                        && current["revision"].as_u64().unwrap_or(0) + 1 == revision
                        && current["role"] == role
                        && current["type"] == entry_type
                }),
                _ => false,
            };
            let tool_order_valid = if entry_type == "tool_result" {
                body.get("call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|call_id| tool_calls.contains(call_id))
            } else {
                true
            };
            if !transition_valid || !tool_order_valid {
                items.push(json!({
                    "id": entry_id(&claim, "invalid-transition"),
                    "sequence": sequence,
                    "revision": 1,
                    "timestamp": timestamp,
                    "role": "system",
                    "type": "error",
                    "final": true,
                    "body": {
                        "code": "invalid-timeline-transition",
                        "message": format!("driver timeline entry `{id}` has an invalid {operation} transition"),
                        "retryable": false,
                        "details": { "claim_id": claim.id }
                    }
                }));
                continue;
            }
            if entry_type == "tool_call"
                && let Some(call_id) = body.get("call_id").and_then(Value::as_str)
            {
                tool_calls.insert(call_id.to_owned());
            }
            if operation == "append" {
                explicit.insert(id.to_owned(), items.len());
                items.push(json!({
                    "id": id,
                    "sequence": sequence,
                    "revision": revision,
                    "timestamp": timestamp,
                    "role": role,
                    "type": entry_type,
                    "final": final_entry,
                    "body": body
                }));
            } else if let Some(index) = explicit.get(id).copied() {
                let sequence = items[index]["sequence"].clone();
                let original_timestamp = items[index]["timestamp"].clone();
                items[index] = json!({
                    "id": id,
                    "sequence": sequence,
                    "revision": revision,
                    "timestamp": original_timestamp,
                    "role": role,
                    "type": entry_type,
                    "final": operation == "finalize" || final_entry,
                    "body": body
                });
            }
            continue;
        }
        if claim.subject == owner && claim.kind == "runtime.observed" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            let Some(observed) = fields.get("status").and_then(Value::as_str) else {
                continue;
            };
            let status = match observed {
                "pending" | "starting" => "queued",
                "running" | "ready" | "working" => "running",
                "idle" | "blocked" => "waiting",
                "stopped" | "exited" | "absent" => "completed",
                "failed" => "failed",
                "cancelled" => "cancelled",
                _ => continue,
            };
            items.push(json!({
                "id": entry_id(&claim, "runtime-status"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "status",
                "final": true, "body": { "status": status, "detail": observed }
            }));
            continue;
        }
        if claim.subject == owner && claim.kind == "harness.observed" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            let observed = fields
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("indeterminate");
            let status = match observed {
                "starting" => "queued",
                "ready" | "working" => "running",
                "idle" | "blocked" => "waiting",
                "ended" => "completed",
                _ => "failed",
            };
            items.push(json!({
                "id": entry_id(&claim, "harness-status"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "status",
                "final": true, "body": { "status": status, "detail": observed }
            }));
            continue;
        }
        if claim.subject == owner && claim.kind == "harness.diagnostic" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            items.push(json!({
                "id": entry_id(&claim, "diagnostic"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "error",
                "final": true, "body": {
                    "code": fields.get("code").and_then(Value::as_str).unwrap_or("harness-diagnostic"),
                    "message": fields.get("reason").and_then(Value::as_str).unwrap_or("the harness reported a diagnostic"),
                    "retryable": fields.get("severity").and_then(Value::as_str) == Some("warning"),
                    "details": { "severity": fields.get("severity"), "claim_id": claim.id }
                }
            }));
            continue;
        }
        if claim.subject == owner && claim.kind == "harness.usage" {
            if fields.get("semantics").and_then(Value::as_str) == Some("response_rollup") {
                continue;
            }
            if !applies_to_incarnation(fields) {
                continue;
            }
            let mut body = fields.as_object().cloned().unwrap_or_default();
            body.remove("incarnation_id");
            let body = normalized_timeline_usage_body(
                Value::Object(body),
                &attribution,
                fields.get("driver").and_then(Value::as_str),
            );
            items.push(json!({
                "id": entry_id(&claim, "usage"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "usage",
                "final": true, "body": body
            }));
            continue;
        }
        if claim.kind == "message.sent" {
            // Only session_messages put a message here.
            let from = fields.get("from").and_then(Value::as_str);
            let role = if from == Some(owner) {
                "assistant"
            } else {
                "user"
            };
            items.push(json!({
                "id": entry_id(&claim, "message"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": role, "type": "message",
                "final": true, "body": session_message_body(&claim)
            }));
            items.push(json!({
                "id": entry_id(&claim, "content"), "sequence": base_sequence + 1,
                "revision": 1, "timestamp": timestamp, "role": role, "type": "content",
                "final": true, "body": {
                    "media_type": "text/plain",
                    "text": fields.get("content").and_then(Value::as_str).unwrap_or_default()
                }
            }));
        }
    }
    items.extend(transcript_notice_entry);
    items.sort_by_key(|item| item["sequence"].as_u64().unwrap_or(u64::MAX));
    // A conversation opens at its newest bounded window. The cursor walks toward older
    // windows, while each individual page remains chronological for straightforward rendering.
    items.reverse();
    let mut page = client_page(
        state,
        snapshot,
        &format!("timeline/{session_id}"),
        items,
        query,
    )?;
    page.items.reverse();
    Ok(Json(json!({
        "kind": "timeline-page",
        "session_id": session_id,
        "items": page.items,
        "page": page.page
    })))
}

#[derive(Default, Deserialize)]
pub(super) struct ConversationQuery {
    after: Option<String>,
    wait_ms: Option<u64>,
}

fn conversation_cursor(
    state: &AppState,
    session_id: &str,
    store_index: u64,
    local_position: u64,
    native_sequence: u64,
) -> String {
    format!(
        "conversation-cursor/{}/{}/{}.{}.{}",
        state.node,
        session_id.trim_start_matches("session/"),
        store_index,
        local_position,
        native_sequence
    )
}

pub(super) fn conversation_session_id(state: &AppState, id: &str) -> Result<String, ApiError> {
    if id.starts_with("agent/") {
        let status = state
            .store
            .status_for_subject_prefix_at("agent/", None, true)
            .map_err(ApiError::internal)?;
        let subject = status
            .subjects
            .into_iter()
            .find(|subject| subject.subject == id)
            .ok_or_else(|| ApiError::not_found(format!("agent `{id}` does not exist")))?;
        let fields = subject
            .actual
            .as_ref()
            .map(|actual| actual.get("fields").unwrap_or(actual));
        let incarnation = fields
            .and_then(|fields| fields.get("incarnation_id"))
            .and_then(Value::as_str)
            .or(subject.projection.runtime_incarnation.as_deref())
            .or_else(|| {
                fields
                    .and_then(|fields| fields.get("runtime_id"))
                    .and_then(Value::as_str)
            })
            .ok_or_else(|| validation("the agent has no current session"))?;
        return Ok(client_session_id(id, incarnation));
    }
    if id.starts_with("message/") {
        let claim = state
            .store
            .claims_for(id, Some("message.sent"))
            .map_err(ApiError::internal)?
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::not_found(format!("message `{id}` does not exist")))?;
        let session_id = claim
            .body
            .pointer("/fields/session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| validation("the message has no session peer"))?;
        return Ok(session_id.to_owned());
    }
    Ok(client_detail_id("session", id))
}

fn conversation_position(
    state: &AppState,
    session_id: &str,
    after: &str,
) -> Result<(u64, u64, u64), ApiError> {
    let prefix = format!(
        "conversation-cursor/{}/{}/",
        state.node,
        session_id.trim_start_matches("session/")
    );
    after
        .strip_prefix(&prefix)
        .and_then(|part| {
            let mut parts = part.split('.');
            let result = (
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            );
            parts.next().is_none().then_some(result)
        })
        .ok_or_else(|| ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "the conversation cursor belongs to another owner or session".into(),
            details: Box::new(serde_json::Map::from_iter([(
                "full_resync".into(),
                Value::Bool(true),
            )])),
        })
}

fn conversation_read_now(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
    after: Option<&str>,
) -> Result<Value, ApiError> {
    #[cfg(test)]
    if let Ok(mut rebuilds) = timeline_rebuilds().lock() {
        *rebuilds.entry(session_id.to_owned()).or_default() += 1;
    }
    let snapshot = new_client_snapshot(state);
    let page = timeline_value(
        state,
        &snapshot,
        session,
        session_id,
        &ClientListQuery {
            limit: Some(200),
            ..Default::default()
        },
    )?
    .0;
    let all = page["items"]
        .as_array()
        .ok_or_else(|| ApiError::internal("the timeline has no items"))?;
    let native_latest = all
        .iter()
        .filter(|item| {
            item["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("timeline-entry/native-"))
        })
        .filter_map(|item| item["sequence"].as_u64())
        .max()
        .unwrap_or(0);
    let local_latest = state
        .store
        .local_observations_tail(1)
        .map_err(ApiError::internal)?
        .first()
        .and_then(crate::store::local_observation_position)
        .unwrap_or(0);
    let position = after
        .map(|cursor| conversation_position(state, session_id, cursor))
        .transpose()?;
    if let Some((store_index, local_position, native_sequence)) = position {
        if store_index > snapshot.store_index
            || local_position > local_latest
            || native_sequence > native_latest
        {
            return Err(ApiError {
                status: StatusCode::GONE,
                code: "cursor-gap".into(),
                message: "the conversation cursor is outside the bounded replay window".into(),
                details: Box::new(serde_json::Map::from_iter([(
                    "full_resync".into(),
                    Value::Bool(true),
                )])),
            });
        }
    }
    let mut changed_indexes = BTreeSet::new();
    let mut explicit_ids = BTreeSet::new();
    let mut message_indexes = BTreeSet::new();
    if let Some((store_index, local_position, _)) = position {
        let owner = super::managed_session_owner_at(&state.store, snapshot.store_index, session_id)
            .map_err(ApiError::internal)?
            .map(|managed| managed.0);
        if let Some(owner) = owner.as_deref() {
            for claim in state
                .store
                .claims_page(Some(owner), None, store_index, None, false, 10_000)
                .map_err(ApiError::internal)?
                .claims
            {
                changed_indexes.insert(claim.store_index);
                if claim.kind == "harness.timeline" {
                    if let Some(id) = claim
                        .body
                        .pointer("/fields/entry_id")
                        .and_then(Value::as_str)
                    {
                        explicit_ids.insert(id.to_owned());
                    }
                }
            }
        }
        for claim in state
            .store
            .claims_for_kind_at("message.sent", None, true, 10_000)
            .map_err(ApiError::internal)?
            .claims
        {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            if claim.store_index > store_index
                && fields
                    .get("session_id")
                    .and_then(Value::as_str)
                    .is_none_or(|message_session| message_session == session_id)
                && owner.as_deref().is_some_and(|owner| {
                    fields.get("from").and_then(Value::as_str) == Some(owner)
                        || fields.get("to").and_then(Value::as_str) == Some(owner)
                })
            {
                changed_indexes.insert(claim.store_index);
                message_indexes.insert(claim.store_index);
            }
        }
        for claim in state
            .store
            .local_observations_after(local_position, 10_000)
            .map_err(ApiError::internal)?
        {
            if claim.subject == owner.as_deref().unwrap_or_default()
                && claim.kind == "harness.timeline"
            {
                if let Some(id) = claim
                    .body
                    .pointer("/fields/entry_id")
                    .and_then(Value::as_str)
                {
                    explicit_ids.insert(id.to_owned());
                }
            }
        }
    }
    let mut items = all
        .iter()
        .filter(|item| {
            let Some((_, _, native_sequence)) = position else {
                return false;
            };
            let id = item["id"].as_str().unwrap_or_default();
            if id.starts_with("timeline-entry/native-") {
                return item["sequence"]
                    .as_u64()
                    .is_some_and(|sequence| sequence > native_sequence);
            }
            explicit_ids.contains(id)
                || item["sequence"]
                    .as_u64()
                    .is_some_and(|sequence| changed_indexes.contains(&(sequence / 4)))
        })
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        a["timestamp"]
            .as_str()
            .cmp(&b["timestamp"].as_str())
            .then_with(|| a["sequence"].as_u64().cmp(&b["sequence"].as_u64()))
    });
    if message_indexes.iter().any(|index| {
        ![
            index.saturating_mul(4),
            index.saturating_mul(4).saturating_add(1),
        ]
        .iter()
        .all(|sequence| {
            items
                .iter()
                .any(|item| item["sequence"].as_u64() == Some(*sequence))
        })
    }) || explicit_ids
        .iter()
        .any(|id| !items.iter().any(|item| item["id"].as_str() == Some(id)))
        || (all.len() == 200
            && position.is_some_and(|(_, _, native)| {
                all.iter()
                    .find(|item| {
                        item["id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with("timeline-entry/native-"))
                    })
                    .and_then(|item| item["sequence"].as_u64())
                    .is_some_and(|first| first > native)
            }))
    {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "the conversation exceeded its bounded replay window".into(),
            details: Box::new(serde_json::Map::from_iter([(
                "full_resync".into(),
                Value::Bool(true),
            )])),
        });
    }
    Ok(
        json!({"kind":"conversation-changes", "session_id":session_id, "items":items, "next_cursor":conversation_cursor(state, session_id, snapshot.store_index, local_latest, native_latest)}),
    )
}

/// What a conversation read last saw, so a wake-up can tell cheaply whether anything that
/// concerns the conversation changed: a claim about its agent, Small Talk to or from it, a local
/// timeline entry, or its native transcript file.
#[derive(Clone)]
struct ConversationMark {
    owner: Option<String>,
    transcript: Option<std::path::PathBuf>,
    store_index: u64,
    local_position: u64,
    transcript_seen: Option<(u64, std::time::SystemTime)>,
}

/// The cursors this member gave out recently, each with its conversation's transcript as it was
/// then (length and modification time). A long-poll that brings one back can tell, without
/// rebuilding the timeline, that nothing concerning the conversation changed since: idle polls
/// rebuilt a 200-entry timeline every time, 275–620 ms of daemon CPU each (idle-cpu findings).
type TranscriptSeen = Option<(u64, std::time::SystemTime)>;
type HashMap<K, V> = std::collections::HashMap<K, V>;
const ISSUED_CURSORS: usize = 4096;

fn issued_cursors()
-> &'static std::sync::Mutex<(std::collections::VecDeque<String>, HashMap<String, TranscriptSeen>)> {
    static ISSUED: std::sync::OnceLock<
        std::sync::Mutex<(std::collections::VecDeque<String>, HashMap<String, TranscriptSeen>)>,
    > = std::sync::OnceLock::new();
    ISSUED.get_or_init(Default::default)
}

fn remember_cursor(cursor: &str, seen: TranscriptSeen) {
    let Ok(mut issued) = issued_cursors().lock() else {
        return;
    };
    let (order, cursors) = &mut *issued;
    if cursors.insert(cursor.to_owned(), seen).is_none() {
        order.push_back(cursor.to_owned());
        while order.len() > ISSUED_CURSORS {
            if let Some(oldest) = order.pop_front() {
                cursors.remove(&oldest);
            }
        }
    }
}

fn issued_transcript(cursor: &str) -> Option<TranscriptSeen> {
    issued_cursors().lock().ok()?.1.get(cursor).copied()
}

/// How many times each session's timeline was rebuilt for a change read, for the budget test.
#[cfg(test)]
fn timeline_rebuilds() -> &'static std::sync::Mutex<HashMap<String, u64>> {
    static REBUILDS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    REBUILDS.get_or_init(Default::default)
}

fn transcript_seen(path: Option<&std::path::Path>) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path?).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

impl ConversationMark {
    fn new(state: &AppState, session_id: &str) -> Result<Self, ApiError> {
        let index = state.store.index().map_err(ApiError::internal)?;
        let managed = super::managed_session_owner_at(&state.store, index, session_id)
            .map_err(ApiError::internal)?;
        let (owner, incarnation) = managed
            .map(|(owner, incarnation, _)| (Some(owner), incarnation))
            .unwrap_or_default();
        // Resolve the transcript once: finding it walks the harness's session directories.
        let transcript = match (&owner, &incarnation) {
            (Some(owner), Some(incarnation)) => managed_transcript(state, owner, incarnation)?
                .and_then(|managed| managed.transcript.ok()),
            _ => crate::external_sessions::find(state.native_session_home.as_deref(), session_id)
                .map_err(ApiError::internal)?,
        }
        .map(|external| external.transcript);
        Ok(Self {
            transcript_seen: transcript_seen(transcript.as_deref()),
            transcript,
            owner,
            store_index: index,
            local_position: local_latest_position(state)?,
        })
    }

    /// Whether anything that concerns the conversation changed since the last look.
    fn changed(&mut self, state: &AppState) -> Result<bool, ApiError> {
        let mut changed = false;
        let index = state.store.index().map_err(ApiError::internal)?;
        if index > self.store_index {
            let claims = state
                .store
                .claims_page(
                    None,
                    None,
                    self.store_index,
                    index.checked_add(1),
                    false,
                    10_000,
                )
                .map_err(ApiError::internal)?
                .claims;
            // A burst too large to scan is treated as a change.
            changed |= claims.len() >= 10_000
                || claims.iter().any(|claim| {
                    let fields = claim.body.get("fields").unwrap_or(&claim.body);
                    Some(claim.subject.as_str()) == self.owner.as_deref()
                        || (claim.kind == "message.sent"
                            && ["from", "to"].iter().any(|side| {
                                fields.get(*side).and_then(Value::as_str) == self.owner.as_deref()
                            }))
                });
            self.store_index = index;
        }
        let local = local_latest_position(state)?;
        if local > self.local_position {
            changed |= state
                .store
                .local_observations_after(self.local_position, 10_000)
                .map_err(ApiError::internal)?
                .iter()
                .any(|claim| Some(claim.subject.as_str()) == self.owner.as_deref());
            self.local_position = local;
        }
        let seen = transcript_seen(self.transcript.as_deref());
        if seen != self.transcript_seen {
            changed = true;
            self.transcript_seen = seen;
        }
        Ok(changed)
    }
}

fn local_latest_position(state: &AppState) -> Result<u64, ApiError> {
    Ok(state
        .store
        .local_observations_tail(1)
        .map_err(ApiError::internal)?
        .first()
        .and_then(crate::store::local_observation_position)
        .unwrap_or(0))
}

async fn conversation_changes_local(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
    after: Option<&str>,
    wait_ms: u64,
) -> Result<Value, ApiError> {
    let mut changed = state.event_notify.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms.min(30_000));
    let mut mark = ConversationMark::new(state, session_id)?;
    // A cursor this member gave out, with nothing that concerns the conversation changed since:
    // there is nothing to read yet, so wait without rebuilding the timeline.
    let mut quiet = None;
    if let Some(cursor) = after
        && let Some(seen) = issued_transcript(cursor)
        && let Ok((store_index, local_position, _)) =
            conversation_position(state, session_id, cursor)
    {
        let mut since = ConversationMark {
            store_index,
            local_position,
            transcript_seen: seen,
            ..mark.clone()
        };
        if !since.changed(state)? {
            mark = since;
            quiet = Some(json!({"kind":"conversation-changes", "session_id":session_id, "items":[], "next_cursor":cursor}));
        }
    }
    loop {
        // The transcript as the read below will at least see it, for the cursor it returns.
        let seen = mark.transcript_seen;
        let value = match quiet.take() {
            Some(value) => value,
            None => conversation_read_now(state, session, session_id, after)?,
        };
        if let Some(cursor) = value["next_cursor"].as_str() {
            remember_cursor(cursor, seen);
        }
        if !value["items"]
            .as_array()
            .is_some_and(|items| items.is_empty())
            || after.is_none()
            || tokio::time::Instant::now() >= deadline
        {
            return Ok(value);
        }
        // Read again only when something that concerns this conversation changed: a full read
        // parses the whole transcript, and the fleet commits many times a second.
        loop {
            let pause = Duration::from_millis(250)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
            tokio::select! { _ = changed.changed() => {}, _ = tokio::time::sleep(pause) => {} }
            if mark.changed(state)? {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                // Nothing concerned it: move the cursor past what was checked without a read.
                let mut value = value;
                if let Some(cursor) = value["next_cursor"].as_str() {
                    let (_, _, native) = conversation_position(state, session_id, cursor)?;
                    let next = conversation_cursor(
                        state,
                        session_id,
                        mark.store_index,
                        mark.local_position,
                        native,
                    );
                    remember_cursor(&next, mark.transcript_seen);
                    value["next_cursor"] = Value::String(next);
                }
                return Ok(value);
            }
        }
    }
}

pub(super) async fn conversation_changes(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let session_id = conversation_session_id(&state, &id)?;
    let snapshot = new_client_snapshot(&state);
    if let Some((_, _, Some(origin))) =
        super::managed_session_owner_at(&state.store, snapshot.store_index, &session_id)
            .map_err(ApiError::internal)?
    {
        if origin != state.store.origin() {
            if !acting_party(&session) {
                return Err(forbidden(
                    "remote conversation changes require a concrete person or agent",
                ));
            }
            let owner = client_host_id(&origin);
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&owner))?;
            let value = relay
                .read(
                    &owner,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor,
                        relay: None,
                        request: crate::peer::ClientReadOperation::ConversationChanges {
                            session_id,
                            after: query.after,
                            wait_ms: query.wait_ms.unwrap_or(0).min(30_000),
                        },
                    },
                )
                .await
                .map_err(|error| remote_read_error(&owner, error))?;
            return Ok(Json(value));
        }
    }
    conversation_changes_local(
        &state,
        &session,
        &session_id,
        query.after.as_deref(),
        query.wait_ms.unwrap_or(0),
    )
    .await
    .map(Json)
}

pub(super) async fn conversation_stream(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ConversationQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_scope(&session, "read.projections")?;
    let protocols = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(','))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if protocols != [CONVERSATION_SUBPROTOCOL] {
        return Err(validation(
            "the conversation WebSocket requires exactly st3.client.conversation.v0",
        ));
    }
    let session_id = conversation_session_id(&state, &id)?;
    let snapshot = new_client_snapshot(&state);
    let remote = super::managed_session_owner_at(&state.store, snapshot.store_index, &session_id)
        .map_err(ApiError::internal)?
        .and_then(|(_, _, origin)| origin)
        .filter(|origin| origin != state.store.origin())
        .map(|origin| client_host_id(&origin));
    if remote.is_some() && !acting_party(&session) {
        return Err(forbidden(
            "remote conversation stream requires a concrete person or agent",
        ));
    }
    if let Some(owner) = &remote {
        if state
            .client_relay
            .as_ref()
            .is_none_or(|relay| !relay.reaches(owner))
        {
            return Err(remote_unavailable(owner));
        }
    } else {
        conversation_read_now(&state, &session, &session_id, query.after.as_deref())?;
    }
    Ok(websocket
        .protocols([CONVERSATION_SUBPROTOCOL])
        .on_upgrade(move |socket| {
            conversation_stream_socket(socket, state, session, session_id, query.after, remote)
        }))
}

async fn conversation_stream_socket(
    mut socket: WebSocket,
    state: AppState,
    session: ClientSession,
    session_id: String,
    mut after: Option<String>,
    remote: Option<String>,
) {
    loop {
        let after_input = after.clone();
        let read = async {
            if let Some(owner) = &remote {
                let relay = state
                    .client_relay
                    .as_ref()
                    .ok_or_else(|| remote_unavailable(owner))?;
                relay
                    .read(
                        owner,
                        &crate::peer::ClientReadRequest {
                            authority_actor: session.authority_actor.clone(),
                            relay: None,
                            request: crate::peer::ClientReadOperation::ConversationChanges {
                                session_id: session_id.clone(),
                                after: after_input.clone(),
                                wait_ms: 10_000,
                            },
                        },
                    )
                    .await
                    .map_err(|error| remote_read_error(owner, error))
            } else {
                conversation_changes_local(
                    &state,
                    &session,
                    &session_id,
                    after_input.as_deref(),
                    10_000,
                )
                .await
            }
        };
        tokio::pin!(read);
        let value = tokio::select! { value = &mut read => value, message = socket.recv() => { if matches!(message, None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))) { return; } else { continue; } } };
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                close_terminal_stream_with_error(&mut socket, &error).await;
                return;
            }
        };
        let next = value["next_cursor"].as_str().map(str::to_owned);
        if after.is_none()
            || value["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        {
            if !send_terminal_stream_value(&mut socket, &terminal_stream_envelope(&state, value))
                .await
            {
                close_terminal_stream(
                    &mut socket,
                    1009,
                    "conversation update exceeds the client limit",
                )
                .await;
                return;
            }
        }
        after = next;
    }
}

#[derive(Default, Deserialize)]
pub(super) struct EventsQuery {
    after: Option<String>,
    limit: Option<usize>,
    wait_ms: Option<u64>,
}

fn decode_event_cursor(node: &str, cursor: Option<&str>) -> Result<EventCursor, ApiError> {
    let Some(cursor) = cursor else {
        return Ok(EventCursor {
            claim: 0,
            local: None,
        });
    };
    let mut parts = cursor.split('/');
    if parts.next() != Some("event-cursor") || parts.next() != Some(node) {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "the event cursor belongs to another host or retention epoch".into(),
            details: Box::new(serde_json::Map::from_iter([(
                "full_resync".into(),
                Value::Bool(true),
            )])),
        });
    }
    parts
        .next()
        .and_then(|value| match value.split_once('.') {
            Some((claim, local)) => Some(EventCursor {
                claim: claim.parse().ok()?,
                local: Some(local.parse().ok()?),
            }),
            None => Some(EventCursor {
                claim: value.parse().ok()?,
                local: None,
            }),
        })
        .filter(|_| parts.next().is_none())
        .ok_or_else(|| validation("the event cursor is malformed"))
}

fn event_resume_floor(oldest: u64) -> u64 {
    oldest.saturating_sub(1)
}

fn validate_event_cursor(
    node: &str,
    cursor_was_supplied: bool,
    after: u64,
    oldest: u64,
    newest: u64,
) -> Result<(), ApiError> {
    let floor = event_resume_floor(oldest);
    if cursor_was_supplied && (after < floor || after > newest) {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "the event cursor is outside the retained event range".into(),
            details: Box::new(serde_json::Map::from_iter([
                ("full_resync".into(), Value::Bool(true)),
                (
                    "oldest_cursor".into(),
                    Value::String(format!("event-cursor/{node}/{floor}")),
                ),
                (
                    "newest_cursor".into(),
                    Value::String(format!("event-cursor/{node}/{newest}")),
                ),
            ])),
        });
    }
    Ok(())
}

fn client_session_id(owner: &str, incarnation: &str) -> String {
    let digest = hex::encode(Sha256::digest(format!("{owner}:{incarnation}").as_bytes()));
    format!("session/{}", &digest[..24])
}

fn safe_event_projection(state: &AppState, record: &EventRecord) -> (String, Vec<String>, Value) {
    let fields = record.body.get("fields").unwrap_or(&record.body);
    if record.kind == "harness.timeline" {
        let resource_ids = fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .map(|incarnation| vec![client_session_id(&record.subject, incarnation)])
            .unwrap_or_default();
        return (
            "upsert".into(),
            resource_ids,
            json!({ "reason": "session-timeline-invalidated" }),
        );
    }
    if record.kind.starts_with("custom.client.pairing-") {
        return (
            "capabilities.changed".into(),
            Vec::new(),
            json!({ "reason": "authenticated-client-capabilities-changed" }),
        );
    }
    if record.kind.starts_with("custom.client.terminal-") {
        let resource_ids = fields
            .get("terminal_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                state
                    .store
                    .claims_for(&record.subject, Some("custom.client.terminal-attached"))
                    .ok()?
                    .into_iter()
                    .next()?
                    .body
                    .pointer("/fields/terminal_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .map(|terminal| vec![terminal.to_owned()])
            .unwrap_or_default();
        return (
            "upsert".into(),
            resource_ids,
            json!({ "reason": "terminal-viewer-lifecycle-changed" }),
        );
    }
    let mut resource_ids = Vec::new();
    if record.subject.starts_with("message/")
        || record.subject.starts_with("attention/")
        || record.subject.starts_with("agent/")
        || record.subject.starts_with("observer/")
        || record.subject.starts_with("subscription/")
        || record.subject.starts_with("step-run/")
        || record.subject.starts_with("mission/")
    {
        resource_ids.push(record.subject.clone());
    } else if record.subject.starts_with("mission-run/")
        && let Ok(Some(mission)) = state.store.mission_for_run(&record.subject)
    {
        resource_ids.push(mission);
    } else if record.subject.starts_with("planning-session/") {
        resource_ids.push(format!(
            "launch/{}",
            record.subject.trim_start_matches("planning-session/")
        ));
    }
    if record.kind == "message.sent"
        && let Some(session_id) = fields.get("session_id").and_then(Value::as_str)
    {
        resource_ids.push(session_id.to_owned());
    }
    let event_type = if record.kind == "runtime.observed"
        && fields.get("status").and_then(Value::as_str) == Some("running")
        && fields.get("terminal").and_then(Value::as_bool) == Some(true)
    {
        "terminal.available"
    } else {
        "upsert"
    };
    (
        event_type.into(),
        resource_ids,
        json!({
            "reason": "client-projection-invalidated",
            "change": record.kind,
            "subject": record.subject,
            "state": fields.get("state").or_else(|| fields.get("status")).cloned()
        }),
    )
}

pub(super) async fn events(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let (oldest, newest) = state.store.event_bounds().map_err(ApiError::internal)?;
    let after = decode_event_cursor(&state.node, query.after.as_deref())?;
    validate_event_cursor(
        &state.node,
        query.after.is_some(),
        after.claim,
        oldest,
        newest,
    )?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(query.wait_ms.unwrap_or(0).min(30_000));
    // Subscribe before the first store read. An event between reading an empty
    // page and subscribing must wake this long poll, not wait for another event.
    let mut changed = state.event_notify.subscribe();
    let records = loop {
        let records = if query.after.is_some() {
            feed_events_after(&state.store, after, limit.saturating_add(1))
        } else {
            feed_events_tail(&state.store, limit)
        }
        .map_err(ApiError::internal)?;
        if !records.is_empty() || tokio::time::Instant::now() >= deadline {
            break records;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !matches!(
            tokio::time::timeout(remaining, changed.changed()).await,
            Ok(Ok(()))
        ) {
            break Vec::new();
        }
    };
    let has_more = query.after.is_some() && records.len() > limit;
    let records = records.into_iter().take(limit).collect::<Vec<_>>();
    let resume = records
        .last()
        .map(|(record, local)| EventCursor {
            claim: record.store_index,
            local: *local,
        })
        .unwrap_or(after);
    let items = records
        .into_iter()
        .map(|(record, local)| {
            let position = EventCursor {
                claim: record.store_index,
                local,
            };
            let previous = match local {
                Some(local) => EventCursor {
                    claim: record.store_index,
                    local: Some(local.saturating_sub(1)),
                },
                None => EventCursor {
                    claim: record.store_index.saturating_sub(1),
                    local: None,
                },
            };
            let event_snapshot = client_snapshot_at(&state, record.store_index);
            let (event_type, resource_ids, body) = safe_event_projection(&state, &record);
            json!({
                "id": format!("projection-event/{}/{}", state.node, position.label()),
                "epoch": state.node,
                "sequence": record.store_index,
                "previous_cursor": previous.encode(&state.node),
                "next_cursor": position.encode(&state.node),
                "timestamp": event_snapshot.created_at,
                "type": event_type,
                "resource_ids": resource_ids,
                "snapshot_id": event_snapshot.id,
                "body": body
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "kind": "event-page",
        "oldest_cursor": format!("event-cursor/{}/{}", state.node, event_resume_floor(oldest)),
        "resume_cursor": resume.encode(&state.node),
        "items": items,
        "has_more": has_more
    })))
}

/// A position in this node's event feed. Claim events come in store order. A local
/// observation event follows the claim it was written after, so `local` names the last
/// local observation delivered after `claim`; `None` means none of them yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventCursor {
    claim: u64,
    local: Option<u64>,
}

impl EventCursor {
    fn label(&self) -> String {
        match self.local {
            Some(local) => format!("{}.{local}", self.claim),
            None => self.claim.to_string(),
        }
    }

    fn encode(&self, node: &str) -> String {
        format!("event-cursor/{node}/{}", self.label())
    }
}

type FeedEvent = (EventRecord, Option<u64>);

fn local_feed_event(record: ClaimRecord) -> FeedEvent {
    let local = crate::store::local_observation_position(&record);
    (
        EventRecord {
            store_index: record.store_index,
            kind: record.kind,
            subject: record.subject,
            body: record.body,
        },
        local,
    )
}

fn feed_order(event: &FeedEvent) -> (u64, u64) {
    (event.0.store_index, event.1.unwrap_or(0))
}

/// Claim events and local observation events after `after`, oldest first.
fn feed_events_after(
    store: &Store,
    after: EventCursor,
    limit: usize,
) -> anyhow::Result<Vec<FeedEvent>> {
    let local_after = match after.local {
        Some(local) => local,
        None => store.local_observation_floor_after_claim(after.claim)?,
    };
    let mut events = store
        .events_after_bounded(after.claim, limit)?
        .into_iter()
        .map(|record| (record, None))
        .collect::<Vec<_>>();
    events.extend(
        store
            .local_observations_after(local_after, limit)?
            .into_iter()
            .map(local_feed_event),
    );
    events.sort_by_key(feed_order);
    events.truncate(limit);
    Ok(events)
}

/// The newest `limit` claim and local observation events, oldest first.
fn feed_events_tail(store: &Store, limit: usize) -> anyhow::Result<Vec<FeedEvent>> {
    let mut events = store
        .events_tail_bounded(limit)?
        .into_iter()
        .map(|record| (record, None))
        .collect::<Vec<_>>();
    events.extend(
        store
            .local_observations_tail(limit)?
            .into_iter()
            .map(local_feed_event),
    );
    events.sort_by_key(feed_order);
    let excess = events.len().saturating_sub(limit);
    events.drain(..excess);
    Ok(events)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PairingBegin {
    api_version: String,
    device_name: String,
    person_id: String,
    full_control: Option<bool>,
}

pub(super) async fn pairing_begin(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Json(request): Json<PairingBegin>,
) -> Result<Json<Value>, ApiError> {
    if session.transport != "unix" {
        return Err(forbidden("pairing can only begin on the local Unix API"));
    }
    if request.person_id != session.authority_actor {
        return Err(forbidden(
            "the pairing person must match the authenticated Unix person",
        ));
    }
    if request.api_version != CLIENT_API_VERSION
        || request.device_name.trim().is_empty()
        || request.device_name.len() > 120
        || !request.person_id.starts_with("person/")
        || request.person_id.matches('/').count() != 1
    {
        return Err(validation(
            "the pairing request requires a valid version, device name, and concrete initiating person",
        ));
    }
    let mut random = [0_u8; 10];
    getrandom::fill(&mut random).map_err(ApiError::internal)?;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let code = random[..8]
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect::<String>();
    let stable = hex::encode(Sha256::digest(
        format!("{}:{code}", request.device_name).as_bytes(),
    ));
    let pairing_id = format!("pairing/{}", &stable[..24]);
    let subject = format!("custom/client/pairing-{}", &stable[..24]);
    let expires_at = client_now_ms() + 300_000;
    let person_id = request.person_id;
    let scopes = if request.full_control.unwrap_or(false) {
        ALL_SCOPES
    } else {
        LIMITED_PAIRING_SCOPES
    };
    state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "custom.client.pairing-begun".into(),
            actor: Some(person_id.clone()),
            fields: BTreeMap::from([
                ("pairing_id".into(), Value::String(pairing_id.clone())),
                ("device_name".into(), Value::String(request.device_name)),
                ("person_id".into(), Value::String(person_id)),
                ("scopes".into(), json!(scopes)),
                ("code_hash".into(), Value::String(credential_digest(&code))),
                ("expires_at_unix_ms".into(), json!(expires_at)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(
        json!({ "kind": "pairing-challenge", "pairing_id": pairing_id, "code": code, "expires_at": client_timestamp(expires_at) }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PairingComplete {
    api_version: String,
    code: String,
    device_public_key: String,
    /// Where the device keeps its signing key: `secure-enclave` or `software`.
    #[serde(default)]
    key_storage: Option<String>,
}

/// A device's signing key, when its public key is one: `p256:` and the base64url of an
/// uncompressed P-256 point, or a bare base64url Ed25519 key. Anything else is a legacy device
/// that pairs without signing.
pub(super) fn device_signing_key(public_key: &str) -> Option<&str> {
    let decode = |text: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text.as_bytes())
            .ok()
    };
    match public_key.strip_prefix("p256:") {
        Some(point) => decode(point)
            .is_some_and(|point| point.len() == 65 && point[0] == 4)
            .then_some(public_key),
        None => decode(public_key)
            .is_some_and(|key| key.len() == 32)
            .then_some(public_key),
    }
}

pub(super) async fn pairing_complete(
    State(state): State<AppState>,
    Extension(_session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PairingComplete>,
) -> Result<Json<Value>, ApiError> {
    if request.api_version != CLIENT_API_VERSION || request.device_public_key.len() < 32 {
        return Err(validation(
            "the pairing completion has an invalid version or public key",
        ));
    }
    let pairing_id = client_detail_id("pairing", &id);
    let subject = format!("custom/client/pairing-{id}");
    let begun = state
        .store
        .claims_for_subject_kind_at(&subject, "custom.client.pairing-begun", None, true, 1)
        .map_err(ApiError::internal)?
        .claims
        .into_iter()
        .find(|claim| {
            claim
                .body
                .pointer("/fields/pairing_id")
                .and_then(Value::as_str)
                == Some(pairing_id.as_str())
        })
        .ok_or_else(|| ApiError::not_found(format!("pairing `{pairing_id}` does not exist")))?;
    let used = !state
        .store
        .claims_for_subject_kind_at(&subject, "custom.client.pairing-completed", None, true, 1)
        .map_err(ApiError::internal)?
        .claims
        .is_empty();
    let valid_code = begun
        .body
        .pointer("/fields/code_hash")
        .and_then(Value::as_str)
        == Some(credential_digest(&request.code).as_str());
    let expires = begun
        .body
        .pointer("/fields/expires_at_unix_ms")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or_default();
    if used || !valid_code || expires <= client_now_ms() {
        return Err(forbidden(
            "the pairing code is invalid, expired, or already used",
        ));
    }
    let device_public_key = request.device_public_key.clone();
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(ApiError::internal)?;
    let credential = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let device_hash = hex::encode(Sha256::digest(request.device_public_key.as_bytes()));
    let device_id = format!("device/{}", &device_hash[..24]);
    let actor_suffix = &device_hash[..16];
    let person_id = begun
        .body
        .pointer("/fields/person_id")
        .and_then(Value::as_str)
        .filter(|actor| actor.starts_with("person/") && actor.matches('/').count() == 1)
        .ok_or_else(|| forbidden("the pairing has no authenticated concrete person"))?
        .to_owned();
    let session_actor = format!("{person_id}/session/{actor_suffix}");
    let expires_at = client_now_ms() + 30 * 24 * 60 * 60 * 1_000;
    // The concrete scope list was sealed into the authenticated local begin
    // claim. Legacy pending challenges retain their original limited grant.
    let scopes = match begun.body.pointer("/fields/scopes") {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| value.as_str().filter(|scope| ALL_SCOPES.contains(scope)))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| validation("the pairing has invalid delegated scopes"))?,
        None => LIMITED_PAIRING_SCOPES.to_vec(),
        Some(_) => return Err(validation("the pairing has invalid delegated scopes")),
    };
    let completed = state.store.append_claim(&ClaimInput {
        subject: begun.subject.clone(),
        kind: "custom.client.pairing-completed".into(),
        actor: Some(person_id.clone()),
        fields: BTreeMap::from([
            ("pairing_id".into(), Value::String(pairing_id)),
            ("device_id".into(), Value::String(device_id.clone())),
            ("session_actor".into(), Value::String(session_actor.clone())),
            ("person_id".into(), Value::String(person_id.clone())),
            ("delegated_by".into(), Value::String(person_id.clone())),
            (
                "credential_hash".into(),
                Value::String(credential_digest(&credential)),
            ),
            (
                "device_public_key".into(),
                Value::String(device_public_key.clone()),
            ),
            ("scopes".into(), json!(scopes)),
            ("delegated_scopes".into(), json!(scopes)),
            ("expires_at_unix_ms".into(), json!(expires_at)),
        ]),
        evidence: vec![begun.id.clone()],
        expected_subject: Some(Some(begun.id.clone())),
        idempotency_key: None,
    });
    if let Err(error) = completed {
        if error.code == "stale-subject" {
            return Err(forbidden(
                "the pairing code is invalid, expired, or already used",
            ));
        }
        return Err(ApiError::bad(error));
    }
    // A device with a real key, paired to send messages, is enrolled: the person's root key
    // grants it as a device key. A device paired only to read gets no key that speaks for the
    // person, so a wall display can never sign as them.
    let signs = scopes.contains(&"control.messages");
    let chain = match device_signing_key(&device_public_key).filter(|_| signs) {
        Some(key) => {
            let name = begun
                .body
                .pointer("/fields/device_name")
                .and_then(Value::as_str)
                .unwrap_or("device");
            let storage = match request.key_storage.as_deref() {
                Some("secure-enclave") => " (secure enclave)",
                Some("software") => " (software key)",
                _ => "",
            };
            Some(
                state
                    .store
                    .enroll_device_key(&person_id, key, &format!("{name}{storage}"))
                    .map_err(ApiError::bad)?,
            )
        }
        None => None,
    };
    signal_changed(&state);
    let mut session = json!({ "kind": "paired-session", "device_id": device_id, "person_id": person_id, "session_actor": session_actor, "credential": credential, "scopes": scopes, "expires_at": client_timestamp(expires_at) });
    if let Some(chain) = chain {
        session["device_key_chain"] = json!(chain);
    }
    Ok(Json(session))
}

fn terminal_subject(id: &str) -> String {
    id.strip_prefix("terminal/").unwrap_or(id).to_owned()
}

fn terminal_live_session(
    state: &AppState,
    subject: &str,
    expected_incarnation: Option<&str>,
) -> Result<LiveSession, ApiError> {
    live_session(state, subject, expected_incarnation).map_err(|error| {
        if error.code == "stale-incarnation" {
            stale(error.message)
        } else {
            error
        }
    })
}

// The gateway issues its own attachment so the capability remains bound to the
// authenticated session here. The owner rechecks the runtime on every signed read.
fn remote_terminal_live_session(
    state: &AppState,
    subject: &str,
    expected_incarnation: &str,
) -> Result<LiveSession, ApiError> {
    let status = state
        .store
        .status(Some(subject))
        .map_err(ApiError::internal)?;
    let selected = status
        .subjects
        .first()
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no live session")))?;
    if !matches!(selected.reachability.as_str(), "reachable" | "local") {
        return Err(terminal_unavailable("the terminal owner is not reachable"));
    }
    let origin = selected
        .actual_origin
        .as_deref()
        .ok_or_else(|| terminal_unavailable("the terminal owner is unknown"))?;
    if origin == state.store.origin() {
        return terminal_live_session(state, subject, Some(expected_incarnation));
    }
    let owner_host_id = client_host_id(origin);
    if state
        .client_relay
        .as_ref()
        .is_none_or(|relay| !relay.reaches(&owner_host_id))
    {
        return Err(remote_unavailable(&owner_host_id));
    }
    let actual = selected
        .actual
        .as_ref()
        .ok_or_else(|| ApiError::not_found("the terminal has no runtime"))?;
    let fields = actual.get("fields").unwrap_or(actual);
    if fields.get("status").and_then(Value::as_str) != Some("running") {
        return Err(ApiError::not_found("the terminal is not running"));
    }
    let incarnation = fields
        .get("incarnation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found("the terminal has no incarnation"))?;
    if incarnation != expected_incarnation {
        return Err(stale("the terminal incarnation fence is stale"));
    }
    let runtime_id = fields
        .get("runtime_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found("the terminal has no runtime ID"))?;
    Ok(LiveSession {
        runtime_id: runtime_id.into(),
        incarnation_id: incarnation.into(),
        owner_host_id,
        terminal: fields
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        driver: None,
    })
}

/// How long a terminal attach capability stays valid, in milliseconds. A projected-screen
/// capability is a lease: it opens any number of streams until it expires, is detached, or the
/// runtime incarnation changes, so a client that reconnects reuses it instead of attaching again.
const TERMINAL_ATTACHMENT_TTL_MS: u128 = 300_000;
/// How long a viewer waits for a terminal's first screen before giving up.
const TERMINAL_FIRST_SCREEN_TIMEOUT: Duration = Duration::from_secs(5);
/// A gateway's owner long poll stays inside the peer relay's request deadline.
const TERMINAL_RELAY_WAIT_MS: u64 = 10_000;
/// The most often an open terminal stream rechecks its runtime incarnation fence.
const TERMINAL_FENCE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

fn terminal_unavailable(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "terminal-unavailable".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn terminal_view_error(end: terminal_view::ViewEnd) -> ApiError {
    match end {
        terminal_view::ViewEnd::Unavailable(message) => terminal_unavailable(message),
        terminal_view::ViewEnd::Idle => terminal_unavailable("the terminal viewer became idle"),
        terminal_view::ViewEnd::Exited => ApiError {
            status: StatusCode::GONE,
            code: "terminal-ended".into(),
            message: "the terminal session ended".into(),
            details: Box::default(),
        },
    }
}

/// Read the owner-local screen. With `after`, wait up to `wait` for a screen whose revision
/// differs, then return the current screen either way.
async fn terminal_screen_value(
    state: &AppState,
    id: &str,
    expected_incarnation: Option<&str>,
    after: Option<&str>,
    wait: Duration,
) -> Result<Value, ApiError> {
    let subject = terminal_subject(id);
    let live = terminal_live_session(state, &subject, expected_incarnation)?;
    if !live.terminal {
        return Err(validation(
            "the requested runtime does not expose a terminal",
        ));
    }
    let mut screens =
        terminal_view::subscribe(&state.pty_root, &live.runtime_id, &live.incarnation_id);
    let first = tokio::time::Instant::now() + TERMINAL_FIRST_SCREEN_TIMEOUT;
    let mut screen = terminal_view::next_screen(&mut screens, None, first)
        .await
        .map_err(terminal_view_error)?
        .ok_or_else(|| ApiError::internal("the terminal screen did not arrive"))?;
    if let Some(after) = after
        && screen.revision() == after
    {
        let deadline = tokio::time::Instant::now() + wait;
        if let Some(changed) = terminal_view::next_screen(&mut screens, Some(after), deadline)
            .await
            .map_err(terminal_view_error)?
        {
            screen = changed;
        }
    }
    Ok(screen.value(&client_detail_id("terminal", id), &live.incarnation_id))
}

#[derive(Default, Deserialize)]
pub(super) struct TerminalScreenQuery {
    after: Option<String>,
    wait_ms: Option<u64>,
    /// Also return the owner's best-effort session facts.
    #[serde(default)]
    facts: bool,
}

/// The most tags and the longest tag text a screen's facts carry.
const TERMINAL_FACTS_TAGS: usize = 32;
const TERMINAL_FACTS_TAG_BYTES: usize = 256;
/// How long the owner waits for a session to report its stats before leaving facts out.
const TERMINAL_FACTS_TIMEOUT: Duration = Duration::from_secs(1);

/// The session's geometry, clients, uptime and tags as its owner sees them. Best effort: a
/// session that does not answer in time has no facts, never a failed screen. Nothing here
/// decides admission; the fences on actions still do.
async fn terminal_facts(state: &AppState, id: &str) -> Option<Value> {
    let live = terminal_live_session(state, &terminal_subject(id), None).ok()?;
    let root = state.pty_root.clone();
    tokio::task::spawn_blocking(move || {
        let stats = pty_client::stats::query_stats_in_with_timeout(
            &root,
            &live.runtime_id,
            TERMINAL_FACTS_TIMEOUT,
        )
        .ok()?;
        let tags = pty_core::registry::read_metadata_in(&root, &live.runtime_id)
            .and_then(|metadata| metadata.tags)
            .unwrap_or_default()
            .into_iter()
            .filter(|(key, value)| {
                key.len() <= TERMINAL_FACTS_TAG_BYTES && value.len() <= TERMINAL_FACTS_TAG_BYTES
            })
            .take(TERMINAL_FACTS_TAGS)
            .collect::<BTreeMap<_, _>>();
        let mut process = json!({ "alive": stats.process.alive });
        if let Some(code) = stats.process.exit_code {
            process["exit_code"] = json!(code);
        }
        Some(json!({
            "rows": stats.terminal.rows,
            "columns": stats.terminal.cols,
            "clients": {
                "total": stats.clients.total,
                "attached": stats.clients.attached,
                "read_only": stats.clients.read_only,
            },
            "process": process,
            "uptime_s": stats.uptime_seconds.and_then(|seconds| u64::try_from(seconds).ok()),
            "tags": tags,
        }))
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn terminal_screen(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<TerminalScreenQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "terminal.read")?;
    let wait_ms = query.wait_ms.unwrap_or(0).min(30_000);
    match terminal_screen_value(
        &state,
        &id,
        None,
        query.after.as_deref(),
        Duration::from_millis(wait_ms),
    )
    .await
    {
        Ok(mut screen) => {
            if query.facts
                && let Some(facts) = terminal_facts(&state, &id).await
            {
                screen["facts"] = facts;
            }
            Ok(Json(screen))
        }
        Err(error) if error.code == "runtime-not-local" => {
            let subject = terminal_subject(&id);
            let status = state
                .store
                .status(Some(&subject))
                .map_err(ApiError::internal)?;
            let host = status
                .subjects
                .first()
                .and_then(|subject| subject.actual_origin.as_deref())
                .map(client_host_id)
                .ok_or_else(|| stale("the terminal owner is not known"))?;
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&host))?;
            if !acting_party(&session) {
                return Err(forbidden(
                    "remote terminal screen requires a concrete person or agent",
                ));
            }
            let terminal_id = client_detail_id("terminal", &id);
            let request = match query.after {
                Some(after_revision) => crate::peer::ClientReadOperation::TerminalScreenChange {
                    terminal_id,
                    after_revision,
                    wait_ms: wait_ms.min(TERMINAL_RELAY_WAIT_MS),
                    facts: query.facts,
                },
                None => crate::peer::ClientReadOperation::TerminalScreen {
                    terminal_id,
                    facts: query.facts,
                },
            };
            let (mut value, provenance) = relay
                .read_traced(
                    &host,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor.clone(),
                        relay: None,
                        request,
                    },
                )
                .await
                .map_err(|error| remote_read_error(&host, error))?;
            value["relay"] = json!({
                "owner_host_id": provenance.owner_host_id,
                "via": provenance.via,
                "direct": provenance.direct,
                "transport": provenance.transport,
                "rtt_ms": provenance.rtt_ms,
                "capability_ttl_s": TERMINAL_ATTACHMENT_TTL_MS / 1_000,
                "fence_conflicts": provenance.fence_conflicts,
            });
            Ok(Json(value))
        }
        Err(error) => Err(error),
    }
}

#[derive(Default, Deserialize)]
pub(super) struct TerminalStreamQuery {
    incarnation: Option<String>,
}

pub(super) async fn terminal_stream(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<TerminalStreamQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_scope(&session, "terminal.read")?;
    let protocols = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
        .collect::<Vec<_>>();
    let normative_count = protocols
        .iter()
        .filter(|protocol| **protocol == TERMINAL_SUBPROTOCOL)
        .count();
    let secondary = protocols
        .iter()
        .filter(|protocol| **protocol != TERMINAL_SUBPROTOCOL)
        .copied()
        .collect::<Vec<_>>();
    let stream_capability = secondary
        .first()
        .and_then(|protocol| protocol.strip_prefix(TERMINAL_CAPABILITY_PROTOCOL_PREFIX))
        .filter(|capability| !capability.is_empty());
    if normative_count != 1 || secondary.len() != 1 || stream_capability.is_none() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "validation-failed".into(),
            message: format!(
                "the terminal WebSocket must request exactly `{TERMINAL_SUBPROTOCOL}` plus one `st3.cap.*` capability protocol"
            ),
            details: Box::default(),
        });
    }
    let follow = prepare_terminal_follow(
        &state,
        &session,
        &id,
        query.incarnation.as_deref(),
        stream_capability,
    )?;
    Ok(websocket
        .protocols([TERMINAL_SUBPROTOCOL])
        .on_upgrade(move |socket| follow.run(state, TerminalSink::Socket(Box::new(socket)))))
}

/// A terminal viewer, checked and holding its consumed attachment, ready to follow.
enum TerminalFollow {
    Local {
        id: String,
        incarnation: String,
    },
    Remote {
        id: String,
        owner: String,
        authority_actor: String,
        incarnation: String,
    },
}

impl TerminalFollow {
    async fn run(self, state: AppState, sink: TerminalSink) {
        match self {
            Self::Local { id, incarnation } => {
                terminal_stream_socket(sink, state, id, incarnation).await;
            }
            Self::Remote {
                id,
                owner,
                authority_actor,
                incarnation,
            } => {
                remote_terminal_stream_socket(sink, state, id, owner, authority_actor, incarnation)
                    .await;
            }
        }
    }
}

/// Check a viewer's right to follow a terminal and consume its single-use attachment.
fn prepare_terminal_follow(
    state: &AppState,
    session: &ClientSession,
    id: &str,
    incarnation: Option<&str>,
    capability: Option<&str>,
) -> Result<TerminalFollow, ApiError> {
    require_scope(session, "terminal.read")?;
    let subject = terminal_subject(id);
    let live = match incarnation {
        Some(incarnation) => remote_terminal_live_session(state, &subject, incarnation)?,
        None => terminal_live_session(state, &subject, None)?,
    };
    if !live.terminal {
        return Err(validation(
            "the requested runtime does not expose a terminal",
        ));
    }
    if live.owner_host_id != client_host_id(&state.node) && !acting_party(session) {
        return Err(forbidden(
            "remote terminal stream requires a concrete person or agent",
        ));
    }
    consume_terminal_attachment(
        state,
        session,
        &client_detail_id("terminal", id),
        &live.incarnation_id,
        capability,
    )?;
    Ok(if live.owner_host_id != client_host_id(&state.node) {
        TerminalFollow::Remote {
            id: id.to_owned(),
            owner: live.owner_host_id,
            authority_actor: session.authority_actor.clone(),
            incarnation: live.incarnation_id,
        }
    } else {
        TerminalFollow::Local {
            id: id.to_owned(),
            incarnation: live.incarnation_id,
        }
    })
}

/// The latest thing a terminal subscription has to say: whole screens replace each other, so
/// a slow client skips to the newest one.
#[derive(Clone)]
enum TerminalFrame {
    Waiting,
    /// A screen envelope, as the dedicated terminal socket sends it.
    Screen(Value),
    /// The error envelope that ended the stream.
    Ended(Value),
}

/// Where a followed terminal's screens go: its own WebSocket, or one subscription on a
/// client's collection socket.
enum TerminalSink {
    Socket(Box<WebSocket>),
    Subscription(watch::Sender<TerminalFrame>),
}

impl TerminalSink {
    async fn send(&mut self, value: &Value) -> bool {
        match self {
            Self::Socket(socket) => send_terminal_stream_value(socket, value).await,
            Self::Subscription(sender) => {
                serde_json::to_vec(value)
                    .is_ok_and(|bytes| bytes.len() <= CLIENT_MAX_RESPONSE_BYTES)
                    && sender.send(TerminalFrame::Screen(value.clone())).is_ok()
            }
        }
    }

    async fn close(&mut self, code: u16, reason: &str) {
        match self {
            Self::Socket(socket) => close_terminal_stream(socket, code, reason).await,
            Self::Subscription(sender) => {
                sender.send_replace(TerminalFrame::Ended(terminal_stream_error(
                    &ApiError::internal(reason.to_owned()),
                )));
            }
        }
    }

    /// End with one error: an error envelope and a close frame, or the subscription's end.
    async fn fail(&mut self, error: &ApiError) {
        match self {
            Self::Socket(socket) => close_terminal_stream_with_error(socket, error).await,
            Self::Subscription(sender) => {
                sender.send_replace(TerminalFrame::Ended(terminal_stream_error(error)));
            }
        }
    }

    /// Resolves once nobody watches any more.
    async fn gone(&mut self) {
        match self {
            Self::Socket(socket) => loop {
                if matches!(
                    socket.recv().await,
                    None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))
                ) {
                    return;
                }
            },
            Self::Subscription(sender) => sender.closed().await,
        }
    }
}

/// Relay a terminal another host owns. Each owner long poll returns as soon as the owner
/// publishes a screen whose revision differs from the last one sent, so an idle terminal
/// costs one request per relay wait and sends the client nothing.
async fn remote_terminal_stream_socket(
    mut sink: TerminalSink,
    state: AppState,
    id: String,
    owner: String,
    authority_actor: String,
    incarnation: String,
) {
    let Some(relay) = state.client_relay.as_ref() else {
        sink.fail(&remote_unavailable(&owner)).await;
        return;
    };
    let terminal_id = client_detail_id("terminal", &id);
    let mut sent: Option<String> = None;
    let mut failures = 0_u32;
    loop {
        let request = crate::peer::ClientReadRequest {
            authority_actor: authority_actor.clone(),
            relay: None,
            request: match sent.clone() {
                Some(after_revision) => crate::peer::ClientReadOperation::TerminalScreenChange {
                    terminal_id: terminal_id.clone(),
                    after_revision,
                    wait_ms: TERMINAL_RELAY_WAIT_MS,
                    facts: false,
                },
                None => crate::peer::ClientReadOperation::TerminalScreen {
                    terminal_id: terminal_id.clone(),
                    facts: false,
                },
            },
        };
        let read = relay.read(&owner, &request);
        tokio::pin!(read);
        let read = tokio::select! {
            read = &mut read => read,
            () = sink.gone() => return,
        };
        let screen = match read {
            Ok(screen) => screen,
            Err(error) => {
                let error = remote_read_error(&owner, error);
                if error.code == "remote-unavailable" && failures < 3 {
                    failures += 1;
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
                    continue;
                }
                sink.fail(&error).await;
                return;
            }
        };
        failures = 0;
        if screen["runtime_incarnation"].as_str() != Some(incarnation.as_str()) {
            sink.fail(&stale("the terminal incarnation fence is stale"))
                .await;
            return;
        }
        let Some(revision) = screen["revision"].as_str().map(str::to_owned) else {
            sink.fail(&ApiError::internal(
                "the terminal owner does not publish screen revisions",
            ))
            .await;
            return;
        };
        if sent.as_deref() == Some(revision.as_str()) {
            continue;
        }
        if !sink.send(&terminal_stream_envelope(&state, screen)).await {
            sink.close(1009, "terminal screen exceeds the client limit")
                .await;
            return;
        }
        sent = Some(revision);
    }
}

fn terminal_attachment_subject(id: &str) -> Result<String, ApiError> {
    id.strip_prefix("terminal-attachment/")
        .filter(|suffix| !suffix.is_empty() && !suffix.contains('/'))
        .map(|suffix| format!("custom/client/terminal-attachment-{suffix}"))
        .ok_or_else(|| validation("the terminal attachment ID is invalid"))
}

fn terminal_attachment_id(session: &ClientSession, request: &ActionRequest) -> String {
    let stable = hex::encode(Sha256::digest(
        format!("{}:{}", session.actor, request.idempotency_key).as_bytes(),
    ));
    format!("terminal-attachment/{}", &stable[..24])
}

fn existing_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
    request_digest: &str,
) -> Result<Option<Value>, ApiError> {
    let attachment_id = terminal_attachment_id(session, request);
    let subject = terminal_attachment_subject(&attachment_id)?;
    let claims = state
        .store
        .claims_for(&subject, None)
        .map_err(ApiError::internal)?;
    let Some(attached) = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.terminal-attached")
    else {
        return Ok(None);
    };
    let old_digest = attached
        .body
        .pointer("/fields/request_digest")
        .and_then(Value::as_str);
    let legacy_digest = hex::encode(Sha256::digest(
        serde_json::to_vec(request).map_err(ApiError::internal)?,
    ));
    if old_digest != Some(request_digest) && old_digest != Some(legacy_digest.as_str()) {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "idempotency-conflict".into(),
            message: "the idempotency key was already used for a different action".into(),
            details: Box::default(),
        });
    }
    terminal_attachment_response(state, session, &attachment_id).map(Some)
}

fn terminal_capability_key(state: &AppState) -> Result<Vec<u8>, ApiError> {
    use std::io::{Read as _, Write as _};

    let path = state.state_dir.join("client-terminal.key");
    fn read_valid(path: &Path) -> Result<Vec<u8>, ApiError> {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(ApiError::internal)?;
        let metadata = file.metadata().map_err(ApiError::internal)?;
        if !metadata.file_type().is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(ApiError::internal(
                "the terminal capability key must be a daemon-owned 0600 regular file",
            ));
        }
        let mut key = Vec::with_capacity(32);
        file.read_to_end(&mut key).map_err(ApiError::internal)?;
        (key.len() == 32)
            .then_some(key)
            .ok_or_else(|| ApiError::internal("the terminal capability key is invalid"))
    }

    if path.exists() {
        return read_valid(&path);
    }
    fs::create_dir_all(&state.state_dir).map_err(ApiError::internal)?;
    let mut key = vec![0_u8; 32];
    getrandom::fill(&mut key).map_err(ApiError::internal)?;
    let mut staged = tempfile::Builder::new()
        .prefix(".client-terminal.key.")
        .tempfile_in(&state.state_dir)
        .map_err(ApiError::internal)?;
    staged
        .as_file_mut()
        .write_all(&key)
        .map_err(ApiError::internal)?;
    staged
        .as_file_mut()
        .sync_all()
        .map_err(ApiError::internal)?;
    match staged.persist_noclobber(&path) {
        Ok(_) => {
            fs::File::open(&state.state_dir)
                .and_then(|directory| directory.sync_all())
                .map_err(ApiError::internal)?;
            Ok(key)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => read_valid(&path),
        Err(error) => Err(ApiError::internal(error.error)),
    }
}

fn derive_terminal_capability(
    state: &AppState,
    session_actor: &str,
    attachment_id: &str,
    owner_host_id: &str,
) -> Result<String, ApiError> {
    let key = terminal_capability_key(state)?;
    // HMAC-SHA256 (RFC 2104) over the session-bound deterministic attachment identity.
    let mut block = [0_u8; 64];
    block[..key.len()].copy_from_slice(&key);
    let mut inner_key = [0x36_u8; 64];
    let mut outer_key = [0x5c_u8; 64];
    for index in 0..64 {
        inner_key[index] ^= block[index];
        outer_key[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(session_actor.as_bytes());
    inner.update([0]);
    inner.update(attachment_id.as_bytes());
    inner.update([0]);
    inner.update(owner_host_id.as_bytes());
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(outer.finalize()))
}

fn terminal_attachment_response(
    state: &AppState,
    session: &ClientSession,
    attachment_id: &str,
) -> Result<Value, ApiError> {
    let subject = terminal_attachment_subject(attachment_id)?;
    let claims = state
        .store
        .claims_for(&subject, None)
        .map_err(ApiError::internal)?;
    let attached = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.terminal-attached")
        .ok_or_else(|| ApiError::internal("the terminal attachment claim is missing"))?;
    if attached.origin != state.store.origin() {
        return Err(forbidden(
            "the terminal attachment belongs to another gateway",
        ));
    }
    let field = |name: &str| attached.body.pointer(&format!("/fields/{name}"));
    if field("session_actor").and_then(Value::as_str) != Some(session.actor.as_str()) {
        return Err(forbidden(
            "the terminal attachment belongs to another authenticated session",
        ));
    }
    let owner_host_id = field("owner_host_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no owner host"))?;
    if owner_host_id != client_host_id(&state.node)
        && state
            .client_relay
            .as_ref()
            .is_none_or(|relay| !relay.reaches(owner_host_id))
    {
        return Err(remote_unavailable(owner_host_id));
    }
    let latest = claims
        .last()
        .ok_or_else(|| ApiError::internal("the terminal attachment has no head"))?;
    let expires = field("expires_at_unix_ms")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or_default();
    let state_name = if latest.kind == "custom.client.terminal-detached" {
        "detached"
    } else if latest.kind == "custom.client.terminal-consumed" {
        "consumed"
    } else if expires <= client_now_ms() {
        "expired"
    } else {
        "available"
    };
    let capability = if state_name == "available" {
        Some(derive_terminal_capability(
            state,
            &session.actor,
            attachment_id,
            owner_host_id,
        )?)
    } else {
        None
    };
    let terminal_id = field("terminal_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no terminal ID"))?;
    let incarnation = field("runtime_incarnation")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no incarnation"))?;
    let routed = terminal_id.trim_start_matches("terminal/");
    Ok(json!({
        "attachment_id": attachment_id,
        "terminal_id": terminal_id,
        "runtime_incarnation": incarnation,
        "owner_host_id": owner_host_id,
        "stream_url": format!(
            "/v1/client/terminals/{}/stream?incarnation={}",
            urlencoding::encode(routed),
            urlencoding::encode(incarnation)
        ),
        "stream_capability": capability,
        "state": state_name,
        "expires_at": client_timestamp(expires),
        "reusable": true,
        "ttl_s": TERMINAL_ATTACHMENT_TTL_MS / 1_000,
        "retry_hint": if state_name == "available" { Value::Null } else { json!("reattach") },
    }))
}

fn create_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
    request_digest: &str,
) -> Result<Value, ApiError> {
    let target = parameter_string(&request.parameters, "target_id")?;
    let terminal_id = client_detail_id("terminal", &target);
    let incarnation = request
        .fence
        .runtime_incarnation
        .as_deref()
        .ok_or_else(|| validation("terminal attach requires a runtime incarnation fence"))?;
    let live = remote_terminal_live_session(state, &terminal_subject(&target), incarnation)?;
    if !live.terminal {
        return Err(validation("terminal attach requires a terminal runtime"));
    }
    // One idempotency key names exactly one attachment. The request digest is
    // persisted separately so a conflicting reuse cannot create an orphan on
    // another subject before the action receipt is published.
    let attachment_id = terminal_attachment_id(session, request);
    let subject = terminal_attachment_subject(&attachment_id)?;
    if let Some(existing) = existing_terminal_attachment(state, session, request, request_digest)? {
        return Ok(existing);
    }
    let capability =
        derive_terminal_capability(state, &session.actor, &attachment_id, &live.owner_host_id)?;
    let digest = credential_digest(&capability);
    let expires_at = client_now_ms() + TERMINAL_ATTACHMENT_TTL_MS;
    let appended = state.store.append_claim(&ClaimInput {
        subject,
        kind: "custom.client.terminal-attached".into(),
        actor: Some(session_claim_actor(session)),
        fields: BTreeMap::from([
            ("attachment_id".into(), Value::String(attachment_id.clone())),
            ("terminal_id".into(), Value::String(terminal_id.clone())),
            (
                "runtime_incarnation".into(),
                Value::String(incarnation.to_owned()),
            ),
            (
                "owner_host_id".into(),
                Value::String(live.owner_host_id.clone()),
            ),
            ("session_actor".into(), Value::String(session.actor.clone())),
            (
                "request_digest".into(),
                Value::String(request_digest.to_owned()),
            ),
            ("capability_hash".into(), Value::String(digest)),
            ("expires_at_unix_ms".into(), json!(expires_at)),
        ]),
        evidence: Vec::new(),
        expected_subject: Some(None),
        idempotency_key: None,
    });
    if let Err(error) = appended {
        if error.code != "stale-subject" {
            return Err(ApiError::bad(error));
        }
        let winner = state
            .store
            .claims_for(&terminal_attachment_subject(&attachment_id)?, None)
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|claim| claim.kind == "custom.client.terminal-attached")
            .ok_or_else(|| ApiError::internal("attachment CAS lost without a winning claim"))?;
        if winner
            .body
            .pointer("/fields/request_digest")
            .and_then(Value::as_str)
            != Some(request_digest)
        {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "idempotency-conflict".into(),
                message: "the idempotency key was already used for a different action".into(),
                details: Box::default(),
            });
        }
    }
    signal_changed(state);
    terminal_attachment_response(state, session, &attachment_id)
}

fn consume_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    terminal_id: &str,
    incarnation: &str,
    capability: Option<&str>,
) -> Result<(), ApiError> {
    consume_terminal_attachment_mode(state, session, terminal_id, incarnation, capability, None)
}

fn consume_terminal_attachment_mode(
    state: &AppState,
    session: &ClientSession,
    terminal_id: &str,
    incarnation: &str,
    capability: Option<&str>,
    raw_mode: Option<&str>,
) -> Result<(), ApiError> {
    let capability = capability
        .filter(|value| !value.is_empty())
        .ok_or_else(|| forbidden("a terminal stream capability is required"))?;
    let digest = credential_digest(capability);
    let claims = state
        .store
        .claims_page(None, None, 0, None, true, 100_000)
        .map_err(ApiError::internal)?;
    let attached = claims
        .claims
        .iter()
        .find(|claim| {
            claim.kind == "custom.client.terminal-attached"
                && claim
                    .body
                    .pointer("/fields/capability_hash")
                    .and_then(Value::as_str)
                    == Some(digest.as_str())
        })
        .ok_or_else(|| forbidden("the terminal stream capability is unknown"))?;
    let latest = claims
        .claims
        .iter()
        .filter(|claim| claim.subject == attached.subject)
        .max_by_key(|claim| claim.store_index)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no head"))?;
    let field = |name: &str| attached.body.pointer(&format!("/fields/{name}"));
    let raw_live = raw_mode.map(|_| {
        remote_terminal_live_session(state, &terminal_subject(terminal_id), incarnation)
    }).transpose()?;
    let valid = latest.id == attached.id
        && attached.origin == state.store.origin()
        && field("session_actor").and_then(Value::as_str) == Some(session.actor.as_str())
        && field("raw_mode").and_then(Value::as_str) == raw_mode
        && raw_mode.is_none_or(|_| {
            field("person_id").and_then(Value::as_str) == Some(session.authority_actor.as_str())
        })
        && raw_live.as_ref().is_none_or(|live| {
            field("owner_host_id").and_then(Value::as_str) == Some(live.owner_host_id.as_str())
                && field("runtime_id").and_then(Value::as_str) == Some(live.runtime_id.as_str())
        })
        && field("owner_host_id")
            .and_then(Value::as_str)
            .is_some_and(|owner| {
                owner == client_host_id(&state.node)
                    || state
                        .client_relay
                        .as_ref()
                        .is_some_and(|relay| relay.reaches(owner))
            })
        && field("terminal_id").and_then(Value::as_str) == Some(terminal_id)
        && field("runtime_incarnation").and_then(Value::as_str) == Some(incarnation)
        && field("expires_at_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .is_some_and(|expires| expires > client_now_ms());
    if !valid {
        return Err(forbidden(
            "the terminal stream capability is expired, consumed, detached, or belongs to another session",
        ));
    }
    if raw_mode.is_none() {
        // A projected-screen capability is a lease and stays valid for more streams.
        return Ok(());
    }
    state
        .store
        .append_claim(&ClaimInput {
            subject: attached.subject.clone(),
            kind: "custom.client.terminal-consumed".into(),
            actor: Some(session_claim_actor(session)),
            fields: BTreeMap::from([(
                "attachment_id".into(),
                field("attachment_id")
                    .cloned()
                    .ok_or_else(|| ApiError::internal("terminal attachment ID is missing"))?,
            )]),
            evidence: vec![attached.id.clone()],
            expected_subject: Some(Some(attached.id.clone())),
            idempotency_key: None,
        })
        .map_err(|_| forbidden("the terminal stream capability was already consumed"))?;
    signal_changed(state);
    Ok(())
}

fn detach_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<String, ApiError> {
    let attachment_id = parameter_string(&request.parameters, "target_id")?;
    let subject = terminal_attachment_subject(&attachment_id)?;
    let claims = state
        .store
        .claims_for(&subject, None)
        .map_err(ApiError::internal)?;
    let attached = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.terminal-attached")
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "terminal attachment `{attachment_id}` does not exist"
            ))
        })?;
    if attached.origin != state.store.origin() {
        return Err(forbidden(
            "the terminal attachment belongs to another gateway",
        ));
    }
    let session_actor = attached
        .body
        .pointer("/fields/session_actor")
        .and_then(Value::as_str);
    let incarnation = attached
        .body
        .pointer("/fields/runtime_incarnation")
        .and_then(Value::as_str);
    if session_actor != Some(session.actor.as_str()) {
        return Err(forbidden(
            "the terminal attachment belongs to another authenticated session",
        ));
    }
    if incarnation != request.fence.runtime_incarnation.as_deref() {
        return Err(stale("the terminal attachment incarnation fence is stale"));
    }
    let latest = claims
        .last()
        .ok_or_else(|| ApiError::internal("the terminal attachment has no head"))?;
    if latest.kind == "custom.client.terminal-detached" {
        return Ok(attachment_id);
    }
    state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "custom.client.terminal-detached".into(),
            actor: Some(session_claim_actor(session)),
            fields: BTreeMap::from([(
                "attachment_id".into(),
                Value::String(attachment_id.clone()),
            )]),
            evidence: vec![latest.id.clone()],
            expected_subject: Some(Some(latest.id.clone())),
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(attachment_id)
}

fn terminal_stream_envelope(state: &AppState, value: Value) -> Value {
    json!({
        "api_version": CLIENT_API_VERSION,
        "request_id": format!("request/{}", new_request_id()),
        "snapshot": new_client_snapshot(state),
        "value": value,
    })
}

fn terminal_stream_error(error: &ApiError) -> Value {
    json!({
        "api_version": CLIENT_API_VERSION,
        "error_version": "st3.client.error.v0",
        "request_id": format!("request/{}", new_request_id()),
        "code": client_error_code(Some(&error.code)),
        "message": error.message,
        "retryable": client_error_retryable(error.status, Some(&error.code)),
        "details": error.details,
    })
}

async fn send_terminal_stream_value(socket: &mut WebSocket, value: &Value) -> bool {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return false;
    };
    if bytes.len() > CLIENT_MAX_RESPONSE_BYTES {
        return false;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };
    socket.send(WsMessage::Text(text.into())).await.is_ok()
}

async fn close_terminal_stream(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.to_owned().into(),
        })))
        .await;
}

/// End a stream with one error envelope, then a close frame whose reason is the error code.
async fn close_terminal_stream_with_error(socket: &mut WebSocket, error: &ApiError) {
    let envelope = terminal_stream_error(error);
    let _ = send_terminal_stream_value(socket, &envelope).await;
    let code = envelope["code"].as_str().unwrap_or("internal").to_owned();
    let close = if code == "internal" { 1011 } else { 1008 };
    close_terminal_stream(socket, close, &code).await;
}

/// Hold an owner-local terminal stream open. The first message is the current screen; every
/// later message is a newer screen that replaces it. The shared watcher publishes changes at a
/// capped rate, and a viewer busy sending reads only the latest screen when it is ready.
async fn terminal_stream_socket(
    mut sink: TerminalSink,
    state: AppState,
    id: String,
    expected_incarnation: String,
) {
    let subject = terminal_subject(&id);
    let live = match terminal_live_session(&state, &subject, Some(&expected_incarnation)) {
        Ok(live) => live,
        Err(error) => {
            sink.fail(&error).await;
            return;
        }
    };
    let terminal_id = client_detail_id("terminal", &id);
    let mut graph = state.event_notify.subscribe();
    let mut screens =
        terminal_view::subscribe(&state.pty_root, &live.runtime_id, &live.incarnation_id);
    let first = tokio::time::Instant::now() + TERMINAL_FIRST_SCREEN_TIMEOUT;
    let mut screen = match terminal_view::next_screen(&mut screens, None, first).await {
        Ok(Some(screen)) => Some(screen),
        Ok(None) => {
            sink.fail(&terminal_unavailable("the terminal screen did not arrive"))
                .await;
            return;
        }
        Err(end) => {
            sink.fail(&terminal_view_error(end)).await;
            return;
        }
    };
    let mut sent = None::<String>;
    let mut fence_check_at = None::<tokio::time::Instant>;
    let mut last_fence_check = tokio::time::Instant::now();
    loop {
        if screen.is_none() {
            screen = tokio::select! {
                changed = screens.changed() => {
                    if changed.is_err() {
                        sink.fail(&terminal_view_error(terminal_view::ViewEnd::Exited),
                        )
                        .await;
                        return;
                    }
                    let latest = screens.borrow_and_update().clone();
                    match latest {
                        terminal_view::ViewState::Screen(screen) => Some(screen),
                        terminal_view::ViewState::Ended(end) => {
                            sink.fail(&terminal_view_error(end))
                                .await;
                            return;
                        }
                        terminal_view::ViewState::Connecting => None,
                    }
                }
                changed = graph.changed(), if fence_check_at.is_none() => {
                    if changed.is_ok() {
                        let earliest = last_fence_check + TERMINAL_FENCE_CHECK_INTERVAL;
                        fence_check_at = Some(earliest.max(tokio::time::Instant::now()));
                    }
                    None
                }
                _ = tokio::time::sleep_until(fence_check_at.unwrap_or_else(tokio::time::Instant::now)),
                    if fence_check_at.is_some() =>
                {
                    fence_check_at = None;
                    last_fence_check = tokio::time::Instant::now();
                    if let Err(error) =
                        terminal_live_session(&state, &subject, Some(&expected_incarnation))
                    {
                        sink.fail(&error).await;
                        return;
                    }
                    None
                }
                () = sink.gone() => return,
            };
        }
        let Some(screen) = screen.take() else {
            continue;
        };
        if sent.as_deref() == Some(screen.revision()) {
            continue;
        }
        let value = screen.value(&terminal_id, &live.incarnation_id);
        if !sink.send(&terminal_stream_envelope(&state, value)).await {
            sink.close(1009, "terminal screen exceeds the client limit")
                .await;
            return;
        }
        sent = Some(screen.revision().to_owned());
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fence {
    snapshot_id: String,
    #[serde(default)]
    subject_revisions: BTreeMap<String, String>,
    mission_generation: Option<String>,
    step_definition: Option<String>,
    attempt: Option<u32>,
    readiness_epoch: Option<u64>,
    runtime_incarnation: Option<String>,
    runtime_desired_revision: Option<String>,
    terminal_sequence: Option<u64>,
    preview_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionRequest {
    api_version: String,
    id: String,
    #[serde(rename = "type")]
    action_type: String,
    idempotency_key: String,
    fence: Fence,
    parameters: Value,
}

fn action_scope(action: &str) -> Option<&'static str> {
    if matches!(
        action,
        "agent.create" | "agent.stop" | "agent.start" | "agent.suspend" | "agent.resume"
    ) {
        return Some("control.runtimes");
    }
    if action == "work.done" {
        return Some("control.attention");
    }
    Some(match action.split_once('.')?.0 {
        "attention" => "control.attention",
        "review" => "control.attention",
        "message" => "control.messages",
        "launch" => "control.launches",
        "mission" => "control.missions",
        "session" => "control.missions",
        "work" => "control.work",
        "agent" => "control.work",
        "lane" => "control.work",
        "runtime" => "control.runtimes",
        "terminal" => {
            if matches!(action, "terminal.attach" | "terminal.detach") {
                "terminal.read"
            } else {
                "terminal.control"
            }
        }
        "pairing" => "control.pairing",
        _ => return None,
    })
}

fn parameter_string(parameters: &Value, key: &str) -> Result<String, ApiError> {
    parameters
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| validation(format!("action parameters require `{key}`")))
}

fn validate_message_session(
    state: &AppState,
    snapshot: &ClientSnapshot,
    recipient: &str,
    session_id: &str,
) -> Result<(), ApiError> {
    let recipient = normalize_message_party(recipient);
    let current_session = client_agent_resources(
        &state.store,
        false,
        &snapshot.created_at,
        snapshot.store_index,
    )
    .map_err(ApiError::internal)?
    .into_iter()
    .find(|agent| agent["id"] == recipient)
    .and_then(|agent| agent["current_session_id"].as_str().map(str::to_owned))
    .ok_or_else(|| {
        validation(format!(
            "message recipient `{recipient}` has no current normalized session"
        ))
    })?;
    if current_session != session_id {
        return Err(stale(format!(
            "session `{session_id}` is not the current session for `{recipient}`"
        )));
    }
    Ok(())
}

fn import_lookup_error(error: anyhow::Error) -> ApiError {
    if error.is::<crate::external_sessions::AmbiguousSession>() {
        ApiError::bad(St3Error::new("ambiguous-import-session", error.to_string()))
    } else {
        ApiError::internal(error)
    }
}

async fn import_external_session_action(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Vec<String>, ApiError> {
    let target = parameter_string(&request.parameters, "target_id")?;
    let external =
        crate::external_sessions::find_fresh(state.native_session_home.as_deref(), &target)
            .map_err(import_lookup_error)?
            .ok_or_else(|| {
                ApiError::not_found(format!("external session `{target}` does not exist"))
            })?;
    if request.fence.subject_revisions.get(&target) != Some(&external.revision) {
        return Err(stale("the native session changed before import"));
    }
    if external.process.is_none() {
        let discovery =
            crate::external_sessions::discover_fresh(state.native_session_home.as_deref(), false)
                .map_err(ApiError::internal)?;
        if discovery.unresolved_processes.iter().any(|candidate| {
            candidate.driver == external.driver
                && candidate.process.cwd.as_ref() == external.cwd.as_ref()
        }) {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "ambiguous-running-session".into(),
                message: "a running harness in this workspace does not expose its exact native session ID; stop it before importing the saved session".into(),
                details: Box::new(serde_json::Map::from_iter([(
                    "target_id".into(),
                    Value::String(target),
                )])),
            });
        }
    }

    let import = crate::external_sessions::import_seat(&external).map_err(|error| {
        ApiError::bad(St3Error::new("invalid-import-session", error.to_string()))
    })?;
    let seat_intent = parse_intent(&import.kdl, &state.node).map_err(ApiError::bad)?;
    let seat_preview = state
        .store
        .mission(
            &seat_intent,
            IntentInput {
                kdl: import.kdl.clone(),
                source_name: Some(format!("session import seat {target}")),
            },
        )
        .map_err(ApiError::bad)?;
    if !seat_preview.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-import-seat",
            seat_preview.blockers.join("; "),
        )));
    }

    // Fenced takeover is deliberately stop -> declare -> start. The graph intent is fully parsed
    // and authorized before the predecessor is touched, but the durable seat is not made desired
    // until the exact native process has stopped, so two harnesses never own one native session.
    if let Some(process) = external.process.clone() {
        let driver = external.driver;
        tokio::task::spawn_blocking(move || {
            crate::external_sessions::terminate_exact_process(driver, &process)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    }
    state
        .store
        .apply_as(
            &seat_intent,
            &seat_preview.subject_tokens,
            &format!("{}:seat", request.idempotency_key),
            Some(&session.authority_actor),
        )
        .map_err(ApiError::bad)?;
    state
        .store
        .append_claim(&ClaimInput {
            subject: import.subject.clone(),
            kind: "harness.session-file".into(),
            actor: Some(session_claim_actor(session)),
            fields: BTreeMap::from([
                (
                    "harness".into(),
                    Value::String(external.driver.as_str().into()),
                ),
                (
                    "path".into(),
                    Value::String(external.transcript.to_string_lossy().into_owned()),
                ),
                (
                    "session_id".into(),
                    Value::String(external.native_id.clone()),
                ),
                ("source_session".into(), Value::String(external.id.clone())),
                (
                    "discovery_revision".into(),
                    Value::String(external.revision.clone()),
                ),
                ("agent".into(), Value::String(import.subject.clone())),
                ("status".into(), Value::String("unknown".into())),
                (
                    "modified_at".into(),
                    Value::String(crate::external_sessions::timestamp(
                        external.updated_at_unix_ms,
                    )),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("{}:native-session", request.idempotency_key)),
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(vec![external.id, import.subject])
}

fn contains_identity_selector(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "actor" | "credential" | "fleet_secret" | "shared_secret"
            ) || contains_identity_selector(value)
        }),
        Value::Array(values) => values.iter().any(contains_identity_selector),
        _ => false,
    }
}

fn validate_work_fence(state: &AppState, target: &str, fence: &Fence) -> Result<(), ApiError> {
    let target = client_detail_id("step-run", target);
    let work = state
        .store
        .step_run(&target)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("work `{target}` does not exist")))?;
    if fence.mission_generation.as_deref() != Some(work.generation.as_str())
        || fence.step_definition.as_deref() != Some(work.definition_hash.as_str())
        || fence.attempt != Some(work.attempt)
        || fence.readiness_epoch != Some(u64::from(work.readiness_epoch))
        || fence
            .runtime_incarnation
            .as_deref()
            .is_some_and(|incarnation| {
                // A ready step has no lease incarnation yet. Claim dispatch separately
                // checks the caller's current runtime before acquiring the lease.
                work.claim_incarnation
                    .as_deref()
                    .is_some_and(|claimed| claimed != incarnation)
            })
    {
        return Err(stale(format!(
            "the execution fence for `{}` is stale",
            work.subject
        )));
    }
    Ok(())
}

fn runtime_control_target(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Value, ApiError> {
    let target = parameter_string(&request.parameters, "target_id")?;
    let runtime = runtime_resources(state, true, snapshot, session)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|runtime| runtime["id"] == target || runtime["owner_id"] == target)
        .ok_or_else(|| ApiError::not_found(format!("runtime `{target}` does not exist")))?;
    let expected_incarnation = request
        .fence
        .runtime_incarnation
        .as_deref()
        .ok_or_else(|| validation("runtime control requires an incarnation fence"))?;
    let expected_desired = request
        .fence
        .runtime_desired_revision
        .as_deref()
        .ok_or_else(|| validation("runtime control requires a desired revision fence"))?;
    if runtime["incarnation_id"].as_str() != Some(expected_incarnation)
        || runtime["desired_revision"].as_str() != Some(expected_desired)
    {
        return Err(stale("the runtime incarnation or desired revision changed"));
    }
    Ok(runtime)
}

async fn apply_runtime_control_intent(
    state: &AppState,
    snapshot: &ClientSnapshot,
    request: &ActionRequest,
    actor: &str,
    kdl: String,
) -> Result<Vec<String>, ApiError> {
    let intent = IntentInput {
        kdl,
        source_name: Some(format!("client {}", request.action_type)),
    };
    let preview = mission(
        State(state.clone()),
        Json(MissionRequest {
            intent,
            at_index: Some(snapshot.store_index),
        }),
    )
    .await?
    .0;
    if !preview.blockers.is_empty() {
        return Err(validation(preview.blockers.join("; ")));
    }
    let result = apply(
        State(state.clone()),
        Json(ApplyRequest {
            intent: preview.resolved_intent,
            expected_subjects: preview.subject_tokens,
            idempotency_key: format!("{}:runtime-intent", request.idempotency_key),
            actor: Some(actor.to_owned()),
        }),
    )
    .await?
    .0;
    Ok(result.reconcile_subjects)
}

fn validate_launch_fence(state: &AppState, target: &str, fence: &Fence) -> Result<(), ApiError> {
    let launch_id = launch_session_id(target);
    let launch = state
        .store
        .planning_session(launch_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("launch `{target}` does not exist")))?;
    let resource_id = format!("launch/{}", launch.id);
    let expected = format!("launch/{}", launch.updated_at_unix_ms);
    if fence.subject_revisions.get(&resource_id) != Some(&expected) {
        return Err(stale(format!(
            "the launch revision fence for `{resource_id}` is stale"
        )));
    }
    Ok(())
}

fn validate_fence(state: &AppState, fence: &Fence) -> Result<(), ApiError> {
    let parsed = fence
        .snapshot_id
        .strip_prefix("snapshot/")
        .and_then(|value| value.rsplit_once('/'))
        .and_then(|(host_and_index, _fingerprint)| host_and_index.rsplit_once('/'));
    let current_index = state.store.index().map_err(ApiError::internal)?;
    let expected_host = state.node.replace(char::is_whitespace, "-");
    if !parsed.is_some_and(|(host, index)| {
        host == expected_host
            && index
                .parse::<u64>()
                .ok()
                .is_some_and(|index| index <= current_index)
    }) {
        return Err(stale(
            "the client snapshot belongs to another host or a future store position",
        ));
    }
    for (subject, revision) in &fence.subject_revisions {
        let current = if subject.starts_with("attention/") {
            client_attention_resources(&state.store, None, false)
                .map_err(ApiError::internal)?
                .into_iter()
                .find(|item| item["id"] == *subject)
                .and_then(|item| item["revision"].as_str().map(str::to_owned))
        } else if let Some(id) = subject.strip_prefix("launch/") {
            state
                .store
                .planning_session(id)
                .map_err(ApiError::internal)?
                .map(|launch| format!("launch/{}", launch.updated_at_unix_ms))
        } else if subject.starts_with("session/external-") {
            // A native session st3 does not own has no claims; its revision is its discovery.
            crate::external_sessions::find_fresh(state.native_session_home.as_deref(), subject)
                .map_err(import_lookup_error)?
                .map(|session| session.revision)
        } else {
            state
                .store
                .claims_for(subject, None)
                .map_err(ApiError::internal)?
                .last()
                .map(|claim| claim.id.clone())
        };
        if current.as_deref() != Some(revision) {
            return Err(stale(format!(
                "the revision fence for `{subject}` is stale"
            )));
        }
    }
    Ok(())
}

// Creation uses a session-scoped key in both the declaration and apply receipt. A retried
// dispatch after a lost action receipt recovers the existing member, rather than publishing twice.
fn creation_key(session: &ClientSession, request: &ActionRequest) -> String {
    hex::encode(Sha256::digest(format!(
        "{}:{}",
        session.actor, request.idempotency_key
    )))
}
fn require_creation_actor(session: &ClientSession) -> Result<&str, ApiError> {
    if !acting_party(session) {
        return Err(forbidden(
            "creation requires the session's concrete person or local agent",
        ));
    }
    Ok(&session.authority_actor)
}

fn creation_string(value: &str, field: &str, max: usize) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        return Err(validation(format!(
            "{field} must be nonempty, without NUL, and at most {max} bytes"
        )));
    }
    Ok(())
}
fn existing_creation(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Option<String>, ApiError> {
    let key = creation_key(session, request);
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&request.parameters).map_err(ApiError::internal)?,
    ));
    for desired in state.store.desired_subjects().map_err(ApiError::internal)? {
        if let Some(member) = &desired.member
            && member.tags.get("st3.client.create-key") == Some(&key)
        {
            if member.tags.get("st3.client.create-parameters") != Some(&digest) {
                return Err(ApiError {
                    status: StatusCode::CONFLICT,
                    code: "idempotency-conflict".into(),
                    message: "creation key already used for different parameters".into(),
                    details: Box::default(),
                });
            }
            return Ok(Some(if request.action_type == "terminal.create" {
                format!("terminal/{}", desired.subject)
            } else {
                desired.subject
            }));
        }
    }
    Ok(None)
}
async fn publish_creation(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    request: &ActionRequest,
    kdl: String,
    kind: &str,
) -> Result<String, ApiError> {
    let mut document: kdl::KdlDocument = kdl
        .parse()
        .map_err(|error: kdl::KdlError| validation(error.to_string()))?;
    let node = document
        .nodes_mut()
        .iter_mut()
        .find(|node| node.name().value() == kind)
        .unwrap();
    let tags = node
        .children_mut()
        .as_mut()
        .unwrap()
        .nodes_mut()
        .iter_mut()
        .find(|node| node.name().value() == "tags")
        .unwrap();
    tags.entries_mut().push(kdl::KdlEntry::new_prop(
        "st3.client.create-parameters",
        hex::encode(Sha256::digest(
            serde_json::to_vec(&request.parameters).map_err(ApiError::internal)?,
        )),
    ));
    document.autoformat();
    let kdl = document.to_string();
    let intent = crate::graph::parse_intent(&kdl, &state.node).map_err(ApiError::bad)?;
    let subject = intent
        .subjects
        .values()
        .find(|subject| subject.kind == kind)
        .ok_or_else(|| validation("creation declares no member"))?
        .subject
        .clone();
    let key = creation_key(session, request);
    if let Some(existing) = state
        .store
        .desired_subjects()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|member| member.subject == subject)
    {
        if existing
            .member
            .as_ref()
            .and_then(|member| member.tags.get("st3.client.create-key"))
            == Some(&key)
            && existing.desired
                == intent
                    .subjects
                    .values()
                    .find(|member| member.subject == subject)
                    .unwrap()
                    .desired
        {
            return Ok(subject);
        }
        if kind != "agent" || existing.kind != "stop" {
            return Err(validation(format!(
                "{subject} already exists; use a different name or idempotency key"
            )));
        }
    }
    let mut scoped = request.clone();
    scoped.idempotency_key = format!("client-create:{key}");
    apply_runtime_control_intent(state, snapshot, &scoped, &session.authority_actor, kdl).await?;
    Ok(subject)
}
async fn create_agent(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Vec<String>, ApiError> {
    require_creation_actor(session)?;
    let mut parameters: st3_client::AgentCreateParameters =
        serde_json::from_value(request.parameters.clone())
            .map_err(|error| validation(error.to_string()))?;
    creation_string(&parameters.name, "name", 160)?;
    if !crate::skill::HARNESSES.contains(&parameters.harness.as_str()) {
        return Err(validation("unknown harness"));
    }
    for (field, value) in [
        ("model", &parameters.model),
        ("effort", &parameters.effort),
        ("host", &parameters.host),
        ("description", &parameters.description),
    ] {
        if let Some(value) = value {
            creation_string(value, field, 4096)?;
        }
    }
    if let Some(message) = &parameters.message {
        creation_string(message, "message", 65536)?;
    }
    if let Some(id) = existing_creation(state, session, request)? {
        return Ok(vec![id]);
    }
    let host = parameters
        .host
        .as_deref()
        .unwrap_or(&state.node)
        .trim_start_matches("host/")
        .to_owned();
    let host = if host == "local" {
        state.node.clone()
    } else {
        host
    };
    creation_string(&host, "host", 160)?;
    parameters.host = Some(host.clone());
    let workspace = if let Some(workspace) = &parameters.workspace {
        workspace.clone()
    } else if host == state.node {
        crate::config::default_agent_workspace(&parameters.name)
            .map_err(|error| validation(error.to_string()))?
            .display()
            .to_string()
    } else {
        let relay = state
            .client_relay
            .as_ref()
            .filter(|relay| relay.reaches(&client_host_id(&host)))
            .ok_or_else(|| remote_unavailable(&client_host_id(&host)))?;
        let value = relay
            .read(
                &client_host_id(&host),
                &crate::peer::ClientReadRequest {
                    authority_actor: session.authority_actor.clone(),
                    relay: None,
                    request: crate::peer::ClientReadOperation::AgentWorkspace {
                        identity: parameters.name.clone(),
                    },
                },
            )
            .await
            .map_err(|error| remote_read_error(&client_host_id(&host), error))?;
        value["workspace"]
            .as_str()
            .ok_or_else(|| ApiError::internal("host returned no workspace"))?
            .to_owned()
    };
    creation_string(&workspace, "workspace", 4096)?;
    if !std::path::Path::new(&workspace).is_absolute() {
        return Err(validation(
            "workspace must be absolute on the selected host",
        ));
    }
    let kdl = crate::creation::agent_document(
        &parameters,
        &workspace,
        true,
        Some(&creation_key(session, request)),
    );
    Ok(vec![
        publish_creation(state, snapshot, session, request, kdl, "agent").await?,
    ])
}
async fn create_terminal(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Vec<String>, ApiError> {
    let person = require_creation_actor(session)?;
    let parameters: st3_client::TerminalCreateParameters =
        serde_json::from_value(request.parameters.clone())
            .map_err(|error| validation(error.to_string()))?;
    creation_string(&parameters.name, "name", 160)?;
    if let Some(id) = existing_creation(state, session, request)? {
        return Ok(vec![id]);
    }
    let host = parameters
        .host
        .as_deref()
        .unwrap_or(&state.node)
        .trim_start_matches("host/");
    creation_string(host, "host", 160)?;
    let cwd = parameters.cwd.as_deref().unwrap_or(".");
    creation_string(cwd, "cwd", 4096)?;
    if cwd != "." && !std::path::Path::new(cwd).is_absolute() {
        return Err(validation("cwd must be absolute on the selected host"));
    }
    let key = creation_key(session, request);
    let mut bytes: [u8; 16] = hex::decode(&key[..32]).unwrap().try_into().unwrap();
    bytes[6] = (bytes[6] & 15) | 0x80; // UUIDv8: stable, session-scoped creation identity.
    bytes[8] = (bytes[8] & 63) | 0x80;
    let id = uuid::Uuid::from_bytes(bytes).to_string();
    let kdl = crate::creation::terminal_document(person, &id, &parameters, host, cwd, &key);
    let subject = publish_creation(state, snapshot, session, request, kdl, "pty").await?;
    Ok(vec![format!("terminal/{subject}")])
}

async fn dispatch_action(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Vec<String>, ApiError> {
    let p = &request.parameters;
    let authority_actor = &session.authority_actor;
    match request.action_type.as_str() {
        "agent.create" => create_agent(state, snapshot, session, request).await,
        "terminal.create" => create_terminal(state, snapshot, session, request).await,
        "terminal.end" => {
            let person = require_creation_actor(session)?;
            let subject = terminal_subject(&parameter_string(p, "target_id")?);
            if st3_schema::owned_terminals::owner(&subject)
                .map_err(|error| validation(error.message))?
                != Some(person)
            {
                return Err(forbidden("only the terminal's creator may end it"));
            }
            if state
                .store
                .selected_desired_token(&subject)
                .map_err(ApiError::internal)?
                .is_none()
            {
                return Err(ApiError::not_found("terminal does not exist"));
            }
            let kdl = format!(
                "version 2\nstop {}\n",
                serde_json::to_string(&subject).map_err(ApiError::internal)?
            );
            let mut scoped = request.clone();
            scoped.idempotency_key =
                format!("client-terminal-end:{}", creation_key(session, request));
            apply_runtime_control_intent(state, snapshot, &scoped, person, kdl).await?;
            Ok(vec![format!("terminal/{subject}")])
        }
        decision @ ("review.approve" | "review.reject" | "review.request-changes") => {
            let target = parameter_string(p, "target_id")?;
            let result = post_review(
                State(state.clone()),
                AxumPath(target),
                Json(ReviewRequest {
                    decision: match decision {
                        "review.approve" => "approved",
                        "review.request-changes" => "changes-requested",
                        _ => "rejected",
                    }
                    .into(),
                    reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                    actor: Some(authority_actor.clone()),
                    expected_subject: None,
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "attention.resolve" => Err(ApiError::bad(St3Error::new(
            "attention-migrated",
            "attention is a view; complete or remedy its source",
        ))),
        "work.ask" => {
            if let Some(step) = p.get("step_id").and_then(Value::as_str) {
                validate_work_fence(state, step, &request.fence)?;
            }
            let result = state
                .store
                .ask_person(&PersonAskRequest {
                    legacy_request: None,
                    person: parameter_string(p, "person_id")?,
                    title: parameter_string(p, "title")?,
                    reason: parameter_string(p, "reason")?,
                    actor: authority_actor.clone(),
                    step: p
                        .get("step_id")
                        .map(|_| parameter_string(p, "step_id"))
                        .transpose()?,
                    new_run: p
                        .get("new_run")
                        .map(|_| parameter_string(p, "new_run"))
                        .transpose()?,
                    incarnation: request.fence.runtime_incarnation.clone(),
                    idempotency_key: request.idempotency_key.clone(),
                    request: p.get("request").cloned(),
                })
                .map_err(ApiError::bad)?;
            signal_changed(state);
            Ok(vec![result.subject])
        }
        action @ ("work.done" | "work.cancel-ask") => {
            let target = parameter_string(p, "target_id")?;
            let result = state
                .store
                .finish_person_step(
                    &PersonStepResponse {
                        subject: target,
                        actor: authority_actor.clone(),
                        summary: parameter_string(p, "summary")?,
                        evidence: p
                            .get("evidence")
                            .map(|value| {
                                serde_json::from_value::<Vec<String>>(value.clone()).map_err(|_| {
                                    ApiError::bad(St3Error::new(
                                        "validation-failed",
                                        "evidence must be an array of strings",
                                    ))
                                })
                            })
                            .transpose()?
                            .unwrap_or_default(),
                        episode: Some(parameter_string(p, "episode")?),
                        idempotency_key: request.idempotency_key.clone(),
                        answer: p
                            .get("answer")
                            .map(|value| {
                                serde_json::from_value(value.clone()).map_err(|_| {
                                    ApiError::bad(St3Error::new(
                                        "validation-failed",
                                        "answer takes an optional id and optional text",
                                    ))
                                })
                            })
                            .transpose()?,
                    },
                    action == "work.cancel-ask",
                )
                .map_err(|error| {
                    if error.code == "forbidden" {
                        forbidden(error.message)
                    } else {
                        ApiError::bad(error)
                    }
                })?;
            signal_changed(state);
            Ok(vec![result.subject])
        }
        "message.send" => {
            let to = parameter_string(p, "to")?;
            let session_id = p
                .get("session_id")
                .map(|_| parameter_string(p, "session_id"))
                .transpose()?;
            if let Some(session_id) = session_id.as_deref() {
                validate_message_session(state, snapshot, &to, session_id)?;
            }
            let result = accept_message(
                state,
                MessageSendRequest {
                    idempotency_key: request.idempotency_key.clone(),
                    from: authority_actor.clone(),
                    to,
                    // An attachment may travel alone; text is then optional.
                    content: if p.get("attachments").is_some_and(|value| value.as_array().is_some_and(|list| !list.is_empty())) {
                        p.get("content").and_then(Value::as_str).unwrap_or_default().to_owned()
                    } else {
                        parameter_string(p, "content")?
                    },
                    title: p.get("title").and_then(Value::as_str).map(str::to_owned),
                    in_reply_to: p
                        .get("in_reply_to")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    tags: p
                        .get("tags")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    attachments: p
                        .get("attachments")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(|_| {
                            validation("attachments are `{blob, media_type, name?}` records")
                        })?
                        .unwrap_or_default(),
                },
                session_id,
                p.get("signature")
                    .map(|signature| {
                        serde_json::from_value(signature.clone()).map_err(|error| {
                            validation(format!("the message signature is malformed: {error}"))
                        })
                    })
                    .transpose()?,
            )?
            .0;
            Ok(vec![result.subject])
        }
        "message.read" | "message.close" => {
            let target = parameter_string(p, "target_id")?;
            let lifecycle = if request.action_type == "message.read" {
                "read"
            } else {
                "closed"
            };
            let result = post_message_claim(
                State(state.clone()),
                AxumPath(target),
                Json(MessageLifecycleRequest {
                    lifecycle: lifecycle.into(),
                    actor: Some(authority_actor.clone()),
                    transport: None,
                    runtime_id: None,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "launch.create" => {
            let target = p
                .get("target")
                .and_then(Value::as_object)
                .ok_or_else(|| validation("launch creation requires a typed target"))?;
            let target_type = target
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| validation("launch target requires `type`"))?;
            let (mission, run, workspace) = match target_type {
                "new-mission" => (
                    target
                        .get("mission_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a new-mission launch target requires `mission_id`")
                        })?
                        .trim_start_matches("mission/")
                        .to_owned(),
                    None,
                    target
                        .get("workspace")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a new-mission launch target requires `workspace`")
                        })?
                        .to_owned(),
                ),
                "mission-run" => {
                    let run_id = target
                        .get("mission_run_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a mission-run launch target requires `mission_run_id`")
                        })?;
                    let current = state
                        .store
                        .mission_run(run_id)
                        .map_err(ApiError::internal)?
                        .ok_or_else(|| {
                            ApiError::not_found(format!("mission run `{run_id}` does not exist"))
                        })?;
                    let generation = target
                        .get("generation_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a mission-run launch target requires `generation_id`")
                        })?;
                    if generation != current.generation
                        || request.fence.mission_generation.as_deref()
                            != Some(current.generation.as_str())
                    {
                        return Err(stale("the launch target generation is stale"));
                    }
                    (
                        current.mission.trim_start_matches("mission/").to_owned(),
                        Some(current.subject),
                        current.workspace,
                    )
                }
                _ => return Err(validation("launch target type is invalid")),
            };
            let launch = start_planning_session(
                State(state.clone()),
                Json(PlanningSessionStartRequest {
                    mission,
                    run,
                    request: parameter_string(p, "request")?.into_bytes(),
                    workspace,
                    requester: Some(authority_actor.clone()),
                    provider: p.get("provider").and_then(Value::as_str).map(str::to_owned),
                    model: p.get("model").and_then(Value::as_str).map(str::to_owned),
                    effort: p.get("effort").and_then(Value::as_str).map(str::to_owned),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        "launch.revise" => {
            let launch_id = parameter_string(p, "launch_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let launch = revise_planning_session(
                State(state.clone()),
                AxumPath(launch_session_id(&launch_id).to_owned()),
                Json(PlanningRevisionRequest {
                    actor: authority_actor.clone(),
                    feedback: parameter_string(p, "feedback")?.into_bytes(),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        "launch.preview" => {
            let launch_id = parameter_string(p, "launch_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let variant_id = parameter_string(p, "variant_id")?;
            let variant = variant_id.rsplit('/').next().unwrap_or(&variant_id);
            let launch = preview_named_planning_candidate(
                State(state.clone()),
                AxumPath((launch_session_id(&launch_id).to_owned(), variant.to_owned())),
            )
            .await?
            .0;
            Ok(vec![
                format!("launch/{}", launch.id),
                format!("launch-variant/{}/{}", launch.id, variant),
            ])
        }
        "launch.approve" => {
            let launch_id = parameter_string(p, "launch_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let requested_variant = parameter_string(p, "variant_id")?;
            let requested_variant = requested_variant
                .rsplit('/')
                .next()
                .unwrap_or(&requested_variant);
            let launch = state
                .store
                .planning_session(launch_session_id(&launch_id))
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("launch `{launch_id}` does not exist"))
                })?;
            if launch
                .candidate
                .as_ref()
                .map(|candidate| candidate.variant.as_str())
                != Some(requested_variant)
            {
                return Err(stale("the selected launch variant is stale"));
            }
            let launch = approve_planning_session(
                State(state.clone()),
                AxumPath(launch_session_id(&launch_id).to_owned()),
                Json(PlanningApprovalRequest {
                    actor: authority_actor.clone(),
                    preview_hash: request.fence.preview_token.clone().ok_or_else(|| {
                        validation("launch approval requires a preview token fence")
                    })?,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        "launch.cancel" => {
            let launch_id = parameter_string(p, "target_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let launch = cancel_planning_session(
                State(state.clone()),
                AxumPath(launch_session_id(&launch_id).to_owned()),
                Json(PlanningCancelRequest {
                    actor: authority_actor.clone(),
                    reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        action @ ("work.claim" | "work.renew" | "work.progress" | "work.complete" | "work.fail"
        | "work.release") => {
            let target = parameter_string(p, "target_id")?;
            validate_work_fence(state, &target, &request.fence)?;
            if action == "work.claim" {
                let expected = request
                    .fence
                    .runtime_incarnation
                    .as_deref()
                    .ok_or_else(|| validation("work claim requires a runtime incarnation fence"))?;
                let live = state
                    .store
                    .current_harness(authority_actor)
                    .map_err(ApiError::internal)?;
                if !live.as_ref().is_some_and(|harness| {
                    harness.incarnation_id == expected && harness.state != "ended"
                }) {
                    return Err(stale("the claiming agent's runtime incarnation changed"));
                }
            }
            let result = state
                .store
                .work_action(
                    &target,
                    action.trim_start_matches("work."),
                    &WorkRequest {
                        actor: Some(authority_actor.clone()),
                        incarnation: request.fence.runtime_incarnation.clone(),
                        summary: p.get("summary").and_then(Value::as_str).map(str::to_owned),
                        reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                        evidence: p
                            .get("evidence")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        idempotency_key: request.idempotency_key.clone(),
                    },
                )
                .map_err(ApiError::bad)?;
            signal_changed(state);
            Ok(vec![result.subject])
        }
        "work.retry" => {
            let target = parameter_string(p, "target_id")?;
            validate_work_fence(state, &target, &request.fence)?;
            let result = retry_work(
                State(state.clone()),
                AxumPath(target),
                Json(WorkRetryRequest {
                    actor: authority_actor.clone(),
                    reason: parameter_string(p, "reason")?,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "mission.start" => {
            let inputs = p
                .get("inputs")
                .and_then(Value::as_object)
                .ok_or_else(|| validation("mission start requires an `inputs` object"))?
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_owned()))
                        .ok_or_else(|| validation("mission input values must be strings"))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let result = start_mission_run_action(
                State(state.clone()),
                Json(MissionRunRequest {
                    mission: parameter_string(p, "mission_id")?,
                    revision: None,
                    workspace: parameter_string(p, "workspace")?,
                    requester: Some(authority_actor.clone()),
                    mode: Some("run".into()),
                    inputs,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "mission.cancel" => {
            let target = parameter_string(p, "target_id")?;
            let current = state
                .store
                .mission_run(&target)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("mission run `{target}` does not exist"))
                })?;
            if request.fence.mission_generation.as_deref() != Some(current.generation.as_str()) {
                return Err(stale("the mission generation fence is stale"));
            }
            let reason = p
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("the mission was cancelled by its requester");
            state
                .store
                .request_mission_run_cancellation(&current.subject, reason)
                .map_err(ApiError::bad)?;
            signal_changed(state);
            Ok(vec![current.subject])
        }
        decision @ ("mission.approve-revision" | "mission.cancel-revision") => {
            let target = parameter_string(p, "target_id")?;
            let proposal = state
                .store
                .revision_proposal(&target)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("revision proposal `{target}` does not exist"))
                })?;
            if request.fence.mission_generation.as_deref()
                != Some(proposal.source_generation.as_str())
            {
                return Err(stale("the revision proposal generation fence is stale"));
            }
            if decision == "mission.approve-revision" {
                if request
                    .fence
                    .preview_token
                    .as_deref()
                    .is_some_and(|preview| proposal.preview_hash.as_deref() != Some(preview))
                {
                    return Err(stale("the revision proposal preview fence is stale"));
                }
                let result = approve_revision_proposal(
                    State(state.clone()),
                    AxumPath(target),
                    Json(RevisionApprovalRequest {
                        actor: authority_actor.clone(),
                        preview_hash: request.fence.preview_token.clone().ok_or_else(|| {
                            validation("revision approval requires a preview token fence")
                        })?,
                        idempotency_key: request.idempotency_key.clone(),
                    }),
                )
                .await?
                .0;
                Ok(vec![result.mission_run.subject])
            } else {
                let result = cancel_revision_proposal(
                    State(state.clone()),
                    AxumPath(target),
                    Json(RevisionCancelRequest {
                        actor: authority_actor.clone(),
                        reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                        idempotency_key: request.idempotency_key.clone(),
                    }),
                )
                .await?
                .0;
                Ok(vec![result.subject])
            }
        }
        "session.import" => import_external_session_action(state, session, request).await,
        "terminal.input" => {
            let mode = match parameter_string(p, "mode")?.as_str() {
                "line" => SessionInputMode::Line,
                "raw" => SessionInputMode::Raw,
                "key" => SessionInputMode::Key,
                _ => return Err(validation("terminal input mode is invalid")),
            };
            let target = terminal_subject(&parameter_string(p, "terminal_id")?);
            let result = input_session_as(
                state,
                target,
                SessionInputRequest {
                    expected_incarnation: request.fence.runtime_incarnation.clone().ok_or_else(
                        || validation("terminal input requires a runtime incarnation fence"),
                    )?,
                    mode,
                    value: parameter_string(p, "value")?,
                    idempotency_key: request.idempotency_key.clone(),
                },
                authority_actor,
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "terminal.resize" => {
            let target = terminal_subject(&parameter_string(p, "terminal_id")?);
            let rows = p
                .get("rows")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value != 0)
                .ok_or_else(|| validation("terminal resize rows must fit a positive u16"))?;
            let columns = p
                .get("columns")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value != 0)
                .ok_or_else(|| validation("terminal resize columns must fit a positive u16"))?;
            let live = live_session(state, &target, request.fence.runtime_incarnation.as_deref())?;
            if !live.terminal {
                return Err(validation("terminal resize requires a terminal runtime"));
            }
            let socket = state.pty_root.join(format!("{}.sock", live.runtime_id));
            let runtime_id = live.runtime_id.clone();
            tokio::task::spawn_blocking(move || {
                let stream = std::os::unix::net::UnixStream::connect(&socket)?;
                let mut connection = pty_client::SessionConnection::attach_over(
                    stream,
                    &runtime_id,
                    rows,
                    columns,
                    Some(Duration::from_secs(2)),
                )?;
                connection.resize(rows, columns);
                connection.disconnect();
                anyhow::Ok(())
            })
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
            Ok(vec![client_detail_id(
                "terminal",
                &parameter_string(p, "terminal_id")?,
            )])
        }
        "terminal.detach" => Ok(vec![detach_terminal_attachment(state, session, request)?]),
        action @ ("agent.stop" | "agent.start") => {
            require_creation_actor(session)?;
            let agent = client_detail_id("agent", &parameter_string(p, "agent")?);
            if action == "agent.stop" {
                serde_json::from_value::<st3_client::AgentStopParameters>(p.clone())
                    .map_err(|error| validation(error.to_string()))?;
            } else {
                serde_json::from_value::<st3_client::AgentStartParameters>(p.clone())
                    .map_err(|error| validation(error.to_string()))?;
            }
            let token = state
                .store
                .selected_desired_token(&agent)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("agent `{agent}` has no declaration"))
                })?;
            if request.fence.runtime_desired_revision.as_deref() != Some(token.as_str()) {
                return Err(stale("the agent desired revision changed"));
            }
            // A mission seat stops like any other, and starts again on its run's declaration.
            if action == "agent.start"
                && state
                    .store
                    .declaration_ended_by_stop(&agent)
                    .map_err(ApiError::internal)?
                    .is_some_and(|ended| ended.declaration.owner_run.is_some())
            {
                state
                    .store
                    .start_mission_seat(
                        &agent,
                        Some(&token),
                        authority_actor,
                        &format!("{}:mission-seat-start", request.idempotency_key),
                    )
                    .map_err(ApiError::bad)?;
                signal_changed(state);
                return Ok(vec![agent]);
            }
            let mut claim = state
                .store
                .claim_by_id(&token)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::internal("selected declaration is missing"))?;
            let declared = loop {
                let desired: crate::model::DesiredSubject =
                    serde_json::from_value(claim.body).map_err(ApiError::internal)?;
                if desired.kind == "agent" {
                    if action == "agent.start" && desired.owner_run.is_some() {
                        return Err(validation(
                            "its mission run already declares this agent; restart relaunches it",
                        ));
                    }
                    break desired;
                }
                if desired.kind != "stop" || claim.predecessors.len() != 1 {
                    return Err(validation("agent has no unambiguous prior declaration"));
                }
                claim = state
                    .store
                    .claim_by_id(&claim.predecessors[0])
                    .map_err(ApiError::internal)?
                    .ok_or_else(|| ApiError::internal("prior declaration is missing"))?;
            };
            let kdl = if action == "agent.stop" {
                format!(
                    "version 2\nstop {}\n",
                    serde_json::to_string(&agent).map_err(ApiError::internal)?
                )
            } else {
                let mut node =
                    crate::graph::render_desired_node(&declared.desired).map_err(ApiError::bad)?;
                let identity = agent.trim_start_matches("agent/");
                let mut body = node.children_mut().take().unwrap_or_default();
                if let Some(child) = body
                    .nodes_mut()
                    .iter_mut()
                    .find(|child| child.name().value() == "identity")
                {
                    child.entries_mut()[0] = kdl::KdlEntry::new(identity);
                } else {
                    node.entries_mut()[0] = kdl::KdlEntry::new(identity);
                }
                if let Some(member) = declared.member {
                    let mut host = kdl::KdlNode::new("host");
                    host.entries_mut().push(kdl::KdlEntry::new(member.host));
                    if let Some(child) = body
                        .nodes_mut()
                        .iter_mut()
                        .find(|child| child.name().value() == "host")
                    {
                        *child = host;
                    } else {
                        body.nodes_mut().push(host);
                    }
                }
                node.set_children(body);
                format!("version 2\n{node}\n")
            };
            apply_runtime_control_intent(state, snapshot, request, authority_actor, kdl).await?;
            Ok(vec![agent])
        }
        action @ ("agent.suspend" | "agent.resume") => {
            let agent = client_detail_id("agent", &parameter_string(p, "agent")?);
            let reason = if action == "agent.suspend" {
                serde_json::from_value::<st3_client::AgentSuspendParameters>(p.clone())
                    .map_err(|error| validation(error.to_string()))?
                    .reason
            } else {
                serde_json::from_value::<st3_client::AgentResumeParameters>(p.clone())
                    .map_err(|error| validation(error.to_string()))?;
                None
            };
            let desired = request
                .fence
                .runtime_desired_revision
                .clone()
                .ok_or_else(|| validation("the action needs fence.runtime_desired_revision"))?;
            let incarnation = request.fence.runtime_incarnation.clone();
            if action == "agent.suspend" && incarnation.is_none() {
                return Err(validation("agent.suspend needs fence.runtime_incarnation"));
            }
            let suspension = super::AgentSuspensionRequest {
                subject: agent.clone(),
                actor: authority_actor.clone(),
                idempotency_key: request.idempotency_key.clone(),
                reason,
            };
            let fence = super::SuspensionFence {
                incarnation,
                desired,
            };
            if action == "agent.suspend" {
                super::request_suspend(state, suspension, Some(fence))?;
            } else {
                super::request_resume(state, suspension, Some(fence))?;
            }
            Ok(vec![agent])
        }
        "agent.queue-move" => {
            let agent = client_detail_id("agent", &parameter_string(p, "agent_id")?);
            let move_request = crate::model::SeatQueueMoveRequest {
                agent: agent.clone(),
                run: client_detail_id("mission-run", &parameter_string(p, "mission_run_id")?),
                placement: parameter_string(p, "placement")?,
                anchor: p
                    .get("anchor_run_id")
                    .map(|_| parameter_string(p, "anchor_run_id"))
                    .transpose()?
                    .map(|anchor| client_detail_id("mission-run", &anchor)),
                reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                actor: authority_actor.clone(),
                idempotency_key: request.idempotency_key.clone(),
            };
            let store = state.store.clone();
            blocking_action(move || store.move_seat_queue_run(&move_request)).await?;
            signal_changed(state);
            Ok(vec![agent])
        }
        action @ ("lane.join" | "lane.leave" | "lane.move" | "lane.mark" | "lane.approve") => {
            let optional = |key: &str| p.get(key).map(|_| parameter_string(p, key)).transpose();
            let change_request = crate::model::LaneChangeRequest {
                lane: parameter_string(p, "lane_id")?,
                change: action.trim_start_matches("lane.").to_owned(),
                entry: parameter_string(p, "entry_id")?,
                reason: optional("reason")?,
                outcome: optional("outcome")?,
                placement: optional("placement")?,
                anchor: optional("anchor_id")?,
                state: optional("state")?,
                detail: optional("detail")?,
                head: optional("head")?,
                actor: authority_actor.clone(),
                idempotency_key: request.idempotency_key.clone(),
            };
            let store = state.store.clone();
            let response = blocking_action(move || store.change_lane(&change_request)).await?;
            if response.claim.is_some() {
                signal_changed(state);
            }
            Ok(vec![response.lane.subject])
        }
        action @ ("runtime.stop" | "runtime.restart" | "runtime.reset") => {
            let runtime = runtime_control_target(state, snapshot, session, request)?;
            let owner = runtime["owner_id"]
                .as_str()
                .ok_or_else(|| ApiError::internal("runtime has no owner"))?;
            let reason = p
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("requested through the client");
            match action {
                "runtime.stop" => {
                    let kdl = format!(
                        "version 2\nstop {}\n",
                        serde_json::to_string(owner).map_err(ApiError::internal)?
                    );
                    apply_runtime_control_intent(state, snapshot, request, authority_actor, kdl)
                        .await?;
                    Ok(vec![owner.to_owned()])
                }
                "runtime.restart" => {
                    let member = state
                        .store
                        .desired_subjects()
                        .map_err(ApiError::internal)?
                        .into_iter()
                        .find(|desired| desired.subject == owner)
                        .and_then(|desired| desired.member)
                        .ok_or_else(|| validation("runtime restart requires a declared member"))?;
                    if member.restart != crate::model::RestartType::Always {
                        return Err(validation(
                            "runtime restart requires an always restart policy",
                        ));
                    }
                    let result = signal_session_as(
                        state.clone(),
                        owner.to_owned(),
                        SessionSignalRequest {
                            expected_incarnation: request
                                .fence
                                .runtime_incarnation
                                .clone()
                                .expect("validated incarnation"),
                            signal: "terminate".into(),
                            idempotency_key: format!("{}:runtime-restart", request.idempotency_key),
                        },
                        authority_actor,
                    )
                    .await?
                    .0;
                    Ok(vec![result.subject])
                }
                "runtime.reset" => {
                    let run_id = runtime["owner_run_id"]
                        .as_str()
                        .ok_or_else(|| validation("runtime reset requires a run-owned runtime"))?;
                    let run = state
                        .store
                        .mission_run(run_id)
                        .map_err(ApiError::internal)?
                        .ok_or_else(|| {
                            ApiError::not_found(format!("mission run `{run_id}` does not exist"))
                        })?;
                    let kdl = format!(
                        "version 2\nmission-run {} {{\n  reset {} {{\n    runtime {}\n    from {}\n    reason {}\n  }}\n}}\n",
                        serde_json::to_string(&run.id).map_err(ApiError::internal)?,
                        serde_json::to_string(&request.id).map_err(ApiError::internal)?,
                        serde_json::to_string(owner).map_err(ApiError::internal)?,
                        serde_json::to_string(&run.generation).map_err(ApiError::internal)?,
                        serde_json::to_string(reason).map_err(ApiError::internal)?,
                    );
                    apply_runtime_control_intent(state, snapshot, request, authority_actor, kdl)
                        .await?;
                    Ok(vec![owner.to_owned()])
                }
                _ => unreachable!(),
            }
        }
        "runtime.context-clear" => {
            let target = terminal_subject(&parameter_string(p, "target_id")?);
            let result = clear_context(
                State(state.clone()),
                AxumPath(target),
                Json(ContextClearRequest {
                    expected_incarnation: request.fence.runtime_incarnation.clone().ok_or_else(
                        || validation("runtime control requires an incarnation fence"),
                    )?,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "runtime.signal" => {
            let target = terminal_subject(&parameter_string(p, "target_id")?);
            let result = signal_session_as(
                state.clone(),
                target,
                SessionSignalRequest {
                    expected_incarnation: request.fence.runtime_incarnation.clone().ok_or_else(
                        || validation("runtime control requires an incarnation fence"),
                    )?,
                    signal: parameter_string(p, "signal")?,
                    idempotency_key: request.idempotency_key.clone(),
                },
                authority_actor,
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "pairing.revoke" => {
            let device = parameter_string(p, "target_id")?;
            let claims = state
                .store
                .claims_for_kind_at("custom.client.pairing-completed", None, true, 10_000)
                .map_err(ApiError::internal)?;
            let paired = claims
                .claims
                .iter()
                .find(|claim| {
                    claim
                        .body
                        .pointer("/fields/device_id")
                        .and_then(Value::as_str)
                        == Some(device.as_str())
                })
                .ok_or_else(|| {
                    ApiError::not_found(format!("paired device `{device}` does not exist"))
                })?;
            state
                .store
                .append_claim(&ClaimInput {
                    subject: paired.subject.clone(),
                    kind: "custom.client.pairing-revoked".into(),
                    actor: Some(authority_actor.clone()),
                    fields: BTreeMap::from([("device_id".into(), Value::String(device.clone()))]),
                    evidence: vec![paired.id.clone()],
                    expected_subject: None,
                    idempotency_key: Some(request.idempotency_key.clone()),
                })
                .map_err(ApiError::bad)?;
            // A revoked device's key signs nothing more, on every member.
            let field = |name: &str| paired.body.pointer(&format!("/fields/{name}")).and_then(Value::as_str);
            if let (Some(key), Some(person)) = (field("device_public_key").and_then(device_signing_key), field("person_id")) {
                state
                    .store
                    .revoke_device_key(person, key, &format!("pairing of {device} revoked"))
                    .map_err(ApiError::bad)?;
            }
            signal_changed(state);
            Ok(vec![device])
        }
        _ => Err(ApiError {
            status: StatusCode::NOT_IMPLEMENTED,
            code: "unsupported-capability".into(),
            message: format!(
                "action `{}` is declared but is not available on this daemon",
                request.action_type
            ),
            details: Box::default(),
        }),
    }
}

// Concurrent retries must wait for the first dispatch to persist its receipt. Weak entries
// keep the gate table bounded by requests currently executing or waiting.
fn action_gate(
    state: &AppState,
    session: &ClientSession,
    key: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut gates = GATES
        .get_or_init(Mutex::default)
        .lock()
        .expect("action gates poisoned");
    gates.retain(|_, gate| gate.strong_count() > 0);
    let key = format!("{}:{}:{key}", state.store.origin(), session.actor);
    if let Some(gate) = gates.get(&key).and_then(std::sync::Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

fn action_request_digest(request: &ActionRequest) -> Result<String, ApiError> {
    let mut content = serde_json::to_value(request).map_err(ApiError::internal)?;
    content
        .as_object_mut()
        .expect("an action is an object")
        .remove("fence");
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&content).map_err(ApiError::internal)?,
    )))
}

pub(super) async fn action(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Json(request): Json<ActionRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.api_version != CLIENT_API_VERSION
        || !request.id.starts_with("action/")
        || !(16..=256).contains(&request.idempotency_key.len())
    {
        return Err(validation(
            "the action version, ID, or idempotency key is invalid",
        ));
    }
    if contains_identity_selector(&request.parameters) {
        return Err(validation(
            "client actions cannot select an actor, credential, or fleet secret",
        ));
    }
    if request.action_type == "attention.resolve" {
        return Err(ApiError::bad(St3Error::new(
            "attention-migrated",
            "attention is a view; complete or remedy its source",
        )));
    }
    let scope = action_scope(&request.action_type)
        .ok_or_else(|| validation("the action type is unknown"))?;
    require_scope(&session, scope)?;
    // Snapshot provenance is checked, while freshness belongs to the action's own
    // revision, generation, incarnation, or screen checks.
    let read_only_terminal_lifecycle = matches!(
        request.action_type.as_str(),
        "terminal.attach" | "terminal.detach"
    );
    if !read_only_terminal_lifecycle && !acting_party(&session) {
        return Err(forbidden(
            "client mutations require a concrete person or a local agent",
        ));
    }
    let gate = action_gate(&state, &session, &request.idempotency_key);
    let _guard = gate.lock().await;
    let request_digest = action_request_digest(&request)?;
    // Receipts written before action-content.v1 still accept the exact original request.
    let legacy_digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&request).map_err(ApiError::internal)?,
    ));
    let receipt_digest = hex::encode(Sha256::digest(
        format!("{}:{}", session.actor, request.idempotency_key).as_bytes(),
    ));
    let receipt_subject = format!("custom/client/action-{}", &receipt_digest[..32]);
    if let Some(receipt) = state
        .store
        .claims_for(&receipt_subject, Some("custom.client.action-result"))
        .map_err(ApiError::internal)?
        .last()
    {
        let old_digest = receipt
            .body
            .pointer("/fields/request_digest")
            .and_then(Value::as_str);
        if old_digest != Some(request_digest.as_str()) && old_digest != Some(legacy_digest.as_str())
        {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "idempotency-conflict".into(),
                message: "the idempotency key was already used for a different action".into(),
                details: Box::default(),
            });
        }
        let mut result = receipt
            .body
            .pointer("/fields/result")
            .cloned()
            .ok_or_else(|| ApiError::internal("the client action receipt has no result"))?;
        if request.action_type == "terminal.attach" {
            let attachment_id = result["affected_ids"]
                .as_array()
                .and_then(|ids| ids.first())
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("the terminal attach receipt has no ID"))?;
            result["terminal_attachment"] =
                terminal_attachment_response(&state, &session, attachment_id)?;
        }
        result["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
        return Ok(Json(result));
    }
    if matches!(
        request.action_type.as_str(),
        "terminal.input" | "terminal.resize"
    ) {
        let terminal_id = parameter_string(&request.parameters, "terminal_id")?;
        let incarnation = request
            .fence
            .runtime_incarnation
            .as_deref()
            .ok_or_else(|| validation("terminal control requires an incarnation fence"))?;
        let live =
            remote_terminal_live_session(&state, &terminal_subject(&terminal_id), incarnation)?;
        if live.owner_host_id != client_host_id(&state.node) {
            validate_fence(&state, &request.fence)?;
            let expected_sequence = request
                .fence
                .terminal_sequence
                .ok_or_else(|| validation("terminal control requires a sequence fence"))?;
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&live.owner_host_id))?;
            let mut value = relay
                .read(
                    &live.owner_host_id,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor.clone(),
                        relay: None,
                        request: crate::peer::ClientReadOperation::TerminalControl {
                            action_id: request.id.clone(),
                            idempotency_key: request.idempotency_key.clone(),
                            action_type: request.action_type.clone(),
                            terminal_id: client_detail_id("terminal", &terminal_id),
                            runtime_incarnation: incarnation.into(),
                            expected_sequence,
                            parameters: request.parameters.clone(),
                        },
                    },
                )
                .await
                .map_err(|error| remote_read_error(&live.owner_host_id, error))?;
            value["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
            return Ok(Json(value));
        }
    }
    if matches!(
        request.action_type.as_str(),
        "terminal.input" | "terminal.resize"
    ) {
        let terminal_id = parameter_string(&request.parameters, "terminal_id")?;
        let incarnation = request
            .fence
            .runtime_incarnation
            .as_deref()
            .ok_or_else(|| validation("terminal control requires an incarnation fence"))?;
        let expected = request
            .fence
            .terminal_sequence
            .ok_or_else(|| validation("terminal control requires a sequence fence"))?;
        let screen = terminal_screen_value(
            &state,
            &terminal_id,
            Some(incarnation),
            None,
            Duration::ZERO,
        )
        .await?;
        if screen["next_sequence"].as_u64() != Some(expected) {
            return Err(stale("the terminal sequence fence is stale"));
        }
    }
    let mut reconciled_attachment = None;
    let fence_result = validate_fence(&state, &request.fence);
    if let Err(error) = fence_result {
        if request.action_type == "terminal.attach" {
            reconciled_attachment =
                existing_terminal_attachment(&state, &session, &request, &request_digest)?;
        }
        let recovered_creation = matches!(
            request.action_type.as_str(),
            "agent.create" | "terminal.create"
        ) && require_creation_actor(&session).is_ok()
            && existing_creation(&state, &session, &request)?.is_some();
        if reconciled_attachment.is_none() && !recovered_creation {
            return Err(error);
        }
    }
    let terminal_attachment = if request.action_type == "terminal.attach" {
        let target = parameter_string(&request.parameters, "target_id")?;
        let incarnation = request
            .fence
            .runtime_incarnation
            .as_deref()
            .ok_or_else(|| validation("terminal attach requires an incarnation fence"))?;
        let live = remote_terminal_live_session(&state, &terminal_subject(&target), incarnation)?;
        if live.owner_host_id != client_host_id(&state.node) && reconciled_attachment.is_none() {
            if !acting_party(&session) {
                return Err(forbidden(
                    "remote terminal attach requires a concrete person or agent",
                ));
            }
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&live.owner_host_id))?;
            let screen = relay
                .read(
                    &live.owner_host_id,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor.clone(),
                        relay: None,
                        request: crate::peer::ClientReadOperation::TerminalScreen {
                            terminal_id: client_detail_id("terminal", &target),
                            facts: false,
                        },
                    },
                )
                .await
                .map_err(|error| remote_read_error(&live.owner_host_id, error))?;
            if screen["runtime_incarnation"].as_str() != Some(incarnation) {
                return Err(stale("the terminal incarnation fence is stale"));
            }
        }
        Some(if let Some(existing) = reconciled_attachment {
            existing
        } else {
            create_terminal_attachment(&state, &session, &request, &request_digest)?
        })
    } else {
        None
    };
    let affected = if let Some(attachment) = &terminal_attachment {
        vec![
            attachment["attachment_id"]
                .as_str()
                .ok_or_else(|| ApiError::internal("terminal attachment has no ID"))?
                .to_owned(),
        ]
    } else {
        dispatch_action(&state, &snapshot, &session, &request).await?
    };
    let operation_id = format!("operation/client-{}", &request_digest[..24]);
    let mut result = json!({ "kind": "action-result", "action_id": request.id, "operation_id": operation_id, "status": "completed", "affected_ids": affected });
    if let Some(attachment) = terminal_attachment {
        result["terminal_attachment"] = attachment;
    }
    let mut persisted_result = result.clone();
    persisted_result
        .as_object_mut()
        .expect("action result is an object")
        .remove("terminal_attachment");
    let receipt_write = state.store.append_claim(&ClaimInput {
        subject: receipt_subject.clone(),
        kind: "custom.client.action-result".into(),
        actor: Some(session_claim_actor(&session)),
        fields: BTreeMap::from([
            (
                "authority_actor".into(),
                Value::String(session.authority_actor.clone()),
            ),
            (
                "request_digest".into(),
                Value::String(request_digest.clone()),
            ),
            ("result".into(), persisted_result.clone()),
        ]),
        evidence: Vec::new(),
        expected_subject: Some(None),
        idempotency_key: None,
    });
    if let Err(error) = receipt_write {
        if error.code != "stale-subject" {
            return Err(ApiError::bad(error));
        }
        let receipt = state
            .store
            .claims_for(&receipt_subject, Some("custom.client.action-result"))
            .map_err(ApiError::internal)?
            .into_iter()
            .last()
            .ok_or_else(|| ApiError::internal("action receipt CAS lost without a winner"))?;
        if receipt
            .body
            .pointer("/fields/request_digest")
            .and_then(Value::as_str)
            != Some(request_digest.as_str())
        {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "idempotency-conflict".into(),
                message: "the idempotency key was already used for a different action".into(),
                details: Box::default(),
            });
        }
        let mut replay = receipt
            .body
            .pointer("/fields/result")
            .cloned()
            .ok_or_else(|| ApiError::internal("the client action receipt has no result"))?;
        if request.action_type == "terminal.attach" {
            let attachment_id = replay["affected_ids"]
                .as_array()
                .and_then(|ids| ids.first())
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("the terminal attach receipt has no ID"))?;
            replay["terminal_attachment"] =
                terminal_attachment_response(&state, &session, attachment_id)?;
        }
        replay["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
        return Ok(Json(replay));
    }
    signal_changed(&state);
    result["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::Barrier;

    fn assert_collection_frame_conforms(frame: &Value) {
        let mut schema: Value = serde_json::from_str(include_str!(
            "../../../../docs/st3/client-v0/schemas/client-v0.schema.json"
        ))
        .unwrap();
        schema.as_object_mut().unwrap().remove("oneOf");
        schema["$ref"] = json!("#/$defs/CollectionFrame");
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        let errors: Vec<_> = validator
            .iter_errors(frame)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "{frame}: {errors:?}");
        let mut extra = frame.clone();
        extra["undeclared"] = json!(true);
        assert!(!validator.is_valid(&extra));
        let mut bad_retry = frame.clone();
        bad_retry["retryable"] = json!("yes");
        assert!(!validator.is_valid(&bad_retry));
    }

    #[tokio::test]
    async fn conversation_failure_and_recovery_frames_conform_to_collection_contract() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        for (remote, expected_kind) in [(Some("host/offline"), "resync"), (None, "error")] {
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let follower = tokio::spawn(follow_conversation(
                state.clone(),
                ClientSession::local(None).unwrap(),
                "chat".into(),
                "session/missing".into(),
                remote.map(str::to_owned),
                sender,
            ));
            let (_, frame) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            follower.abort();
            assert_eq!(frame["kind"], expected_kind, "{frame}");
            assert_eq!(frame["retryable"], expected_kind == "resync");
            assert_collection_frame_conforms(&frame);
        }
        assert_collection_frame_conforms(&json!({"kind":"resync", "id":"minimal"}));
    }

    #[test]
    fn agent_todo_projection_selects_latest_at_snapshot_and_fences_native_identity() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let subject = "agent/todo-worker";
        let append = |kind: &str, fields: Value| {
            state.store.append_claim(&ClaimInput {
                subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
                fields: fields.as_object().unwrap().iter()
                    .map(|(key, value)| (key.clone(), value.clone())).collect(),
                evidence: Vec::new(), expected_subject: None, idempotency_key: None,
            }).unwrap()
        };
        assert!(agent_todo(&state.store, subject, Some("one"), state.store.index().unwrap()).unwrap().is_null());
        append("runtime.observed", json!({
            "status":"running", "runtime_id":"todo-runtime", "incarnation_id":"one"
        }));
        append("harness.session-file", json!({
            "harness":"omp", "agent":subject, "session_id":"native-one", "path":"/tmp/session"
        }));
        let before = state.store.index().unwrap();
        let cached = client_agent_resources(&state.store, true, "2026-10-03T09:00:00Z", before).unwrap();
        assert!(cached.iter().find(|agent| agent["id"] == subject).unwrap()["todo"].is_null());
        let snapshot = json!({
            "harness":"omp", "session_id":"native-one", "incarnation_id":"one",
            "observed_at":"2026-10-03T09:00:00Z", "source_op":"update",
            "phases":[{"name":"Build","tasks":[{"content":"Deploy","status":"blocked","blocker":"Approval"}]}],
            "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":1,"abandoned":0}, "truncated":false
        });
        let first = append("harness.todo.observed", snapshot.clone());
        assert!(crate::store::local_observation_position(&first).is_none());
        assert!(state.store.index().unwrap() > before);
        let old = client_agent_resources(&state.store, true, "2026-10-03T09:00:00Z", before).unwrap();
        assert!(old.iter().find(|agent| agent["id"] == subject).unwrap()["todo"].is_null());
        let refreshed = client_agent_resources(&state.store, true, "2026-10-03T09:00:00Z", first.store_index).unwrap();
        assert_eq!(refreshed.iter().find(|agent| agent["id"] == subject).unwrap()["todo"]["snapshot"], snapshot);
        let first_value = agent_todo(&state.store, subject, Some("one"), first.store_index).unwrap();
        assert_eq!(first_value["snapshot"], snapshot);
        assert_eq!(first_value["claim_id"], first.id);
        assert_eq!(first_value["stale"], false);
        assert_eq!(agent_todo(&state.store, subject, Some("two"), first.store_index).unwrap()["stale"], true);
        let mut empty = snapshot;
        empty["phases"] = json!([]);
        empty["totals"]["blocked"] = json!(0);
        empty["truncated"] = json!(true);
        append("harness.session-file", json!({
            "harness":"omp", "agent":subject, "session_id":"native-one", "path":"/tmp/fence"
        }));
        let second = append("harness.todo.observed", empty.clone());
        let latest = agent_todo(&state.store, subject, Some("one"), second.store_index).unwrap();
        assert_eq!(latest["snapshot"], empty);
        assert_eq!(latest["claim_id"], second.id);
        assert_eq!(agent_todo(&state.store, subject, Some("one"), first.store_index).unwrap()["claim_id"], first.id);
        let changed = append("harness.session-file", json!({
            "harness":"omp", "agent":subject, "session_id":"native-two", "path":"/tmp/session"
        }));
        assert_eq!(agent_todo(&state.store, subject, Some("one"), changed.store_index).unwrap()["stale"], true);
    }

    #[test]
    fn agent_todo_malformed_claim_does_not_break_agent_list() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let subject = "agent/malformed-todo";
        let append = |kind: &str, fields: Value| {
            state.store.append_claim(&ClaimInput {
                subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(), expected_subject: None, idempotency_key: None,
            }).unwrap()
        };
        append("runtime.observed", json!({
            "status":"running", "runtime_id":"todo-runtime", "incarnation_id":"one"
        }));
        let claim = append("harness.todo.observed", json!({
            "harness":"omp", "session_id":"native-one", "incarnation_id":"one",
            "observed_at":"2026-10-03T09:00:00Z", "source_op":"clear",
            "phases":[], "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0},
            "truncated":false
        }));
        // Simulate an older or corrupt replicated record beyond the typed writer boundary.
        let connection = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
        connection.execute(
            "UPDATE claims SET body=json_set(body, '$.fields.unrecognized', 1) WHERE id=?1",
            [&claim.id],
        ).unwrap();
        let items = client_agent_resources(
            &state.store, true, "2026-10-03T09:00:00Z", state.store.index().unwrap(),
        ).unwrap();
        assert!(items.iter().find(|agent| agent["id"] == subject).unwrap()["todo"].is_null());
    }

    #[tokio::test]
    async fn steady_collection_retries_a_failed_first_read_without_another_command_or_write() {
        use futures_util::{SinkExt as _, StreamExt as _};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (state, reads) = (state.clone(), counted.clone());
                async move {
                    upgrade.on_upgrade(move |socket| {
                        collection_stream_socket_with_reader(
                            socket,
                            state,
                            ClientSession::local(None).unwrap(),
                            move |state, session, request| {
                                let reads = reads.clone();
                                async move {
                                    if request.id == "refused" {
                                        Err(validation("unknown collection subscription"))
                                    } else if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                                        Err(ApiError::internal("injected first read failure"))
                                    } else {
                                        collection_items(&state, &session, &request).await
                                    }
                                }
                            },
                        )
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"kind":"subscribe","id":"agents","collection":"agents","limit":10})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let first: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(first["kind"], "resync");
        assert_eq!(first["retryable"], true);
        assert_collection_frame_conforms(&first);
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let recovered: Value = serde_json::from_str(next.to_text().unwrap()).unwrap();
        assert_eq!(recovered["kind"], "snapshot");
        assert_eq!(recovered["id"], "agents");
        assert!(reads.load(Ordering::SeqCst) >= 2);
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"kind":"subscribe", "id":"refused", "collection":"agents"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let refused = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let refused: Value = serde_json::from_str(refused.to_text().unwrap()).unwrap();
        assert_eq!(refused["kind"], "error");
        assert_eq!(refused["retryable"], false);
        assert_collection_frame_conforms(&refused);
        socket.close(None).await.unwrap();
        server.abort();
    }

    #[test]
    fn listed_unresolved_process_opens_into_explanatory_no_conversation_state() {
        use crate::external_sessions::{
            ExternalDiscovery, ExternalDriver, ExternalProcess, UnresolvedProcess,
        };

        let unresolved = UnresolvedProcess {
            id: "session/external-process-test".into(),
            revision: "test".into(),
            driver: ExternalDriver::Omp,
            process: ExternalProcess {
                pid: 123,
                parent_pid: 1,
                started_at_unix_ms: 0,
                fingerprint: "123:0:test".into(),
                cwd: None,
                command: "omp --resume native-session".into(),
                exact_session: false,
            },
        };
        let resource =
            super::super::unresolved_session_resource(unresolved.clone(), "2026-09-30T00:00:00Z");
        let id = resource["id"].as_str().unwrap();
        let discovery = ExternalDiscovery {
            sessions: Vec::new(),
            unresolved_processes: vec![unresolved],
        };
        let error = external_conversation_items(discovery.into_conversation(id), id).unwrap_err();
        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.code, "unsupported-capability");
        assert_eq!(error.details["session_id"], id);
        assert_eq!(error.details["reason"], "native-session-unidentified");
        assert!(error.message.contains("not started by st"));
        assert!(
            error
                .message
                .contains("could not identify its saved session")
        );
        assert!(!error.message.contains("does not exist"));
    }

    #[test]
    fn applied_definition_kdl_preserves_kdl_scalars_and_identifiers() {
        let desired = json!({
            "name": "agent",
            "arguments": ["example/worker"],
            "children": [{
                "name": "name with spaces",
                "arguments": ["λ \"quoted\"\\\n", i64::MIN, i64::MAX, 1.0, 1.25, true, false, null],
                "properties": { "property with spaces": "\"\\\nλ" },
                "children": [{ "name": "child", "arguments": [null] }],
            }],
        });
        let rendered = crate::graph::render_agent_desired_kdl(&desired).unwrap();
        let document = rendered.parse::<kdl::KdlDocument>().unwrap();
        assert_eq!(document.nodes()[0].name().value(), "version");
        assert_eq!(document.nodes()[1].name().value(), "agent");
        let node = &document.nodes()[1].children().unwrap().nodes()[0];
        assert_eq!(node.name().value(), "name with spaces");
        let values = node.entries().iter().filter(|entry| entry.name().is_none())
            .map(|entry| entry.value().clone()).collect::<Vec<_>>();
        assert_eq!(values, vec![
            kdl::KdlValue::String("λ \"quoted\"\\\n".into()),
            kdl::KdlValue::Integer(i128::from(i64::MIN)),
            kdl::KdlValue::Integer(i128::from(i64::MAX)),
            kdl::KdlValue::Float(1.0),
            kdl::KdlValue::Float(1.25),
            kdl::KdlValue::Bool(true),
            kdl::KdlValue::Bool(false),
            kdl::KdlValue::Null,
        ]);
        let property = node.entries().iter().find(|entry| entry.name().is_some()).unwrap();
        assert_eq!(property.name().unwrap().value(), "property with spaces");
        assert_eq!(property.value(), &kdl::KdlValue::String("\"\\\nλ".into()));
        let child = &node.children().unwrap().nodes()[0];
        assert_eq!(child.name().value(), "child");
        assert_eq!(child.entries()[0].value(), &kdl::KdlValue::Null);
    }

    fn test_state(root: &Path) -> AppState {
        test_state_named(root, "terminal-test")
    }

    #[test]
    fn a_timeline_message_entry_lists_the_attachments_its_claim_carries() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
        image.extend([7; 32]);
        let hash = crate::blobs::BlobDir::under(root.path()).put(&image).unwrap();
        state
            .store
            .record_blob_upload("person/alex", &hash, "image/png", image.len() as u64, 1 << 20, 60_000)
            .unwrap();
        let sent = accept_message(
            &state,
            MessageSendRequest {
                idempotency_key: "timeline-attachment".into(),
                from: "person/alex".into(),
                to: "agent/terminal-test.seat".into(),
                content: "see image".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: vec![crate::model::AttachmentInput {
                    blob: format!("blob/{hash}"),
                    media_type: "image/png".into(),
                    name: Some("paste.png".into()),
                }],
            },
            None,
            None,
        )
        .unwrap()
        .0;
        let claim = state
            .store
            .claims_for(&sent.subject, Some("message.sent"))
            .unwrap()
            .remove(0);
        let body = session_message_body(&claim);
        assert_eq!(
            body["attachments"],
            json!([{
                "blob": format!("blob/{hash}"), "sha256": hash, "media_type": "image/png",
                "name": "paste.png", "size": image.len(), "origin": "host/terminal-test"
            }])
        );
        let typed: st3_client::TimelineMessageBody = serde_json::from_value(body).unwrap();
        assert_eq!(typed.attachments[0].blob, format!("blob/{hash}"));
        // A message without attachments has no such key.
        let plain = accept_message(
            &state,
            MessageSendRequest {
                idempotency_key: "timeline-plain".into(),
                from: "person/alex".into(),
                to: "agent/terminal-test.seat".into(),
                content: "no image".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            },
            None,
            None,
        )
        .unwrap()
        .0;
        let claim = state
            .store
            .claims_for(&plain.subject, Some("message.sent"))
            .unwrap()
            .remove(0);
        assert!(session_message_body(&claim).get("attachments").is_none());
    }

    #[tokio::test]
    async fn resources_list_filters_latest_observations_and_fences_pages() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let observe = |subject: &str, kind: &str, facts: Value| {
            state.store.append_client_claim(&crate::model::ClaimInput {
                subject: subject.into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), json!(kind)), ("facts".into(), facts)]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            }).unwrap();
        };
        observe("resource/github/a", "vcs.pull-request", json!({"title":"Old", "opened_by":"agent/alice", "opened_by_run":"mission-run/one"}));
        observe("resource/github/b", "vcs.pull-request", json!({"title":"Second", "opened_by":"agent/alice", "opened_by_run":"mission-run/two"}));
        observe("resource/github/c", "vcs.pull-request", json!({"title":"Other", "opened_by":"agent/bob"}));
        observe("resource/repository", "vcs.repository", json!({"url":"https://example.org/repository"}));
        observe("resource/github/a", "vcs.pull-request", json!({"title":"New", "opened_by":"agent/alice", "opened_by_run":"mission-run/one"}));
        let app = super::super::router(state.clone());
        let read = |uri: String| {
            let app = app.clone();
            async move {
                let response = app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
                let status = response.status();
                let body = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES).await.unwrap();
                (status, serde_json::from_slice::<Value>(&body).unwrap())
            }
        };
        let (status, all) = read("/v1/client/resources".into()).await;
        assert_eq!(status, StatusCode::OK, "{all}");
        assert_eq!(all["value"]["items"].as_array().unwrap().iter().map(|item| item["id"].as_str().unwrap()).collect::<Vec<_>>(),
            ["resource/github/a", "resource/github/b", "resource/github/c", "resource/repository"]);
        assert_eq!(all["value"]["items"][0]["facts"]["title"], "New");
        assert_eq!(all["value"]["items"][0]["opened_by"], "agent/alice");
        assert_eq!(all["value"]["items"][0]["opened_by_run"], "mission-run/one");
        chrono::DateTime::parse_from_rfc3339(all["value"]["items"][0]["observed_at"].as_str().unwrap()).unwrap();
        assert_eq!(all["value"]["items"][3]["opened_by"], Value::Null);
        let (status, run) = read("/v1/client/resources?opened_by=mission-run%2Fone&kind=vcs.pull-request".into()).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(run["value"]["items"], json!([all["value"]["items"][0].clone()]));
        let (status, repository) = read("/v1/client/resources?kind=vcs.repository".into()).await;
        assert_eq!(status, StatusCode::OK, "{repository}");
        assert_eq!(repository["value"]["items"], json!([all["value"]["items"][3].clone()]));
        let filters = "opened_by=agent%2Falice&kind=vcs.pull-request&subject_prefix=resource%2Fgithub%2F&limit=1";
        let (status, first) = read(format!("/v1/client/resources?{filters}")).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["value"]["filters"], json!({"opened_by":"agent/alice", "kind":"vcs.pull-request", "subject_prefix":"resource/github/"}));
        assert_eq!(first["value"]["items"], json!([all["value"]["items"][0].clone()]));
        assert_eq!(first["value"]["page"]["has_more"], true);
        let cursor = urlencoding::encode(first["value"]["page"]["next_cursor"].as_str().unwrap());
        let continuation = format!("/v1/client/resources?{filters}&cursor={cursor}");
        state.store.append_claim(&ClaimInput {
            subject: "custom/test/unrelated".into(),
            kind: "custom.test.marker".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();
        let (status, second) = read(continuation.clone()).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["snapshot"], first["snapshot"]);
        assert_eq!(second["value"]["items"], json!([all["value"]["items"][1].clone()]));
        assert_eq!(second["value"]["page"]["has_more"], false);
        let (status, changed_filter) = read(format!("/v1/client/resources?opened_by=agent%2Fbob&cursor={cursor}")).await;
        assert_eq!(status, StatusCode::GONE, "{changed_filter}");
        assert_eq!(changed_filter["code"], "page-cursor-expired");
        observe("resource/github/a", "vcs.pull-request", json!({"title":"Reassigned", "opened_by":"agent/bob"}));
        let (status, expired) = read(continuation).await;
        assert_eq!(status, StatusCode::GONE, "{expired}");
        assert_eq!(expired["code"], "page-cursor-expired");
        // A later observation cannot take a pull request's opener over (#778 rule 5): it keeps
        // alice and takes the new title.
        let (_, refreshed) = read("/v1/client/resources?opened_by=agent%2Falice".into()).await;
        let items = refreshed["value"]["items"].as_array().unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["resource/github/a", "resource/github/b"]
        );
        assert_eq!(items[0]["facts"]["title"], "Reassigned");
        assert_eq!(items[0]["opened_by"], "agent/alice");
        let (status, invalid) = read("/v1/client/resources?opened_by=person%2Fada".into()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
        assert_eq!(invalid["code"], "validation-failed");
    }

    #[tokio::test]
    async fn resources_list_requires_projection_read_scope() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let credential = "resources-reader";
        let app = super::super::fabric_router(state.clone());
        for (scopes, expected) in [(json!([]), StatusCode::FORBIDDEN), (json!(["read.projections"]), StatusCode::OK)] {
            state.store.append_claim(&ClaimInput {
                subject: "custom/client/resources-reader".into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::from([
                    ("credential_hash".into(), json!(credential_digest(credential))),
                    ("session_actor".into(), json!("client/resources-reader")),
                    ("person_id".into(), json!("person/ada")),
                    ("scopes".into(), scopes),
                    ("expires_at_unix_ms".into(), json!(client_now_ms() as u64 + 60_000)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            }).unwrap();
            let response = app.clone().oneshot(Request::builder()
                .uri("/v1/client/resources")
                .header(AUTHORIZATION, format!("Bearer {credential}"))
                .body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn fleet_operational_commands_are_bounded_and_report_slow_queries() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        state.store.seed_operational_fleet();
        // The old card's complete run array would exceed the negotiated one-megabyte limit.
        let app = super::super::router(state.clone());
        let read = |uri: String| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                    .await
                    .unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                    panic!("{status}: {error}: {}", String::from_utf8_lossy(&bytes))
                });
                assert_eq!(status, StatusCode::OK, "{value}");
                value
            }
        };
        let missions = read("/v1/client/missions?history=true&limit=50".into()).await;
        let item = &missions["value"]["items"][0];
        assert_eq!(item["total_runs"], 2401);
        assert_eq!(item["active_runs"], 601);
        assert_eq!(item["runs"].as_array().unwrap().len(), 3);
        assert_eq!(item["runs_truncated"], true);
        let mut large = vec![item.clone(); 200];
        for card in &mut large {
            for run in card["run_details"].as_array_mut().unwrap() {
                run["requester"] = json!(format!("person/{}", "x".repeat(1990)));
            }
        }
        assert!(bound_mission_cards(&mut large).unwrap());
        assert!(!large.is_empty() && large.len() < 200);
        assert!(serde_json::to_vec(&large).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES - 120_000);
        let overview = read("/v1/mission-overview?mission=mission%2Fexample%2Ffleet".into()).await;
        assert_eq!(overview["value"]["total_runs"], 2401);
        for collection in ["missions", "work"] {
            let page = read(format!(
                "/v1/outcome-history?collection={collection}&since=0&status=failed&limit=50"
            ))
            .await;
            assert_eq!(page["value"]["items"].as_array().unwrap().len(), 50);
            assert_eq!(page["value"]["has_more"], true);
        }
        // The SQL hook sees a real fleet-scale query, without file profiling or diagnostic claims.
        let index = state.store.index().unwrap();
        let _ = state
            .store
            .claims_page(None, None, 0, None, false, 500)
            .unwrap();
        let report = read("/v1/performance".into()).await;
        assert_eq!(report["value"]["window_seconds"], 300);
        assert!(!report["value"]["queries"].as_array().unwrap().is_empty());
        assert!(!report["value"]["requests"].as_array().unwrap().is_empty());
        assert_eq!(
            state.store.index().unwrap(),
            index,
            "operational samples never write graph claims"
        );
        assert!(!report.to_string().contains("person/operator"));
    }

    fn test_state_named(root: &Path, node: &str) -> AppState {
        AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(Notify::new()),
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
        }
    }

    #[tokio::test]
    async fn agent_declarations_require_sensitive_scope_and_select_exact_revision() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let publish = |source: &str| {
            let intent = crate::graph::parse_intent(source, "terminal-test").unwrap();
            let planned = state
                .store
                .mission(
                    &intent,
                    IntentInput {
                        kdl: source.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            assert!(planned.blockers.is_empty(), "{:?}", planned.blockers);
            state
                .store
                .apply(&intent, &planned.subject_tokens, source)
                .unwrap();
        };
        publish(
            "version 2\nagent \"dotfiles/steward\" { workspace \"/tmp\"; command \"true\"; env { TOKEN \"old-secret\" } }",
        );
        let original = state
            .store
            .agent_declaration_revisions("agent/dotfiles/steward")
            .unwrap()[0]
            .clone();
        publish(
            "version 2\nagent \"dotfiles/steward\" { workspace \"/tmp\"; command \"true\"; env { TOKEN \"new-secret\" } }",
        );
        let read = |revision, show_env_values| {
            agent_declaration(
                State(state.clone()),
                Extension(ClientSession::local(Some("person/test")).unwrap()),
                AxumPath("dotfiles/steward".into()),
                Query(AgentDeclarationQuery {
                    revision,
                    show_env_values,
                }),
            )
        };
        for (revision, secret) in [(None, "new-secret"), (Some(original.clone()), "old-secret")] {
            let redacted = read(revision.clone(), false).await.unwrap().0;
            let kdl = redacted["kdl"].as_str().unwrap();
            let parsed = crate::graph::parse_intent(kdl, "terminal-test").unwrap();
            assert_eq!(
                parsed.subjects["agent/dotfiles/steward"].desired,
                redacted["tree"]
            );
            let env = redacted["tree"]["children"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["name"] == "env")
                .unwrap();
            assert_eq!(env["children"][0]["name"], "TOKEN");
            assert_eq!(env["children"][0]["arguments"][0], "<redacted>");
            assert!(!redacted.to_string().contains(secret));
            let visible = read(revision, true).await.unwrap().0;
            assert!(
                visible["kdl"].as_str().unwrap().contains(secret),
                "{visible}"
            );
            assert!(visible["tree"].to_string().contains(secret), "{visible}");
            assert_eq!(visible["revision"], redacted["revision"]);
            assert_eq!(visible["revisions"].as_array().unwrap().len(), 2);
            if secret == "old-secret" {
                assert_eq!(visible["revision"], original);
            }
        }
        assert!(read(Some("not-a-revision".into()), false).await.is_err());
        for revision in [None, Some(original)] {
            for show_env_values in [false, true] {
                assert!(
                    agent_declaration(
                        State(state.clone()),
                        Extension(ClientSession::local(None).unwrap()),
                        AxumPath("dotfiles/steward".into()),
                        Query(AgentDeclarationQuery {
                            revision: revision.clone(),
                            show_env_values,
                        }),
                    )
                    .await
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn observers_subscriptions_and_agentless_gate_kinds_are_projected() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "client-watch-test");
        let source = r#"
version 2
mission "watch-work" state="ready" {
  goal "Project gate types."
  step "watch" { agentless }
  step "flag" {
    agentless
    gate "flag is ready" { field "status" "resource/watch/source" "is" "ready" }
  }
  step "command" {
    agentless
    gate "command succeeds" { exec "true"; host "local"; workspace "/tmp" }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "client-watch-test").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(planned.blockers.is_empty(), "{:?}", planned.blockers);
        state
            .store
            .apply(&intent, &planned.subject_tokens, "client-watch-fixture")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "watch-work".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/alex".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "client-watch-run".into(),
            })
            .unwrap();
        for step in &run.steps {
            state
                .store
                .set_step_state(&step.subject, "ready", None)
                .unwrap();
        }
        let observer_source = r#"
version 2
resource "watch/source" { kind "filesystem.file" }
observer "watch/source" {
  resource "resource/watch/source"
  provider "local.file"
  locator "/tmp/client-watch-source"
  field "status"
}
subscription "watch/source" {
  observer "observer/watch/source"
  to "agent/client-watch-test.worker"
  on "status"
  delivery "message"
}
"#;
        let observer_intent =
            crate::graph::parse_execution_intent(observer_source, "client-watch-test", "watch-run")
                .unwrap();
        state
            .store
            .apply_internal(&observer_intent, "client-watch-observer")
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let observers =
            observer_subscription_resources(&state, "observer", false, &snapshot).unwrap();
        let subscriptions =
            observer_subscription_resources(&state, "subscription", false, &snapshot).unwrap();
        assert_eq!(observers.len(), 1);
        assert_eq!(observers[0]["spec"]["resource"], "resource/watch/source");
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(
            subscriptions[0]["spec"]["observer"],
            "observer/watch-run/watch/source"
        );
        let work = super::super::client_work_resources(
            &state.store,
            None,
            false,
            client_snapshot_time(&snapshot),
            snapshot.store_index,
        )
        .unwrap();
        let gate = |path: &str| {
            work.iter()
                .find(|item| item["mission_run_id"] == run.subject && item["path"] == path)
                .unwrap()["gate_kind"]
                .clone()
        };
        assert_eq!(gate("watch"), "watch");
        assert_eq!(gate("flag"), "predicate");
        assert_eq!(gate("command"), "command");
    }

    #[tokio::test]
    async fn creation_recovers_committed_declarations_before_the_action_receipt() {
        for kind in ["agent.create", "terminal.create"] {
            let root = tempfile::tempdir().unwrap();
            let state = test_state_named(root.path(), "create-recovery");
            let session = ClientSession::local(Some("person/ada")).unwrap();
            let snapshot = new_client_snapshot(&state);
            let parameters = if kind == "agent.create" {
                json!({"name":"worker", "harness":"codex", "workspace":"/tmp", "message":"First"})
            } else {
                json!({"name":"Shell", "cwd":"/tmp"})
            };
            let request = ActionRequest {
                api_version: CLIENT_API_VERSION.into(),
                id: "action/recovery".into(),
                action_type: kind.into(),
                idempotency_key: "creation-recovery-001".into(),
                fence: Fence {
                    snapshot_id: snapshot.id.clone(),
                    ..Default::default()
                },
                parameters,
            };
            let ids = dispatch_action(&state, &snapshot, &session, &request)
                .await
                .unwrap();
            assert_eq!(
                existing_creation(&state, &session, &request).unwrap(),
                Some(ids[0].clone())
            );
            let result = action(
                State(state.clone()),
                Extension(snapshot.clone()),
                Extension(session.clone()),
                Json(request.clone()),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(result["affected_ids"], json!(ids));
            assert_eq!(state.store.desired_subjects().unwrap().len(), 1);
            let mut changed = request.clone();
            changed.parameters["name"] = json!("different");
            assert_eq!(
                existing_creation(&state, &session, &changed)
                    .unwrap_err()
                    .code,
                "idempotency-conflict"
            );
            // Retry after the receipt exists recovers it despite the original stale snapshot.
            let retry = action(
                State(state.clone()),
                Extension(new_client_snapshot(&state)),
                Extension(session),
                Json(request),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(retry["affected_ids"], result["affected_ids"]);
        }
    }

    #[tokio::test]
    async fn typed_agent_stop_down_seat_is_idempotent_and_start_restores_declaration() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let source = "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; restart \"always\" }\n";
        let intent = crate::graph::parse_intent(source, &state.node).unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &preview.subject_tokens, "agent-control-fixture")
            .unwrap();
        let agent = "agent/example/worker";
        let declaration = state.store.agent_declaration(agent, None).unwrap().unwrap();
        let person = ClientSession::local(Some("person/alex")).unwrap();
        let snapshot = new_client_snapshot(&state);
        let stop = st3_client::ActionRequest::agent_stop(
            "action/stop-down-seat",
            "stop-down-seat-key-0001",
            st3_client::Fence {
                snapshot_id: snapshot.id.clone(),
                runtime_desired_revision: Some(declaration.0.clone()),
                ..Default::default()
            },
            st3_client::AgentStopParameters {
                agent: agent.into(),
                reason: Some("maintenance".into()),
            },
        )
        .unwrap();
        let stop: ActionRequest =
            serde_json::from_value(serde_json::to_value(stop).unwrap()).unwrap();
        let submit = |snapshot: ClientSnapshot, session: ClientSession, request| {
            action(
                State(state.clone()),
                Extension(snapshot),
                Extension(session),
                Json(request),
            )
        };
        let first = submit(snapshot.clone(), person.clone(), stop.clone())
            .await
            .unwrap()
            .0;
        assert_eq!(
            state.store.selected_desired_kind(agent).unwrap().as_deref(),
            Some("stop")
        );
        let stopped = state.store.selected_desired_token(agent).unwrap().unwrap();
        let replay = submit(new_client_snapshot(&state), person, stop)
            .await
            .unwrap()
            .0;
        assert_eq!(first["operation_id"], replay["operation_id"]);
        assert_eq!(
            state
                .store
                .selected_desired_token(agent)
                .unwrap()
                .as_deref(),
            Some(stopped.as_str())
        );
        assert_eq!(
            state
                .store
                .agent_declaration(agent, Some(&declaration.0))
                .unwrap()
                .unwrap()
                .1,
            declaration.1
        );
        let snapshot = new_client_snapshot(&state);
        let start = st3_client::ActionRequest::agent_start(
            "action/start-down-seat",
            "start-down-seat-key-0001",
            st3_client::Fence {
                snapshot_id: snapshot.id.clone(),
                runtime_desired_revision: Some(stopped),
                ..Default::default()
            },
            st3_client::AgentStartParameters {
                agent: agent.into(),
            },
        )
        .unwrap();
        let start: ActionRequest =
            serde_json::from_value(serde_json::to_value(start).unwrap()).unwrap();
        // The shared free-mode creation policy permits local agents without authority blocks.
        let mut actor = ClientSession::local(Some("person/alex")).unwrap();
        actor.actor = "agent/example/operator".into();
        actor.authority_actor = actor.actor.clone();
        assert!(require_creation_actor(&actor).is_ok());
        let started = submit(snapshot, actor, start).await.unwrap().0;
        assert_eq!(started["affected_ids"], json!([agent]));
        assert_eq!(
            state.store.selected_desired_kind(agent).unwrap().as_deref(),
            Some("agent")
        );
        let restored = state
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|d| d.subject == agent)
            .unwrap();
        assert_eq!(restored.member.unwrap().host, state.node);
        assert!(
            state
                .store
                .claims_for(agent, Some("intent.desired"))
                .unwrap()
                .iter()
                .any(|c| c.actor.as_deref() == Some("agent/example/operator"))
        );
    }

    #[tokio::test]
    async fn runtime_stop_uses_the_person_and_rejects_a_stale_desired_fence() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "client-control-test");
        let source = "version 2\nagent \"worker\" { workspace \"/tmp\"; command \"true\"; restart \"always\" }\n";
        let intent = crate::graph::parse_intent(source, "client-control-test").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "client-control-agent")
            .unwrap();
        let owner = state
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.kind == "agent")
            .unwrap()
            .subject;
        let incarnation = "client-control-runtime:i1";
        state
            .store
            .append_claim(&ClaimInput {
                subject: owner.clone(),
                kind: "runtime.observed".into(),
                actor: Some(owner.clone()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("client-control-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let snapshot = new_client_snapshot(&state);
        let desired = state.store.selected_desired_token(&owner).unwrap().unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/stop-worker".into(),
            action_type: "runtime.stop".into(),
            idempotency_key: "stop-worker-client-0001".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                runtime_incarnation: Some(incarnation.into()),
                runtime_desired_revision: Some("stale-desired".into()),
                ..Fence::default()
            },
            parameters: json!({"target_id": "runtime/client-control-runtime", "reason": "operator stop"}),
        };
        assert_eq!(
            dispatch_action(&state, &snapshot, &session, &request)
                .await
                .unwrap_err()
                .code,
            "stale-fence"
        );
        let mut current = request;
        current.fence.runtime_desired_revision = Some(desired);
        let typed_fence = st3_client::Fence {
            snapshot_id: snapshot.id.clone(),
            runtime_incarnation: Some(incarnation.into()),
            runtime_desired_revision: current.fence.runtime_desired_revision.clone(),
            ..st3_client::Fence::default()
        };
        let parameters = st3_client::TargetParameters {
            target_id: "runtime/client-control-runtime".into(),
            reason: Some("operator control".into()),
            ..st3_client::TargetParameters::default()
        };
        // Before the generated Rust `Fence` carried the desired revision, these typed requests
        // reached the daemon without it and were refused with 422.
        let restart = st3_client::ActionRequest::runtime_restart(
            "action/restart-worker",
            "restart-worker-client-0001",
            typed_fence.clone(),
            parameters.clone(),
        )
        .unwrap();
        let mut restart: ActionRequest =
            serde_json::from_value(serde_json::to_value(restart).unwrap()).unwrap();
        assert_eq!(
            runtime_control_target(&state, &snapshot, &session, &restart).unwrap()["owner_id"],
            owner
        );
        restart.fence.runtime_desired_revision = None;
        assert_eq!(
            runtime_control_target(&state, &snapshot, &session, &restart)
                .unwrap_err()
                .message,
            "runtime control requires a desired revision fence"
        );
        let stop = st3_client::ActionRequest::runtime_stop(
            "action/stop-worker",
            "stop-worker-client-0001",
            typed_fence,
            parameters,
        )
        .unwrap();
        current = serde_json::from_value(serde_json::to_value(stop).unwrap()).unwrap();
        assert_eq!(
            dispatch_action(&state, &snapshot, &session, &current)
                .await
                .unwrap(),
            vec![owner.clone()]
        );
        assert_eq!(
            state
                .store
                .selected_desired_kind(&owner)
                .unwrap()
                .as_deref(),
            Some("stop")
        );
    }

    #[tokio::test]
    async fn runtime_reset_records_the_run_local_reset_operation() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "client-reset-test");
        let mission_source = "version 2\nmission \"reset-work\" state=\"ready\" { goal \"Reset a runtime.\"; step \"hold\" { agentless } }\n";
        let intent = crate::graph::parse_intent(mission_source, "client-reset-test").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: mission_source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "reset-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "reset-work".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/alex".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "reset-run".into(),
            })
            .unwrap();
        let runtime_source = "version 2\nagent \"worker\" { workspace \"/tmp\"; command \"true\"; restart \"always\" }\n";
        let runtime_intent =
            crate::graph::parse_execution_intent(runtime_source, "client-reset-test", &run.id)
                .unwrap();
        state
            .store
            .apply_internal(&runtime_intent, "reset-runtime")
            .unwrap();
        let owner = runtime_intent
            .subjects
            .keys()
            .find(|id| id.starts_with("agent/"))
            .unwrap()
            .clone();
        let incarnation = "client-reset-runtime:i1";
        state
            .store
            .append_claim(&ClaimInput {
                subject: owner.clone(),
                kind: "runtime.observed".into(),
                actor: Some(owner.clone()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("client-reset-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/reset-worker".into(),
            action_type: "runtime.reset".into(),
            idempotency_key: "reset-worker-client-0001".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                runtime_incarnation: Some(incarnation.into()),
                runtime_desired_revision: state.store.selected_desired_token(&owner).unwrap(),
                ..Fence::default()
            },
            parameters: json!({"target_id": "runtime/client-reset-runtime", "reason": "clear restart limit"}),
        };
        assert_eq!(
            dispatch_action(&state, &snapshot, &session, &request)
                .await
                .unwrap(),
            vec![owner.clone()]
        );
        let reset = state
            .store
            .latest_claim(&owner, Some("runtime.restart-window-reset"))
            .unwrap()
            .unwrap();
        assert_eq!(reset.body["fields"]["reason"], "clear restart limit");
    }

    #[tokio::test]
    async fn client_work_retry_uses_the_person_and_execution_fence() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "client-retry-test");
        let source = "version 2\nmission \"retry-work\" state=\"ready\" { goal \"Retry one check.\"; step \"check\" { agentless; goal \"Check the result.\" } }\n";
        let intent = crate::graph::parse_intent(source, "client-retry-test").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "client-retry-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "retry-work".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/alex".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "client-retry-run".into(),
            })
            .unwrap();
        let step = &run.steps[0];
        state
            .store
            .set_step_state(&step.subject, "failed", Some("check failed"))
            .unwrap();
        let current = state.store.step_run(&step.subject).unwrap().unwrap();
        let snapshot = new_client_snapshot(&state);
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let mut request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/retry-check".into(),
            action_type: "work.retry".into(),
            idempotency_key: "client-retry-check-0001".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                mission_generation: Some(current.generation.clone()),
                step_definition: Some(current.definition_hash.clone()),
                attempt: Some(current.attempt),
                readiness_epoch: Some(u64::from(current.readiness_epoch)),
                ..Fence::default()
            },
            parameters: json!({"target_id": step.subject, "reason": "check host recovered"}),
        };
        request.fence.attempt = Some(current.attempt + 1);
        assert_eq!(
            dispatch_action(&state, &snapshot, &session, &request)
                .await
                .unwrap_err()
                .code,
            "stale-fence"
        );
        request.fence.attempt = Some(current.attempt);
        let affected = dispatch_action(&state, &snapshot, &session, &request)
            .await
            .unwrap();
        assert_eq!(affected, vec![run.subject]);
        let retried = state.store.step_run(&step.subject).unwrap().unwrap();
        assert_eq!(retried.attempt, current.attempt + 1);
    }

    #[test]
    fn machines_report_the_latest_replication_success_while_a_peer_stays_up() {
        const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
        let root = tempfile::tempdir().unwrap();
        let mut state = test_state_named(root.path(), "hub");
        state.configured_peers = vec!["edge".into()];
        state.store.bind_fleet(FLEET).unwrap();
        // The transport comes up once, which records its claim.
        state
            .store
            .record_transport_observation("edge", "up", None, Some(1_000))
            .unwrap();
        // A later exchange succeeds. The status is still up, so no new claim is written.
        let edge = Store::open_memory("edge").unwrap();
        edge.bind_fleet(FLEET).unwrap();
        let exchange = edge
            .export_replication_exchange(FLEET, &crate::model::ReplicationInventory::default())
            .unwrap();
        state
            .store
            .receive_replication_exchange("edge", FLEET, &exchange)
            .unwrap();
        state
            .store
            .record_transport_observation("edge", "up", None, None)
            .unwrap();
        let peer_success = state
            .store
            .replication_peer_last_success("edge")
            .unwrap()
            .unwrap();
        assert!(peer_success > 1_000);

        let session = ClientSession::local(Some("person/alex")).unwrap();
        let machines =
            machine_resources(&state, false, &new_client_snapshot(&state), &session).unwrap();
        let edge = machines
            .iter()
            .find(|machine| machine["host_id"] == "host/edge")
            .unwrap();
        assert_eq!(edge["state"], "reachable");
        assert_eq!(
            edge["transports"][0]["last_success_at"],
            client_timestamp(peer_success)
        );
    }

    /// The runtime, terminal and machine lists read what they show in a fixed number of
    /// statements, however many runtimes there are.
    #[test]
    fn runtime_and_machine_lists_cost_a_fixed_number_of_statements() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let observe = |number: usize| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("agent/listed/runtime-{number}"),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("status".into(), Value::String("running".into())),
                        (
                            "runtime_id".into(),
                            Value::String(format!("listed-{number}")),
                        ),
                        (
                            "incarnation_id".into(),
                            Value::String(format!("listed-{number}:1")),
                        ),
                        ("terminal".into(), Value::Bool(true)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        };
        let statements = |count: usize| {
            let snapshot = new_client_snapshot(&state);
            // The first read reduces each runtime once; count a read at the same snapshot.
            machine_resources(&state, false, &snapshot, &session).unwrap();
            crate::store::STATEMENTS_RUN.with(|run| run.set(0));
            let runtimes = runtime_resources(&state, false, &snapshot, &session).unwrap();
            let runtime_statements = crate::store::STATEMENTS_RUN.with(|run| run.replace(0));
            assert_eq!(runtimes.len(), count);
            for runtime in &runtimes {
                let observed = state
                    .store
                    .claim_by_id(runtime["revision"].as_str().unwrap())
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    runtime["updated_at"],
                    client_timestamp(observed.accepted_at_unix_ms)
                );
            }
            crate::store::STATEMENTS_RUN.with(|run| run.set(0));
            let machines = machine_resources(&state, false, &snapshot, &session).unwrap();
            assert_eq!(machines[0]["runtime_ids"].as_array().unwrap().len(), count);
            let machine_statements = crate::store::STATEMENTS_RUN.with(std::cell::Cell::get);
            (runtime_statements, machine_statements)
        };
        for number in 0..3 {
            observe(number);
        }
        let few = statements(3);
        for number in 3..40 {
            observe(number);
        }
        assert_eq!(statements(40), few);
    }

    /// The operations collection answers while the first diagnostic report since a start is
    /// being made, saying so, and lists the report once it is made.
    #[test]
    fn operations_answer_while_the_first_report_is_made() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let key = Arc::as_ptr(&state.store) as usize;
        // As `start_operation_report` leaves it until its thread has made the report.
        operation_reports().insert(
            key,
            OperationReport {
                store: Arc::downgrade(&state.store),
                checks: None,
                at: Instant::now(),
                refreshing: true,
            },
        );
        crate::store::STATEMENTS_RUN.with(|run| run.set(0));
        let pending = operation_resources(&state, "2026-09-30T00:00:00Z").unwrap();
        assert_eq!(crate::store::STATEMENTS_RUN.with(std::cell::Cell::get), 0);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0]["kind"], "operation");
        assert_eq!(pending[0]["component"], "daemon");
        assert_eq!(pending[0]["severity"], "info");
        assert_eq!(pending[0]["state"], "running");
        assert_eq!(pending[0]["updated_at"], "2026-09-30T00:00:00Z");
        refresh_operation_report(&state, key);
        let report = operation_resources(&state, "2026-09-30T00:00:00Z").unwrap();
        assert!(
            report.iter().all(|item| item["state"] != "running"),
            "{report:?}"
        );
        assert!(
            report
                .iter()
                .any(|item| item["revision"] == "claim-store:pass"),
            "{report:?}"
        );

        // A started report is made on its own thread, and a second start keeps it.
        let other = tempfile::tempdir().unwrap();
        let started = test_state(other.path());
        start_operation_report(&started);
        let deadline = Instant::now() + Duration::from_secs(30);
        while operation_checks(&started).unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "the started report was never made"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let made = operation_checks(&started).unwrap().unwrap();
        start_operation_report(&started);
        assert!(Arc::ptr_eq(
            &made,
            &operation_checks(&started).unwrap().unwrap()
        ));
    }

    #[test]
    fn attention_events_name_the_attention_they_change() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let before = state.store.index().unwrap();
        state
            .store
            .request_attention(
                "attention/sync-demo",
                &crate::model::AttentionRequest {
                    reviewer: "person/alex".into(),
                    title: "Review the invented plan".into(),
                    reason: "A replicated change must refresh Now.".into(),
                    severity: "warning".into(),
                    targets: Vec::new(),
                    actor: "agent/example/example/builder".into(),
                    idempotency_key: "attention-event".into(),
                },
            )
            .unwrap();
        let records = state.store.events_after_bounded(before, 10).unwrap();
        let record = records
            .iter()
            .find(|record| record.subject == "attention/sync-demo")
            .unwrap();
        let (_, resource_ids, _) = safe_event_projection(&state, record);
        assert_eq!(resource_ids, ["attention/sync-demo"]);
    }

    #[test]
    fn pages_say_when_the_host_is_catching_up_with_a_peer() {
        const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
        let root = tempfile::tempdir().unwrap();
        let mut state = test_state_named(root.path(), "hub");
        state.configured_peers = vec!["edge".into()];
        state.store.bind_fleet(FLEET).unwrap();
        let edge = Store::open_memory("edge").unwrap();
        edge.bind_fleet(FLEET).unwrap();
        for index in 0..1_200 {
            edge.append_client_claim(&crate::model::ClaimInput {
                subject: format!("resource/sync-{index}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.test.replication".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        }
        let page = |state: &AppState| {
            super::super::client_page(
                state,
                &new_client_snapshot(state),
                "now",
                Vec::new(),
                &super::super::ClientListQuery::default(),
            )
            .unwrap()
        };
        assert!(page(&state).sync.is_none(), "nothing is measured yet");
        let pull = |state: &AppState| {
            let summary = state.store.export_replication_summary(FLEET).unwrap();
            // Classic 512-envelope pages, so the backlog takes more than one exchange.
            let inventory = crate::model::ReplicationInventory {
                accepts: None,
                ..summary.inventory
            };
            let exchange = edge.export_replication_exchange(FLEET, &inventory).unwrap();
            state
                .store
                .receive_replication_exchange("edge", FLEET, &exchange)
                .unwrap();
            exchange.envelopes.len() as u64
        };

        let received = pull(&state);
        let sync = page(&state).sync.expect("the hub is catching up");
        assert_eq!(sync.state, "catching-up");
        let [peer] = sync.peers.as_slice() else {
            panic!("one peer is ahead: {:?}", sync.peers);
        };
        assert_eq!(peer.host_id, "host/edge");
        let total = edge.replication_inventory().unwrap().envelopes.len() as u64;
        assert_eq!(peer.peer_only_envelopes, total - received);
        assert_eq!(
            peer.last_exchange_at,
            state
                .store
                .replication_peer_last_success("edge")
                .unwrap()
                .map(client_timestamp)
        );
        let json = serde_json::to_value(page(&state)).unwrap();
        assert_eq!(json["sync"]["peers"][0]["host_id"], "host/edge");

        // Once the rest fits in one exchange, pages stop carrying the notice.
        pull(&state);
        let json = serde_json::to_value(page(&state)).unwrap();
        assert!(json.get("sync").is_none(), "{json}");
    }

    #[test]
    fn a_dial_out_member_is_dial_out_and_an_ended_member_is_history() {
        const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abd";
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "hub");
        state.store.bind_fleet(FLEET).unwrap();
        let anchor = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        state.store.pin_fleet_anchor(anchor.public()).unwrap();
        state.store.set_member_key(Some(anchor.clone())).unwrap();
        let admit = |name: &str, key: &str, via: &str, mode: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("host/{name}"),
                    kind: "fleet.member-admitted".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("fleet_id".into(), Value::String(FLEET.into())),
                        ("member_key".into(), Value::String(key.into())),
                        ("via".into(), Value::String(via.into())),
                        ("mode".into(), Value::String(mode.into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        };
        admit("hub", anchor.public(), "anchor", "listening");
        admit("laptop", "laptop-key", "invite", "dial-out");
        admit("gone", "gone-key", "invite", "listening");
        state
            .store
            .append_claim(&ClaimInput {
                subject: "host/gone".into(),
                kind: "fleet.member-removed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("member_key".into(), Value::String("gone-key".into())),
                    ("high_water".into(), Value::from(0)),
                    ("reason".into(), Value::String("test".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        // An old observation says the laptop was down; it no longer decides anything.
        state
            .store
            .record_transport_observation("laptop", "down", Some("asleep"), None)
            .unwrap();

        let session = ClientSession::local(Some("person/alex")).unwrap();
        let machines =
            machine_resources(&state, false, &new_client_snapshot(&state), &session).unwrap();
        let laptop = machines
            .iter()
            .find(|machine| machine["host_id"] == "host/laptop")
            .expect("the dial-out member is a current machine");
        assert_eq!(laptop["state"], "last-seen");
        assert_eq!(laptop["transports"][0]["status"], "last-seen");
        assert!(
            machines
                .iter()
                .all(|machine| machine["host_id"] != "host/gone"),
            "an ended member is history, not a current machine"
        );
    }

    #[test]
    fn device_projection_uses_paired_device_name() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let subject = "custom/client/pairing-named";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-begun".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    ("pairing_id".into(), Value::String("pairing/named".into())),
                    ("device_name".into(), Value::String("Alex's iPhone".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    ("device_id".into(), Value::String("device/named".into())),
                    ("person_id".into(), Value::String("person/alex".into())),
                    (
                        "session_actor".into(),
                        Value::String("person/alex/session/named".into()),
                    ),
                    (
                        "expires_at_unix_ms".into(),
                        Value::from(client_now_ms() as u64 + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let resources =
            device_resources(&state, &new_client_snapshot(&state), "person/alex").unwrap();
        assert_eq!(resources[0]["name"], "Alex's iPhone");
    }

    #[test]
    fn device_projection_finds_new_pairings_after_unrelated_history() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let append = |subject: &str, kind: &str, fields: Value| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: kind.into(),
                    actor: Some("person/alex".into()),
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        let seed = append(
            "message/history-seed",
            "message.sent",
            json!({
                "from":"person/alex", "to":"person/blair", "content":"old history", "status":"sent"
            }),
        );
        // Make a mature log without running 100,000 separate writer transactions. These
        // unrelated fixture rows are never replicated or used by a projection.
        let connection = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
        smallclaims::store::configure_projection_writer(&connection).unwrap();
        connection
            .execute(
                "WITH RECURSIVE numbers(n) AS (
                 SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<100000
             )
             INSERT INTO claims(id, batch_id, subject, kind, origin, actor, body,
                                predecessors, accepted_at_unix_ms)
             SELECT printf('%064x', n), c.batch_id, c.subject, c.kind, c.origin,
                    c.actor, c.body, c.predecessors, c.accepted_at_unix_ms
             FROM numbers CROSS JOIN claims c WHERE c.id=?1",
                [&seed.id],
            )
            .unwrap();
        let subject = "custom/client/pairing-new-phone";
        append(
            subject,
            "custom.client.pairing-begun",
            json!({"device_name":"Alex's new phone"}),
        );
        let completed = append(
            subject,
            "custom.client.pairing-completed",
            json!({
                "device_id":"device/new-phone", "person_id":"person/alex",
                "session_actor":"person/alex/session/new-phone", "scopes":["control.messages"],
                "expires_at_unix_ms": client_now_ms() as u64 + 60_000,
            }),
        );
        let snapshot = new_client_snapshot(&state);
        let revoked = append(
            subject,
            "custom.client.pairing-revoked",
            json!({"device_id":"device/new-phone"}),
        );
        let resources = device_resources(&state, &snapshot, "person/alex").unwrap();
        assert_eq!(
            resources.len(),
            1,
            "new pairings must survive unrelated history"
        );
        assert_eq!(resources[0]["name"], "Alex's new phone");
        assert_eq!(resources[0]["revision"], completed.id);
        assert_eq!(
            resources[0]["state"], "active",
            "a later revocation cannot change an older snapshot"
        );
        let current =
            device_resources(&state, &new_client_snapshot(&state), "person/alex").unwrap();
        assert_eq!(current[0]["revision"], revoked.id);
        assert_eq!(current[0]["state"], "revoked");
        assert!(
            device_resources(&state, &snapshot, "person/blair")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn device_projection_keeps_pairings_across_history_pages() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        for index in 0..300 {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("custom/client/pairing-{index}"),
                    kind: "custom.client.pairing-completed".into(),
                    actor: Some("person/alex".into()),
                    fields: serde_json::from_value(json!({
                        "device_id":format!("device/{index:03}"),
                        "person_id": if index % 2 == 0 { "person/alex" } else { "person/blair" },
                        "session_actor":format!("person/alex/session/{index}"),
                        "expires_at_unix_ms":client_now_ms() as u64 + 60_000,
                    }))
                    .unwrap(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/pairing-0".into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([("device_id".into(), json!("device/000"))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let resources = device_resources(&state, &snapshot, "person/alex").unwrap();
        assert_eq!(resources.len(), 150);
        assert_eq!(resources[0]["id"], "device/000");
        assert_eq!(resources[0]["state"], "revoked");
        assert_eq!(resources[149]["id"], "device/298");
        assert!(
            resources
                .iter()
                .all(|device| device["person_id"] == "person/alex")
        );
        let other = device_resources(&state, &snapshot, "person/blair").unwrap();
        assert_eq!(other.len(), 150);
        assert!(other.iter().all(|device| device["state"] == "active"));
    }

    #[test]
    fn steady_errors_preserve_store_conflicts_and_distinguish_terminal_lifetime() {
        let conflict = ApiError::internal(anyhow::Error::new(St3Error::new(
            "stale-subject",
            "the resource changed",
        )));
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        let envelope = client_error_envelope(
            conflict.status,
            &json!({"code":conflict.code,"message":conflict.message}),
            "request/test",
        );
        assert_eq!(envelope["code"], "stale-fence");
        assert_eq!(envelope["retryable"], false);
        for (code, status, retryable) in [
            ("terminal-ended", StatusCode::GONE, false),
            (
                "terminal-unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
                true,
            ),
            ("remote-unavailable", StatusCode::SERVICE_UNAVAILABLE, true),
            ("page-cursor-expired", StatusCode::GONE, true),
            ("validation-failed", StatusCode::UNPROCESSABLE_ENTITY, false),
        ] {
            let envelope = client_error_envelope(
                status,
                &json!({"code":code,"message":"test"}),
                "request/test",
            );
            assert_eq!(envelope["code"], code);
            assert_eq!(envelope["retryable"], retryable);
        }
    }

    #[tokio::test]
    async fn steady_actions_survive_churn_and_replay_with_fresh_fences_without_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let snapshot = new_client_snapshot(&state);
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/steady-send".into(),
            action_type: "message.send".into(),
            idempotency_key: "steady-send-idempotency-key".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                ..Default::default()
            },
            parameters: json!({"to":"person/blair","content":"Send once despite unrelated activity."}),
        };
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/unrelated".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), json!("vanished"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let submit = |request: ActionRequest| {
            action(
                State(state.clone()),
                Extension(new_client_snapshot(&state)),
                Extension(session.clone()),
                Json(request),
            )
        };
        let (first, concurrent) = tokio::join!(submit(request.clone()), submit(request.clone()));
        let first = first.unwrap().0;
        assert_eq!(concurrent.unwrap().0["affected_ids"], first["affected_ids"]);
        let mut fresh = request.clone();
        fresh.fence.snapshot_id = new_client_snapshot(&state).id;
        // A completed retry is answered before testing even an obsolete resource fence.
        fresh
            .fence
            .subject_revisions
            .insert("agent/unrelated".into(), "obsolete".into());
        let replay = submit(fresh.clone()).await.unwrap().0;
        assert_eq!(replay["operation_id"], first["operation_id"]);
        assert_eq!(replay["affected_ids"], first["affected_ids"]);
        assert_eq!(
            state
                .store
                .claims_for_kind_at("message.sent", None, true, 100)
                .unwrap()
                .claims
                .len(),
            1
        );
        fresh.parameters["content"] = json!("Different mutation");
        assert_eq!(
            submit(fresh).await.unwrap_err().code,
            "idempotency-conflict"
        );
        let mut other = request.clone();
        other.idempotency_key = "steady-send-foreign-snapshot".into();
        other.fence.snapshot_id = snapshot.id.replace("snapshot/", "snapshot/foreign-");
        assert_eq!(submit(other).await.unwrap_err().code, "stale-fence");
    }

    #[test]
    fn paired_authentication_uses_pairing_claims_and_honors_revocation() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let subject = "custom/client/pairing-auth-test";
        let credential = "pairing-auth-test-secret";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    (
                        "credential_hash".into(),
                        Value::String(credential_digest(credential)),
                    ),
                    (
                        "session_actor".into(),
                        Value::String("client/test-session".into()),
                    ),
                    ("person_id".into(), Value::String("person/alex".into())),
                    ("scopes".into(), json!(["read.projections"])),
                    (
                        "expires_at_unix_ms".into(),
                        json!(client_now_ms() as u64 + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let request = Request::builder()
            .uri("/v1/client/agents")
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            authenticate(&state, &request, "fabric-loopback")
                .unwrap()
                .actor,
            "client/test-session"
        );
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(authenticate(&state, &request, "fabric-loopback").is_err());
    }

    #[test]
    fn default_mission_list_hides_loop_round_definitions_and_counts_active_runs() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "loop-node");
        let source = r#"version 2
mission "example/looped" state="ready" {
  goal "Repeat a bounded round."
  concurrent-runs max=2
  completion { when "all-steps-exhausted" }
  loop "improve" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      step "work" { agentless; goal "Complete round ${loop.round}." }
    }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "loop-node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: Some("looped.kdl".into()),
                },
            )
            .unwrap();
        state
            .store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                "publish-looped",
                Some("person/operator"),
            )
            .unwrap();
        let start = |key: &str| {
            state
                .store
                .create_mission_run(&crate::model::MissionRunRequest {
                    mission: "example/looped".into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/operator".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("looped-{key}"),
                })
                .unwrap()
        };
        let first = start("first");
        let second = start("second");
        state
            .store
            .set_mission_run_state(&first.id, "cancelled", "terminal", Some("no longer needed"))
            .unwrap();

        let internal = |resources: &[Value]| {
            resources
                .iter()
                .filter(|value| {
                    value["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("mission/__st3/"))
                })
                .count()
        };
        let history =
            mission_resources(&state.store, state.store.index().unwrap(), true, None).unwrap();
        assert!(internal(&history) > 0, "{history:?}");
        let current =
            mission_resources(&state.store, state.store.index().unwrap(), false, None).unwrap();
        assert_eq!(internal(&current), 0, "{current:?}");
        let looped = current
            .iter()
            .find(|value| value["id"] == "mission/example/looped")
            .unwrap();
        assert_eq!(looped["runs"].as_array().unwrap().len(), 2);
        assert_eq!(looped["active_runs"], 1);
        assert_eq!(looped["run_details"].as_array().unwrap().len(), 2);
        let current_run = looped["run_details"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["status"] == "running")
            .unwrap();
        assert_eq!(current_run["requester"], "person/operator");
        assert_eq!(current_run["progress"]["total"], 1);
        assert_eq!(current_run["current_steps"].as_array().unwrap().len(), 0);
        assert!(current_run["state_since"].is_string());
        assert!(current_run["outcome"].is_null());

        state
            .store
            .set_mission_run_outcome(
                &first.subject,
                "completed",
                "person/operator",
                "its work shipped before it was cancelled",
                "looped-first-outcome",
            )
            .unwrap();
        let current =
            mission_resources(&state.store, state.store.index().unwrap(), false, None).unwrap();
        let looped = current
            .iter()
            .find(|value| value["id"] == "mission/example/looped")
            .unwrap();
        let finished = looped["run_details"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["id"] == first.subject)
            .unwrap();
        assert_eq!(finished["status"], "completed");
        assert_eq!(finished["must_act"], "nobody");
        assert_eq!(finished["outcome"]["status"], "completed");
        assert_eq!(finished["outcome"]["previous_status"], "cancelled");
        assert_eq!(finished["outcome"]["actor"], "person/operator");
        assert_eq!(
            finished["outcome"]["reason"],
            "its work shipped before it was cancelled"
        );
        assert_eq!(finished["state_since"], finished["outcome"]["at"]);

        // Retired once no run is open, the mission leaves the current view at once, even with
        // a run that ended moments ago, and its history names it retired.
        state
            .store
            .set_mission_run_state(&second.id, "failed", "terminal", Some("a check failed"))
            .unwrap();
        state
            .store
            .retire_mission("example/looped", "person/operator", "retire-looped")
            .unwrap();
        let current =
            mission_resources(&state.store, state.store.index().unwrap(), false, None).unwrap();
        assert!(
            current
                .iter()
                .all(|value| value["id"] != "mission/example/looped")
        );
        let history =
            mission_resources(&state.store, state.store.index().unwrap(), true, None).unwrap();
        let retired = history
            .iter()
            .find(|value| value["id"] == "mission/example/looped")
            .unwrap();
        assert_eq!(retired["state"], "retired");
    }

    /// A mission's detail reads only its own runs, seats and states, and says what the whole
    /// history says about them.
    #[test]
    fn a_mission_detail_matches_the_mission_history() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "detail-node");
        for mission in ["example/detail", "example/neighbor"] {
            let source = format!(
                "version 2\nmission \"{mission}\" state=\"ready\" {{\n  goal \"Finish.\"\n  concurrent-runs max=10\n  step \"do\" {{ assigned-to \"agent/doer\" }}\n}}\n"
            );
            let intent = crate::graph::parse_intent(&source, "detail-node").unwrap();
            let preview = state
                .store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.clone(),
                        source_name: None,
                    },
                )
                .unwrap();
            state
                .store
                .apply_as(
                    &intent,
                    &preview.subject_tokens,
                    &format!("publish-{mission}"),
                    Some("person/operator"),
                )
                .unwrap();
            for run in 0..2 {
                let view = state
                    .store
                    .create_mission_run(&crate::model::MissionRunRequest {
                        mission: mission.into(),
                        revision: None,
                        workspace: root.path().display().to_string(),
                        requester: Some("person/operator".into()),
                        mode: None,
                        inputs: BTreeMap::new(),
                        idempotency_key: format!("{mission}-{run}"),
                    })
                    .unwrap();
                if run == 0 {
                    for phase in ["cleanup-completed", "terminal"] {
                        state
                            .store
                            .set_mission_run_state(&view.id, "completed", phase, Some("done"))
                            .unwrap();
                    }
                }
            }
        }
        let index = state.store.index().unwrap();
        let history = mission_resources(&state.store, index, true, None).unwrap();
        for mission in ["mission/example/detail", "mission/example/neighbor"] {
            let listed = history.iter().find(|item| item["id"] == mission).unwrap();
            let detail = mission_resources(&state.store, index, true, Some(mission)).unwrap();
            assert_eq!(detail.len(), 1);
            let detail = &detail[0];
            for field in [
                "runs",
                "state",
                "must_act",
                "active_runs",
                "usage",
                "revision",
            ] {
                assert_eq!(detail[field], listed[field], "{mission} {field}");
            }
            let runs = |item: &Value| {
                item["run_details"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|run| {
                        json!([
                            run["id"],
                            run["status"],
                            run["state_since"],
                            run["outcome"],
                            run["must_act"]
                        ])
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(runs(detail), runs(listed), "{mission}");
        }
    }

    /// Page reads of the operations collection serve the daemon's last diagnostic report, whose
    /// checks read the whole store, instead of running every check on every read.
    #[test]
    fn operations_serve_the_last_diagnostic_report() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "operations-node");
        let first = operation_checks(&state).unwrap().unwrap();
        let second = operation_checks(&state).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(
            first
                .iter()
                .any(|check| check.name == "operation-projection")
        );
        let other = test_state_named(&root.path().join("other"), "operations-other");
        assert!(!Arc::ptr_eq(
            &first,
            &operation_checks(&other).unwrap().unwrap()
        ));
    }

    /// A fleet larger than the tree lists shows the first items and says what it left out, rather
    /// than failing the whole tree.
    #[test]
    fn the_missions_tree_lists_a_bounded_part_of_a_large_fleet() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "tree-node");
        for mission in ["example/first", "example/second", "example/third"] {
            let source = format!(
                "version 2\nmission \"{mission}\" state=\"ready\" {{\n  goal \"Wait to start.\"\n}}\n"
            );
            let intent = crate::graph::parse_intent(&source, "tree-node").unwrap();
            let preview = state
                .store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.clone(),
                        source_name: None,
                    },
                )
                .unwrap();
            state
                .store
                .apply_as(
                    &intent,
                    &preview.subject_tokens,
                    &format!("publish-{mission}"),
                    Some("person/operator"),
                )
                .unwrap();
        }
        let index = state.store.index().unwrap();
        let whole = missions_tree_value(&state.store, "now", index).unwrap();
        assert_eq!(whole["unstarted_missions"].as_array().unwrap().len(), 3);
        assert_eq!(whole["truncated"], json!({}));
        let bounded = missions_tree_value_within(&state.store, "now", index, 2, 1000).unwrap();
        assert_eq!(
            bounded["unstarted_missions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|mission| mission["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["mission/example/first", "mission/example/second"]
        );
        assert_eq!(
            bounded["truncated"],
            json!({"unstarted_missions": {"shown": 2, "total": 3}})
        );
    }

    /// A mission detail and the missions tree show each step's state and latest progress
    /// without reading the timing, wake and definition history a work view reads for every
    /// step: they enrich no step, and they show what the enriched runs show.
    #[test]
    fn mission_detail_and_the_tree_read_no_step_history() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "steps-node");
        let publish = |source: &str, key: &str| {
            let intent = crate::graph::parse_intent(source, "steps-node").unwrap();
            let preview = state
                .store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            state
                .store
                .apply_as(
                    &intent,
                    &preview.subject_tokens,
                    key,
                    Some("person/operator"),
                )
                .unwrap();
        };
        publish(
            r#"version 2
mission "example/steps" state="ready" {
  goal "Build and review."
  concurrent-runs max=10
  step "build" { assigned-to "agent/builder" }
  step "review" { assigned-to "agent/reviewer" }
}"#,
            "publish-steps",
        );
        for (mission, state_name) in [("example/waiting", "ready"), ("example/drafted", "draft")] {
            publish(
                &format!(
                    "version 2\nmission \"{mission}\" state=\"{state_name}\" {{\n  goal \"Wait to start.\"\n}}\n"
                ),
                &format!("publish-{mission}"),
            );
        }
        let mut runs = Vec::new();
        for run in 0..2 {
            let view = state
                .store
                .create_mission_run(&crate::model::MissionRunRequest {
                    mission: "example/steps".into(),
                    revision: None,
                    workspace: root.path().display().to_string(),
                    requester: Some("person/operator".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("steps-{run}"),
                })
                .unwrap();
            let build = view.steps.iter().find(|step| step.step == "build").unwrap();
            let (build, builder) = (
                build.subject.clone(),
                build
                    .assigned_to
                    .clone()
                    .expect("the build step is assigned"),
            );
            state.store.set_step_state(&build, "ready", None).unwrap();
            // A seat holds one step at a time: the first run's is claimed and under way.
            if run == 0 {
                let request = |summary: Option<&str>, key: &str| crate::model::WorkRequest {
                    actor: Some(builder.clone()),
                    incarnation: Some("builder-1".into()),
                    summary: summary.map(str::to_owned),
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: key.into(),
                };
                state
                    .store
                    .work_action(&build, "claim", &request(None, "claim"))
                    .unwrap();
                state
                    .store
                    .work_action(
                        &build,
                        "progress",
                        &request(Some("Half built."), "progress"),
                    )
                    .unwrap();
            }
            runs.push(view.subject);
        }
        let index = state.store.index().unwrap();

        crate::store::STEPS_ENRICHED.with(|enriched| enriched.set(0));
        let detail =
            mission_resources(&state.store, index, true, Some("mission/example/steps")).unwrap();
        let tree = missions_tree_value(&state.store, "now", index).unwrap();
        assert_eq!(crate::store::STEPS_ENRICHED.with(std::cell::Cell::get), 0);

        let shown = |run: &crate::model::MissionRunView| {
            run.steps
                .iter()
                .map(|step| {
                    json!([
                        step.subject,
                        step.status,
                        step.claimant,
                        step.assigned_to,
                        step.attempt,
                        step.blocked_reason,
                        step.blockers,
                        step.progress_summary,
                        step.progress_at_unix_ms,
                        step.completion_summary,
                        step.updated_at_unix_ms,
                    ])
                })
                .collect::<Vec<_>>()
        };
        for run in &runs {
            let enriched = state.store.mission_run(run).unwrap().unwrap();
            let light = state.store.mission_run_steps(run, true).unwrap().unwrap();
            assert_eq!(shown(&light), shown(&enriched), "{run}");
        }
        let claimed = state.store.mission_run(&runs[0]).unwrap().unwrap();
        assert!(claimed.steps.iter().any(|step| step.status == "working"
            && step.progress_summary.as_deref() == Some("Half built.")));
        let details = detail[0]["run_details"].as_array().unwrap();
        assert_eq!(details.len(), 2);
        let progress = details
            .iter()
            .map(|run| (run["id"].as_str().unwrap(), run["last_progress"].clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(progress[runs[0].as_str()], "Half built.");
        assert_eq!(progress[runs[1].as_str()], Value::Null);
        let tree_runs = tree["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| {
                let steps = run["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|step| json!([step["id"], step["state"]]))
                    .collect::<Vec<_>>();
                (run["id"].as_str().unwrap().to_owned(), steps)
            })
            .collect::<BTreeMap<_, _>>();
        let enriched_runs = runs
            .iter()
            .map(|run| {
                let steps = state
                    .store
                    .mission_run(run)
                    .unwrap()
                    .unwrap()
                    .steps
                    .iter()
                    .map(|step| json!([step.subject, client_work_state(&step.status)]))
                    .collect::<Vec<_>>();
                (run.clone(), steps)
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(tree_runs, enriched_runs);
        // The tree's unstarted missions are the current list's ready and draft missions that
        // never ran.
        let mut unstarted = mission_resources(&state.store, index, false, None)
            .unwrap()
            .into_iter()
            .filter(|mission| {
                matches!(mission["state"].as_str(), Some("ready" | "draft"))
                    && mission["runs"].as_array().is_some_and(Vec::is_empty)
            })
            .map(|mission| json!({"id": mission["id"], "title": mission["title"], "state": mission["state"]}))
            .collect::<Vec<_>>();
        unstarted.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        assert_eq!(unstarted.len(), 2);
        assert_eq!(tree["unstarted_missions"], json!(unstarted));
    }

    #[test]
    fn mission_resources_include_published_definitions_without_runs() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "zero-run-node");
        let source = r#"version 2
mission "example/zero-run" state="ready" {
  goal "Remain visible before the first run starts."
}
"#;
        let intent = crate::graph::parse_intent(source, "zero-run-node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: Some("zero-run.kdl".into()),
                },
            )
            .unwrap();
        state
            .store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                "publish-zero-run",
                Some("person/operator"),
            )
            .unwrap();

        let resources =
            mission_resources(&state.store, state.store.index().unwrap(), true, None).unwrap();
        let mission = resources
            .iter()
            .find(|value| value["id"] == "mission/example/zero-run")
            .expect("the zero-run definition is listed");
        assert_eq!(mission["state"], "ready");
        assert_eq!(mission["runs"], json!([]));
        let tree = missions_tree_value(&state.store, "now", state.store.index().unwrap()).unwrap();
        assert!(
            tree["unstarted_missions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| {
                    item["id"] == "mission/example/zero-run" && item["state"] == "ready"
                })
        );
        assert_eq!(mission["operational"]["actionable"], true);
        assert!(mission["visualization"].is_null());
        assert_eq!(
            mission["mission_revision"],
            intent.missions["example/zero-run"].revision
        );
        let details = mission_resources(
            &state.store,
            state.store.index().unwrap(),
            true,
            Some("mission/example/zero-run"),
        )
        .unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(
            details[0]["visualization"]["mission"],
            "mission/example/zero-run"
        );

        let retired_source = source.replace("state=\"ready\"", "state=\"retired\"");
        let retired = crate::graph::parse_intent(&retired_source, "zero-run-node").unwrap();
        let retired_preview = state
            .store
            .mission(
                &retired,
                crate::model::IntentInput {
                    kdl: retired_source,
                    source_name: Some("retired-zero-run.kdl".into()),
                },
            )
            .unwrap();
        state
            .store
            .apply_as(
                &retired,
                &retired_preview.subject_tokens,
                "retire-zero-run",
                Some("person/operator"),
            )
            .unwrap();
        let current =
            mission_resources(&state.store, state.store.index().unwrap(), false, None).unwrap();
        assert!(
            current
                .iter()
                .all(|value| value["id"] != "mission/example/zero-run")
        );
        let retired_resources =
            mission_resources(&state.store, state.store.index().unwrap(), true, None).unwrap();
        let retired_mission = retired_resources
            .iter()
            .find(|value| value["id"] == "mission/example/zero-run")
            .unwrap();
        assert_eq!(retired_mission["state"], "retired");
        assert_eq!(retired_mission["operational"]["actionable"], false);
    }

    #[tokio::test]
    async fn a_saved_native_session_import_declares_one_durable_resuming_seat() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let transcript = home.join(".codex/sessions/2026/09/21/import.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-09-21T08:00:00Z",
                    "payload": {
                        "id": "native-import-test",
                        "cwd": workspace,
                        "source": "test"
                    }
                })
            ),
        )
        .unwrap();
        let external = crate::external_sessions::discover_fresh(Some(&home), true)
            .unwrap()
            .sessions
            .into_iter()
            .find(|item| item.native_id == "native-import-test")
            .unwrap();
        assert!(external.process.is_none());

        let mut state = test_state_named(root.path(), "import-test");
        state.native_session_home = Some(home);
        let session = ClientSession::local(Some("person/tester")).unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/import-test".into(),
            action_type: "session.import".into(),
            idempotency_key: "session-import-test-0001".into(),
            fence: Fence {
                snapshot_id: new_client_snapshot(&state).id,
                subject_revisions: BTreeMap::from([(
                    external.id.clone(),
                    external.revision.clone(),
                )]),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: None,
                runtime_desired_revision: None,
                terminal_sequence: None,
                preview_token: None,
            },
            parameters: json!({"target_id": external.id}),
        };

        // `st3 import run` submits through the action handler, so the generic fence check must
        // read a native session's revision from discovery rather than from the claim store.
        let mut changed = request.clone();
        changed.idempotency_key = "session-import-test-stale".into();
        changed
            .fence
            .subject_revisions
            .insert(external.id.clone(), "an-older-discovery".into());
        let error = action(
            State(state.clone()),
            Extension(new_client_snapshot(&state)),
            Extension(session.clone()),
            Json(changed),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "stale-fence");

        let response = action(
            State(state.clone()),
            Extension(new_client_snapshot(&state)),
            Extension(session.clone()),
            Json(request),
        )
        .await
        .expect("a current native session revision passes the action fence");
        let affected = response.0["affected_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(affected.len(), 2);
        assert!(affected[1].starts_with("agent/import/codex/"));
        assert!(state.store.active_mission_runs().unwrap().is_empty());
        let imported = state
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == affected[1])
            .expect("the imported durable seat is declared");
        assert_eq!(imported.kind, "agent");
        assert!(imported.owner_run.is_none());
        let session_file = state
            .store
            .latest_claim(&affected[1], Some("harness.session-file"))
            .unwrap()
            .expect("the native session identity is durable graph state");
        assert_eq!(
            session_file.body["fields"]["session_id"],
            "native-import-test"
        );
        assert_eq!(session_file.body["fields"]["harness"], "codex");
        assert_eq!(session_file.body["fields"]["agent"], affected[1]);
        assert_eq!(session_file.body["fields"]["source_session"], external.id);
        assert_eq!(
            session_file.body["fields"]["discovery_revision"],
            external.revision
        );
    }

    #[test]
    fn client_events_are_redacted_timestamped_invalidations_with_retention_fences() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "event-node");
        for record in [
            EventRecord {
                store_index: 7,
                kind: "custom.client.pairing-completed".into(),
                subject: "custom/client/pairing-secret".into(),
                body: json!({"fields": {"credential": "PAIRING-PLAINTEXT", "code": "123456"}}),
            },
            EventRecord {
                store_index: 8,
                kind: "custom.client.terminal-attached".into(),
                subject: "custom/client/terminal-secret".into(),
                body: json!({"fields": {"terminal_id": "terminal/agent/viewer", "capability": "TERMINAL-PLAINTEXT", "stream_url": "?secret=yes"}}),
            },
            EventRecord {
                store_index: 9,
                kind: "message.sent".into(),
                subject: "message/safe-id".into(),
                body: json!({"fields": {"content": "PRIVATE-MESSAGE", "from": "person/alex", "to": "agent/worker", "session_id": "session/current"}}),
            },
        ] {
            let projected = safe_event_projection(&state, &record);
            let encoded = serde_json::to_string(&projected).unwrap();
            assert!(!encoded.contains("PLAINTEXT"));
            assert!(!encoded.contains("PRIVATE-MESSAGE"));
            assert!(!encoded.contains("stream_url"));
            assert!(!encoded.contains("credential"));
        }
        let message = safe_event_projection(
            &state,
            &EventRecord {
                store_index: 9,
                kind: "message.sent".into(),
                subject: "message/safe-id".into(),
                body: json!({"fields": {"session_id": "session/current"}}),
            },
        );
        assert_eq!(message.1, ["message/safe-id", "session/current"]);
        let pairing = safe_event_projection(
            &state,
            &EventRecord {
                store_index: 10,
                kind: "custom.client.pairing-revoked".into(),
                subject: "custom/client/pairing-device".into(),
                body: json!({"fields": {"device_id": "device/example"}}),
            },
        );
        assert_eq!(pairing.0, "capabilities.changed");
        for (index, kind) in [
            "custom.client.terminal-attached",
            "custom.client.terminal-consumed",
            "custom.client.terminal-detached",
        ]
        .into_iter()
        .enumerate()
        {
            let projected = safe_event_projection(
                &state,
                &EventRecord {
                    store_index: 11 + index as u64,
                    kind: kind.into(),
                    subject: "custom/client/terminal-attachment-viewer".into(),
                    body: json!({"fields": {
                        "terminal_id": "terminal/agent/viewer",
                        "attachment_id": "terminal-attachment/viewer"
                    }}),
                },
            );
            assert_eq!(projected.0, "upsert", "{kind}");
            assert_eq!(projected.1, ["terminal/agent/viewer"], "{kind}");
            assert_eq!(
                projected.2["reason"], "terminal-viewer-lifecycle-changed",
                "{kind}"
            );
        }
        let gap = validate_event_cursor("event-node", true, 10, 50, 90).unwrap_err();
        assert_eq!(gap.status, StatusCode::GONE);
        assert_eq!(gap.code, "cursor-gap");
        assert_eq!(gap.details["full_resync"], true);
        assert!(validate_event_cursor("event-node", true, 49, 50, 90).is_ok());

        let accepted = state
            .store
            .append_claim(&ClaimInput {
                subject: "message/original-time".into(),
                kind: "message.sent".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/alex".into())),
                    ("to".into(), Value::String("agent/worker".into())),
                    ("content".into(), Value::String("safe".into())),
                    ("status".into(), Value::String("sent".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("event-original-time".into()),
            })
            .unwrap();
        let snapshot = client_snapshot_at(&state, accepted.store_index);
        assert_eq!(
            snapshot.created_at,
            client_timestamp(accepted.accepted_at_unix_ms)
        );
    }

    #[tokio::test]
    async fn capabilities_and_event_pages_publish_the_same_retained_cursor_floor() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "retention-node");
        let append = |id: &str, key: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
                    kind: "message.sent".into(),
                    actor: Some("person/alex".into()),
                    fields: BTreeMap::from([
                        ("from".into(), Value::String("person/alex".into())),
                        ("to".into(), Value::String("agent/worker".into())),
                        ("content".into(), Value::String("secret".into())),
                        ("status".into(), Value::String("sent".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap()
        };
        let first = append("old", "retention-old");
        let retained = append("retained", "retention-new");
        assert!(
            state
                .store
                .prune_events_before(retained.store_index)
                .unwrap()
                > 0
        );
        let floor = retained.store_index.saturating_sub(1);
        let session = ClientSession::local(None).unwrap();
        let snapshot = new_client_snapshot(&state);
        let capabilities = client_capabilities(
            State(state.clone()),
            Extension(snapshot),
            Extension(session.clone()),
        )
        .await
        .0;
        let expected = format!("event-cursor/retention-node/{floor}");
        assert_eq!(capabilities["oldest_event_cursor"], expected);

        let gap = events(
            State(state.clone()),
            Extension(session.clone()),
            Query(EventsQuery {
                after: Some(format!(
                    "event-cursor/retention-node/{}",
                    first.store_index.saturating_sub(1)
                )),
                limit: Some(10),
                wait_ms: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(gap.status, StatusCode::GONE);
        let page = events(
            State(state),
            Extension(session),
            Query(EventsQuery {
                after: Some(expected.clone()),
                limit: Some(10),
                wait_ms: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(page["oldest_cursor"], expected);
        assert!(!serde_json::to_string(&page).unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn event_page_without_a_cursor_returns_the_latest_bounded_activity() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "activity-node");
        let mut accepted = Vec::new();
        for id in ["first", "second", "latest"] {
            accepted.push(
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: format!("message/{id}"),
                        kind: "message.sent".into(),
                        actor: Some("person/alex".into()),
                        fields: BTreeMap::from([
                            ("from".into(), Value::String("person/alex".into())),
                            ("to".into(), Value::String("agent/worker".into())),
                            ("content".into(), Value::String(id.into())),
                            ("status".into(), Value::String("sent".into())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("activity-{id}")),
                    })
                    .unwrap(),
            );
        }
        let page = events(
            State(state),
            Extension(ClientSession::local(None).unwrap()),
            Query(EventsQuery {
                after: None,
                limit: Some(2),
                wait_ms: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["items"][0]["sequence"], accepted[1].store_index);
        assert_eq!(page["items"][1]["sequence"], accepted[2].store_index);
        assert_eq!(page["has_more"], false);
    }

    #[tokio::test]
    async fn the_event_feed_carries_local_observations_after_the_claim_they_follow() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "feed-node");
        let owner = "agent/feed-worker";
        let message = |id: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
                    kind: "message.sent".into(),
                    actor: Some("person/alex".into()),
                    fields: BTreeMap::from([
                        ("from".into(), Value::String("person/alex".into())),
                        ("to".into(), Value::String(owner.into())),
                        ("content".into(), Value::String(id.into())),
                        ("status".into(), Value::String("sent".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("feed-{id}")),
                })
                .unwrap()
        };
        let timeline = |entry: &str| {
            let record = state
                .store
                .append_claim(&ClaimInput {
                    subject: owner.into(),
                    kind: "harness.timeline".into(),
                    actor: Some(owner.into()),
                    fields: BTreeMap::from([
                        ("operation".into(), Value::String("append".into())),
                        ("entry_id".into(), Value::String(entry.into())),
                        ("revision".into(), Value::from(1)),
                        ("role".into(), Value::String("assistant".into())),
                        ("entry_type".into(), Value::String("content".into())),
                        ("final".into(), Value::Bool(true)),
                        (
                            "body".into(),
                            json!({"media_type":"text/plain", "text": entry}),
                        ),
                        ("driver".into(), Value::String("codex".into())),
                        ("incarnation_id".into(), Value::String("feed-inc".into())),
                        (
                            "sequence".into(),
                            Value::from(entry[1..].parse::<u64>().unwrap()),
                        ),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("feed-timeline-{entry}")),
                })
                .unwrap();
            crate::store::local_observation_position(&record).unwrap()
        };
        let first = message("first");
        let t1 = timeline("t1");
        let t2 = timeline("t2");
        let second = message("second");
        let t3 = timeline("t3");
        let (s1, s2) = (first.store_index, second.store_index);
        let page = |after: Option<String>, limit: usize| {
            let state = state.clone();
            async move {
                events(
                    State(state),
                    Extension(ClientSession::local(None).unwrap()),
                    Query(EventsQuery {
                        after,
                        limit: Some(limit),
                        wait_ms: None,
                    }),
                )
                .await
                .map(|page| page.0)
            }
        };
        let cursors = |page: &Value| {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["next_cursor"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let cursor = |label: String| format!("event-cursor/feed-node/{label}");

        let all = page(Some(cursor("0".into())), 10).await.unwrap();
        assert_eq!(
            cursors(&all),
            [
                cursor(s1.to_string()),
                cursor(format!("{s1}.{t1}")),
                cursor(format!("{s1}.{t2}")),
                cursor(s2.to_string()),
                cursor(format!("{s2}.{t3}")),
            ]
        );
        let local = &all["items"][1];
        assert_eq!(local["type"], "upsert");
        assert_eq!(local["body"]["reason"], "session-timeline-invalidated");
        assert_eq!(
            local["resource_ids"],
            json!([client_session_id(owner, "feed-inc")])
        );
        assert_eq!(local["sequence"], s1);
        assert_eq!(local["previous_cursor"], cursor(format!("{s1}.{}", t1 - 1)));
        assert_eq!(local["id"], format!("projection-event/feed-node/{s1}.{t1}"));

        let mut after = Some(cursor("0".into()));
        let mut paged = Vec::new();
        loop {
            let current = page(after.clone(), 2).await.unwrap();
            paged.extend(cursors(&current));
            if current["has_more"] != true {
                break;
            }
            after = current["resume_cursor"].as_str().map(str::to_owned);
        }
        assert_eq!(paged, cursors(&all), "pages of two see every event once");

        let from_claim = page(Some(cursor(s1.to_string())), 10).await.unwrap();
        assert_eq!(
            cursors(&from_claim),
            cursors(&all)[1..],
            "a cursor that names only a claim resumes with the local observations after it"
        );
        let replay = page(local["previous_cursor"].as_str().map(str::to_owned), 10)
            .await
            .unwrap();
        assert_eq!(cursors(&replay), cursors(&all)[1..]);
        let tail = page(None, 2).await.unwrap();
        assert_eq!(cursors(&tail), cursors(&all)[3..]);
        assert_eq!(tail["resume_cursor"], cursor(format!("{s2}.{t3}")));
        let malformed = page(Some(cursor(format!("{s1}.x"))), 10).await.unwrap_err();
        assert_eq!(malformed.code, "validation-failed");

        let waiter_state = state.clone();
        let resume = tail["resume_cursor"].as_str().unwrap().to_owned();
        let waiter = tokio::spawn(async move {
            events(
                State(waiter_state),
                Extension(ClientSession::local(None).unwrap()),
                Query(EventsQuery {
                    after: Some(resume),
                    limit: Some(10),
                    wait_ms: Some(1_000),
                }),
            )
            .await
        });
        tokio::task::yield_now().await;
        let t4 = timeline("t4");
        signal_local_change(&state);
        let woke = tokio::time::timeout(Duration::from_millis(250), waiter)
            .await
            .expect("a local observation did not wake the event long poll")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(cursors(&woke), [cursor(format!("{s2}.{t4}"))]);
    }

    #[tokio::test]
    async fn client_event_long_poll_wakes_for_a_new_claim() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "wait-node");
        let waiter_state = state.clone();
        let waiter = tokio::spawn(async move {
            events(
                State(waiter_state),
                Extension(ClientSession::local(None).unwrap()),
                Query(EventsQuery {
                    after: Some("event-cursor/wait-node/0".into()),
                    limit: Some(10),
                    wait_ms: Some(1_000),
                }),
            )
            .await
        });
        tokio::task::yield_now().await;
        state
            .store
            .append_claim(&ClaimInput {
                subject: "message/wake".into(),
                kind: "message.sent".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/alex".into())),
                    ("to".into(), Value::String("agent/worker".into())),
                    ("content".into(), Value::String("wake".into())),
                    ("status".into(), Value::String("sent".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("client-event-wake".into()),
            })
            .unwrap();
        signal_changed(&state);
        let page = tokio::time::timeout(Duration::from_millis(250), waiter)
            .await
            .expect("the client event long poll did not wake")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn idle_conversation_long_polls_never_rebuild_the_timeline() {
        // Budget (idle-cpu findings, #959): a long-poll whose cursor this member gave out, with
        // nothing that concerns the conversation changed since, answers without rebuilding the
        // timeline; only a change pays for a read.
        let root = tempfile::tempdir().unwrap();
        let owner = test_state_named(root.path(), "conversation-budget");
        let agent = "agent/conversation-budget";
        let incarnation = "conversation-budget-runtime:i1";
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("runtime_id".into(), json!("conversation-budget-runtime")),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("terminal".into(), json!(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-budget-runtime".into()),
            })
            .unwrap();
        let session_id = managed_session_id(agent, incarnation);
        let session = ClientSession::local(Some("person/example")).unwrap();
        let rebuilds = || {
            timeline_rebuilds()
                .lock()
                .unwrap()
                .get(&session_id)
                .copied()
                .unwrap_or(0)
        };
        let baseline = conversation_changes_local(&owner, &session, &session_id, None, 0)
            .await
            .unwrap();
        assert_eq!(rebuilds(), 1, "the first read builds the timeline once");
        let mut cursor = baseline["next_cursor"].as_str().unwrap().to_owned();
        // Unrelated commits wake the poll but concern nothing here.
        for poll in 0..5 {
            owner
                .store
                .append_claim(&ClaimInput {
                    subject: format!("agent/someone-else-{poll}"),
                    kind: "runtime.observed".into(),
                    actor: Some(format!("agent/someone-else-{poll}")),
                    fields: BTreeMap::from([("status".into(), json!("running"))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let idle =
                conversation_changes_local(&owner, &session, &session_id, Some(&cursor), 50)
                    .await
                    .unwrap();
            assert!(idle["items"].as_array().unwrap().is_empty());
            cursor = idle["next_cursor"].as_str().unwrap().to_owned();
        }
        assert_eq!(rebuilds(), 1, "idle long-polls rebuilt the timeline");
        // A message to the agent is a change: the next poll reads once and brings it.
        owner
            .store
            .append_claim(&ClaimInput {
                subject: "message/conversation-budget".into(),
                kind: "message.sent".into(),
                actor: Some("person/example".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/example")),
                    ("to".into(), json!(agent)),
                    ("session_id".into(), json!(session_id)),
                    ("content".into(), json!("hello")),
                    ("status".into(), json!("sent")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-budget-message".into()),
            })
            .unwrap();
        let changed = conversation_changes_local(&owner, &session, &session_id, Some(&cursor), 50)
            .await
            .unwrap();
        assert!(!changed["items"].as_array().unwrap().is_empty(), "{changed}");
        assert_eq!(rebuilds(), 2);
        // A cursor this member did not give out (another member's, or one from before a
        // restart) is read as before.
        let unknown = conversation_cursor(&owner, &session_id, 0, 0, 0);
        conversation_changes_local(&owner, &session, &session_id, Some(&unknown), 0)
            .await
            .unwrap();
        assert_eq!(rebuilds(), 3);
    }

    #[tokio::test]
    async fn conversation_changes_resume_across_isolated_nodes_without_idle_data() {
        let owner_root = tempfile::tempdir().unwrap();
        let follower_root = tempfile::tempdir().unwrap();
        let owner = test_state_named(owner_root.path(), "conversation-owner");
        let follower = test_state_named(follower_root.path(), "conversation-follower");
        let agent = "agent/conversation-worker";
        let incarnation = "conversation-runtime:i1";
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("runtime_id".into(), json!("conversation-runtime")),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("terminal".into(), json!(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-runtime".into()),
            })
            .unwrap();
        follower
            .store
            .import_replication(
                "conversation-owner",
                &owner.store.export_replication(0).unwrap(),
            )
            .unwrap();
        let session_id = managed_session_id(agent, incarnation);
        assert_eq!(conversation_session_id(&owner, agent).unwrap(), session_id);
        let session = ClientSession::local(Some("person/example")).unwrap();
        let origin = super::super::managed_session_owner_at(
            &follower.store,
            follower.store.index().unwrap(),
            &session_id,
        )
        .unwrap()
        .unwrap()
        .2
        .unwrap();
        assert_eq!(origin, "conversation-owner");
        let baseline = conversation_changes_local(&owner, &session, &session_id, None, 0)
            .await
            .unwrap();
        let cursor = baseline["next_cursor"].as_str().unwrap().to_owned();
        assert!(baseline["items"].as_array().unwrap().is_empty());
        let idle = conversation_changes_local(&owner, &session, &session_id, Some(&cursor), 100)
            .await
            .unwrap();
        assert!(idle["items"].as_array().unwrap().is_empty());
        let waiting_owner = owner.clone();
        let waiting_session = session.clone();
        let waiting_session_id = session_id.clone();
        let waiting_cursor = cursor.clone();
        let waiting = tokio::spawn(async move {
            conversation_changes_local(
                &waiting_owner,
                &waiting_session,
                &waiting_session_id,
                Some(&waiting_cursor),
                1000,
            )
            .await
            .unwrap()
        });
        tokio::task::yield_now().await;
        owner
            .store
            .append_claim(&ClaimInput {
                subject: "message/conversation-first".into(),
                kind: "message.sent".into(),
                actor: Some("person/example".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/example")),
                    ("to".into(), json!(agent)),
                    ("session_id".into(), json!(session_id)),
                    ("content".into(), json!("first")),
                    ("status".into(), json!("sent")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-first".into()),
            })
            .unwrap();
        assert_eq!(
            conversation_session_id(&owner, "message/conversation-first").unwrap(),
            session_id
        );
        signal_changed(&owner);
        let first = tokio::time::timeout(Duration::from_millis(900), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        let resume = first["next_cursor"].as_str().unwrap();
        owner
            .store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "harness.timeline".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("operation".into(), json!("append")),
                    (
                        "entry_id".into(),
                        json!("timeline-entry/conversation-reply"),
                    ),
                    ("revision".into(), json!(1)),
                    ("role".into(), json!("assistant")),
                    ("entry_type".into(), json!("content")),
                    ("final".into(), json!(true)),
                    (
                        "body".into(),
                        json!({"media_type":"text/plain","text":"reply"}),
                    ),
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("sequence".into(), json!(1)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("conversation-reply".into()),
            })
            .unwrap();
        let replay = conversation_changes_local(&owner, &session, &session_id, Some(resume), 0)
            .await
            .unwrap();
        assert_eq!(replay["items"].as_array().unwrap().len(), 1);
        assert_eq!(replay["items"][0]["body"]["text"], "reply");
        assert!(
            conversation_changes_local(&follower, &session, &session_id, Some(resume), 0)
                .await
                .is_err()
        );
        for ordinal in 0..101 {
            owner
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/conversation-burst-{ordinal}"),
                    kind: "message.sent".into(),
                    actor: Some("person/example".into()),
                    fields: BTreeMap::from([
                        ("from".into(), json!("person/example")),
                        ("to".into(), json!(agent)),
                        ("session_id".into(), json!(session_id)),
                        ("content".into(), json!(format!("burst {ordinal}"))),
                        ("status".into(), json!("sent")),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("conversation-burst-{ordinal}")),
                })
                .unwrap();
        }
        let gap = conversation_read_now(&owner, &session, &session_id, Some(&cursor)).unwrap_err();
        assert_eq!(gap.code, "cursor-gap");
    }

    #[tokio::test]
    async fn current_agent_session_fences_composer_messages_and_timeline_history() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "session-message-node");
        let subject = "agent/session-message-owner";
        let incarnation = "session-message-runtime:i2";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("session-message-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("session-message-runtime".into()),
            })
            .unwrap();

        let snapshot = new_client_snapshot(&state);
        let agents = client_agent_resources(
            &state.store,
            false,
            &snapshot.created_at,
            snapshot.store_index,
        )
        .unwrap();
        let sessions = client_session_resources(
            &state.store,
            false,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap();
        let agent = agents.iter().find(|agent| agent["id"] == subject).unwrap();
        let owned_sessions = sessions
            .iter()
            .filter(|session| session["owner_id"] == subject)
            .collect::<Vec<_>>();
        assert_eq!(owned_sessions.len(), 1);
        let session_id = owned_sessions[0]["id"].as_str().unwrap().to_owned();
        assert_eq!(agent["current_session_id"], session_id);

        let client_session = ClientSession::local(Some("person/alex")).unwrap();
        let action = |key: &str, parameters: Value| ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: format!("action/{key}"),
            action_type: "message.send".into(),
            idempotency_key: format!("session-message-{key}"),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: None,
                runtime_desired_revision: None,
                terminal_sequence: None,
                preview_token: None,
            },
            parameters,
        };

        let generic = action(
            "generic",
            json!({"to": subject, "content": "generic legacy message"}),
        );
        dispatch_action(&state, &snapshot, &client_session, &generic)
            .await
            .expect("generic messaging remains backward compatible");

        let current_snapshot = new_client_snapshot(&state);
        let composer = action(
            "composer",
            json!({
                "to": subject,
                "session_id": session_id,
                "content": "current composer message"
            }),
        );
        let affected = dispatch_action(&state, &current_snapshot, &client_session, &composer)
            .await
            .unwrap();
        let composer_claim = state
            .store
            .claims_for(&affected[0], Some("message.sent"))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            composer_claim.body["fields"]["session_id"],
            Value::String(session_id.clone())
        );
        let composer_resource = client_message_resources(&state.store, None, true, None)
            .unwrap()
            .into_iter()
            .find(|message| message["id"] == affected[0])
            .unwrap();
        assert_eq!(composer_resource["session_id"], session_id);

        let _ = accept_message(
            &state,
            MessageSendRequest {
                idempotency_key: "session-message-older".into(),
                from: client_session.authority_actor.clone(),
                to: subject.into(),
                content: "older session message".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            },
            Some("session/older-incarnation".into()),
            None,
        )
        .unwrap();

        let rejected = action(
            "stale",
            json!({
                "to": subject,
                "session_id": "session/older-incarnation",
                "content": "must not be accepted"
            }),
        );
        let error = dispatch_action(
            &state,
            &new_client_snapshot(&state),
            &client_session,
            &rejected,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "stale-fence");

        let timeline = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &client_session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        let text = timeline["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["body"]["text"].as_str())
            .collect::<Vec<_>>();
        // A message that names no session belongs to the session current when it arrived; one
        // that names an older session does not.
        assert_eq!(
            text,
            vec!["generic legacy message", "current composer message"]
        );

        // The agent's own Small Talk joins its conversation, saying who wrote to whom.
        let _ = accept_message(
            &state,
            MessageSendRequest {
                idempotency_key: "session-message-outgoing".into(),
                from: subject.into(),
                to: "agent/session-message-peer".into(),
                content: "outgoing small talk".into(),
                title: Some("A question".into()),
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            },
            None,
            None,
        )
        .unwrap();
        let timeline = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &client_session,
            &session_id,
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        let items = timeline["items"].as_array().unwrap();
        let outgoing = items
            .iter()
            .position(|item| item["body"]["text"] == "outgoing small talk")
            .expect("the agent's own message is in its conversation");
        let header = &items[outgoing - 1];
        assert_eq!(header["type"], "message");
        assert_eq!(header["role"], "assistant");
        assert_eq!(header["body"]["from"], subject);
        assert_eq!(header["body"]["to"], "agent/session-message-peer");
        assert_eq!(header["body"]["title"], "A question");

        // A new incarnation starts a new conversation: earlier Small Talk stays with the old one.
        std::thread::sleep(std::time::Duration::from_millis(2));
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("session-message-runtime".into()),
                    ),
                    (
                        "incarnation_id".into(),
                        Value::String("session-message-runtime:i3".into()),
                    ),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("session-message-runtime-i3".into()),
            })
            .unwrap();
        let next_session = super::managed_session_id(subject, "session-message-runtime:i3");
        let next = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &client_session,
            &next_session,
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        assert!(
            next["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["body"]["text"].is_null()),
            "{next:#}"
        );
    }

    #[test]
    fn managed_codex_session_renders_its_exact_native_chat_not_only_status() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let transcript = home.join(".codex/sessions/2026/09/24/managed.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let owner = "agent/managed-codex";
        let incarnation = "native-pty:one";
        let provider_incarnation = "provider-one";
        let native_id = "native-managed-codex-test";
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type":"session_meta","timestamp":"2026-09-24T12:00:00Z","payload":{"id":native_id,"cwd":root.path(),"source":"test"}}),
                json!({"type":"response_item","timestamp":"2026-09-24T12:00:01Z","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"Exact managed transcript"}]}}),
            ),
        )
        .unwrap();
        let mut state = test_state_named(root.path(), "managed-codex-test");
        state.native_session_home = Some(home);
        let directory = state
            .state_dir
            .join("drivers")
            .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
            .join("state");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("runtime.json"),
            serde_json::to_vec(
                &json!({"agent":"managed-codex","incarnation":provider_incarnation}),
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.join("binding.json"),
            serde_json::to_vec(&json!({"agent":"managed-codex","runtimeIncarnation":provider_incarnation,"threadId":native_id})).unwrap(),
        )
        .unwrap();
        for (kind, fields) in [
            (
                "runtime.observed",
                BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("managed-codex-pty".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
            ),
            (
                "harness.observed",
                BTreeMap::from([
                    ("state".into(), Value::String("working".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    (
                        "evidence_incarnation".into(),
                        Value::String(provider_incarnation.into()),
                    ),
                ]),
            ),
        ] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: owner.into(),
                    kind: kind.into(),
                    actor: Some(owner.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let snapshot = new_client_snapshot(&state);
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let session_id = super::managed_session_id(owner, incarnation);
        let timeline = timeline_value(
            &state,
            &snapshot,
            &session,
            &session_id,
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        assert!(timeline["items"].as_array().unwrap().iter().any(|item| {
            item["type"] == "content" && item["body"]["text"] == "Exact managed transcript"
        }));
        let baseline = conversation_read_now(&state, &session, &session_id, None).unwrap();
        let baseline_cursor = baseline["next_cursor"].as_str().unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: "message/managed-native".into(),
                kind: "message.sent".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/alex")),
                    ("to".into(), json!(owner)),
                    ("session_id".into(), json!(session_id)),
                    ("content".into(), json!("Native message")),
                    ("status".into(), json!("sent")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("managed-native-message".into()),
            })
            .unwrap();
        let message_update =
            conversation_read_now(&state, &session, &session_id, Some(baseline_cursor)).unwrap();
        assert_eq!(message_update["items"].as_array().unwrap().len(), 2);
        let message_cursor = message_update["next_cursor"].as_str().unwrap();
        use std::io::Write as _;
        writeln!(std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(), "{}", json!({"type":"response_item","timestamp":"2026-09-24T12:00:02Z","payload":{"type":"message","role":"assistant","id":"later","content":[{"type":"output_text","text":"Native reply"}]}})).unwrap();
        let native_update =
            conversation_read_now(&state, &session, &session_id, Some(message_cursor)).unwrap();
        assert!(
            native_update["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["body"]["text"] == "Native reply")
        );
        // A full replay page still resumes after hundreds of unrelated graph commits.
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        for number in 0..230 {
            writeln!(writer, "{}", json!({"type":"response_item","timestamp":"2026-09-24T12:00:03Z","payload":{"type":"message","role":"assistant","id":format!("steady-{number}"),"content":[{"type":"output_text","text":format!("Turn {number}")} ]}})).unwrap();
        }
        drop(writer);
        let full = conversation_read_now(&state, &session, &session_id, None).unwrap();
        let page = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &session,
            &session_id,
            &ClientListQuery {
                limit: Some(200),
                ..Default::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 200);
        let before_churn = new_client_snapshot(&state);
        for _ in 0..301 {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "agent/unrelated".into(),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), json!("vanished"))]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let unchanged =
            conversation_read_now(&state, &session, &session_id, full["next_cursor"].as_str())
                .unwrap();
        assert!(unchanged["items"].as_array().unwrap().is_empty());
        let _ = timeline_value(
            &state,
            &before_churn,
            &session,
            &session_id,
            &ClientListQuery {
                limit: Some(200),
                ..Default::default()
            },
        )
        .unwrap();
        // A transcript st3 binds but cannot read is named, so the failure can be reported.
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads anything; the check needs a file this user really cannot read.
            if std::fs::read(&transcript).is_err() {
                let unreadable = timeline_value(
                    &state,
                    &new_client_snapshot(&state),
                    &session,
                    &session_id,
                    &ClientListQuery::default(),
                )
                .unwrap()
                .0;
                let notice = unreadable["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["body"]["code"] == "transcript-not-bound")
                    .cloned()
                    .expect("an unreadable transcript is named");
                assert_eq!(
                    notice["body"]["details"]["transcript"],
                    transcript.display().to_string()
                );
                assert!(
                    notice["body"]["message"]
                        .as_str()
                        .unwrap()
                        .starts_with("transcript not bound: the transcript could not be read"),
                    "{notice:#}"
                );
            }
            std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        // A thread that has not written its rollout yet is not a failure: the seat has said
        // nothing since it started, and the notice says so.
        std::fs::write(
            directory.join("binding.json"),
            serde_json::to_vec(&json!({"agent":"managed-codex","runtimeIncarnation":provider_incarnation,"threadId":"native-managed-codex-unwritten"})).unwrap(),
        )
        .unwrap();
        let quiet = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &session,
            &session_id,
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        let notice = quiet["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["body"]["code"] == "transcript-not-bound")
            .cloned()
            .expect("a seat with no rollout yet says so");
        assert_eq!(notice["body"]["details"]["not_yet"], true, "{notice:#}");
        assert!(notice["body"]["details"].get("transcript").is_none());
        std::fs::write(
            directory.join("binding.json"),
            serde_json::to_vec(
                &json!({"agent":"managed-codex","runtimeIncarnation":"other","threadId":native_id}),
            )
            .unwrap(),
        )
        .unwrap();
        let stale = managed_transcript(&state, owner, incarnation)
            .unwrap()
            .unwrap();
        assert_eq!(
            stale.transcript.unwrap_err(),
            Missing::from("the Codex binding belongs to a different runtime")
        );
        let unbound = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &session,
            &session_id,
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        assert!(unbound["items"].as_array().unwrap().iter().any(|item| {
            item["type"] == "error"
                && item["body"]["code"] == "transcript-not-bound"
                && item["body"]["details"]["driver"] == "codex"
        }));
    }

    #[test]
    fn managed_claude_session_uses_only_current_wrapper_binding() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let native_id = "11111111-1111-4111-8111-111111111111";
        let transcript = home.join(format!(".claude/projects/-test/{native_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!("{}\n", json!({"type":"assistant","sessionId":native_id,"timestamp":"2026-09-24T12:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Current Claude answer"}]}})),
        ).unwrap();
        let mut state = test_state_named(root.path(), "managed-claude-test");
        state.native_session_home = Some(home);
        let owner = "agent/managed-claude";
        let incarnation = "native-pty:current";
        let provider_incarnation = "provider-current";
        let identity = "managed-claude";
        let directory = state
            .state_dir
            .join("drivers")
            .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
            .join("catalog/agents")
            .join(st_drivers::run::detect_host())
            .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16]);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("claude-native-session"),
            serde_json::to_vec(
                &json!({"incarnation":provider_incarnation,"native_session_id":native_id}),
            )
            .unwrap(),
        )
        .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: owner.into(),
                kind: "harness.observed".into(),
                actor: Some(owner.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("working".into())),
                    ("driver".into(), Value::String("claude".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    (
                        "evidence_incarnation".into(),
                        Value::String(provider_incarnation.into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let exact = super::managed_transcript(&state, owner, incarnation)
            .unwrap()
            .unwrap()
            .transcript
            .unwrap();
        let timeline = crate::external_sessions::normalized_timeline(&exact).unwrap();
        assert!(
            timeline
                .iter()
                .any(|entry| entry["body"]["text"] == "Current Claude answer")
        );
        assert!(
            super::managed_transcript(&state, owner, "native-pty:old")
                .unwrap()
                .unwrap()
                .transcript
                .is_err()
        );
        std::fs::write(
            directory.join("claude-native-session"),
            serde_json::to_vec(
                &json!({"incarnation":"provider-old","native_session_id":native_id}),
            )
            .unwrap(),
        )
        .unwrap();
        // A stale hook binding is not used, and the process fallback refuses evidence that
        // does not name a driver process.
        let stale = super::managed_transcript(&state, owner, incarnation)
            .unwrap()
            .unwrap()
            .transcript
            .unwrap_err();
        assert!(
            stale.reason.contains("does not name a driver process") && !stale.not_yet,
            "{stale:?}"
        );
    }

    #[test]
    fn managed_claude_without_a_hook_binding_is_proved_from_its_driver_or_says_why_not() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let native_id = "22222222-2222-4222-8222-222222222222";
        let transcript = home.join(format!(".claude/projects/-test/{native_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!("{}\n", json!({"type":"assistant","sessionId":native_id,"timestamp":"2026-09-30T12:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Recovered without the hook"}]}})),
        )
        .unwrap();
        let mut state = test_state_named(root.path(), "managed-claude-fallback-test");
        state.native_session_home = Some(home.clone());
        let owner = "agent/managed-claude-fallback";
        let incarnation = "native-pty:current";
        let append = |kind: &str, fields: BTreeMap<String, Value>| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: owner.into(),
                    kind: kind.into(),
                    actor: Some(owner.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        };
        let observe = |evidence: &str| {
            append(
                "harness.observed",
                BTreeMap::from([
                    ("state".into(), json!("working")),
                    ("driver".into(), json!("claude")),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("evidence_incarnation".into(), json!(evidence)),
                ]),
            );
        };
        append(
            "runtime.observed",
            BTreeMap::from([
                ("status".into(), json!("running")),
                ("runtime_id".into(), json!("managed-claude-pty")),
                ("incarnation_id".into(), json!(incarnation)),
                ("terminal".into(), json!(true)),
            ]),
        );
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let session_id = super::managed_session_id(owner, incarnation);
        let timeline = || {
            timeline_value(
                &state,
                &new_client_snapshot(&state),
                &session,
                &session_id,
                &ClientListQuery::default(),
            )
            .unwrap()
            .0["items"]
                .as_array()
                .unwrap()
                .clone()
        };

        // The hook never bound a session and the evidence names a driver that is gone: the
        // claim timeline says so instead of silently standing in for the conversation.
        observe("4194303-1000-0");
        let unbound = timeline();
        let notice = unbound
            .iter()
            .find(|item| item["body"]["code"] == "transcript-not-bound")
            .expect("the timeline should say why the transcript is missing");
        assert_eq!(notice["type"], "error");
        assert_eq!(notice["role"], "system");
        assert_eq!(notice["body"]["details"]["driver"], "claude");
        assert!(
            notice["body"]["message"]
                .as_str()
                .unwrap()
                .starts_with("transcript not bound: the SessionStart hook did not bind"),
            "{notice:#}"
        );
        assert!(
            !unbound
                .iter()
                .any(|item| item["body"]["text"] == "Recovered without the hook")
        );

        // The live driver named by the evidence proves its Claude child's session.
        #[cfg(target_os = "linux")]
        {
            let fake = crate::external_sessions::test_support::FakeClaudeDriver::start(owner);
            fake.record_session(&home, native_id, None);
            observe(&fake.token());
            let bound = timeline();
            assert!(
                bound
                    .iter()
                    .any(|item| item["body"]["text"] == "Recovered without the hook"),
                "{bound:#?}"
            );
            assert!(
                !bound
                    .iter()
                    .any(|item| item["body"]["code"] == "transcript-not-bound")
            );
            // A different seat's evidence naming this driver binds nothing.
            let other = crate::external_sessions::claude_session_of_managed_driver(
                &home,
                "agent/someone-else",
                &fake.token(),
            );
            assert!(other.is_err());
        }
    }

    #[test]
    fn managed_omp_session_reads_the_current_saved_conversation() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "managed-omp-test");
        let owner = "agent/example/pty-rust/omp";
        let identity = owner.strip_prefix("agent/").unwrap();
        let incarnation = "123:2026-09-25T15:11:54.870Z";
        let directory = state
            .state_dir
            .join("drivers")
            .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
            .join("catalog/agents")
            .join(st_drivers::run::detect_host())
            .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16])
            .join("provider-sessions");
        std::fs::create_dir_all(&directory).unwrap();
        let old = directory.join("2026-09-25T14-00-00-000Z_old.jsonl");
        let current = directory.join("2026-09-25T15-11-55-793Z_current.jsonl");
        std::fs::write(
            &old,
            format!(
                "{}\n",
                json!({"type":"session","id":"old","timestamp":"2026-09-25T14:00:00Z","cwd":"/tmp"})
            ),
        )
        .unwrap();
        std::fs::write(
            &current,
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"session","id":"current","timestamp":"2026-09-25T15:11:55.793Z","cwd":"/tmp"}),
                json!({"type":"message","id":"answer","timestamp":"2026-09-25T15:12:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Saved OMP answer"},{"type":"toolCall","id":"call-1","name":"read","arguments":{"file":"example"}}]}}),
                json!({"type":"message","id":"result","timestamp":"2026-09-25T15:12:01Z","message":{"role":"toolResult","content":[{"type":"text","text":"{\"presence\":null}"}]}}),
            ),
        )
        .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: owner.into(),
                kind: "harness.observed".into(),
                actor: Some(owner.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("omp".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let exact = super::managed_transcript(&state, owner, incarnation)
            .unwrap()
            .unwrap()
            .transcript
            .unwrap();
        assert_eq!(exact.native_id, "current");
        let timeline = crate::external_sessions::normalized_timeline(&exact).unwrap();
        assert!(
            timeline
                .iter()
                .any(|entry| entry["body"]["text"] == "Saved OMP answer")
        );
        assert!(timeline.iter().any(|entry| entry["type"] == "tool_call"));
        assert!(
            timeline
                .iter()
                .any(|entry| entry["role"] == "tool"
                    && entry["body"]["text"] == "{\"presence\":null}")
        );
        assert!(
            super::managed_transcript(&state, owner, "123:2026-09-25T14:00:00Z")
                .unwrap()
                .unwrap()
                .transcript
                .is_err()
        );
    }

    #[test]
    fn durable_timeline_is_cursor_paged_and_enforces_replace_finalize_identity() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "timeline-node");
        let subject = "agent/timeline-owner";
        let incarnation = "timeline-runtime:i1";
        let append = |kind: &str, fields: BTreeMap<String, Value>, key: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: kind.into(),
                    actor: Some(subject.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap();
        };
        append(
            "runtime.observed",
            BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                (
                    "runtime_id".into(),
                    Value::String("timeline-runtime".into()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("terminal".into(), Value::Bool(false)),
            ]),
            "timeline-runtime",
        );
        let message = state
            .store
            .append_claim(&ClaimInput {
                subject: "message/timeline-user".into(),
                kind: "message.sent".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/alex".into())),
                    ("to".into(), Value::String(subject.into())),
                    ("content".into(), Value::String("do the work".into())),
                    ("tags".into(), json!(["dictated", "test-label"])),
                    ("status".into(), Value::String("sent".into())),
                    (
                        "session_id".into(),
                        Value::String(managed_session_id(subject, incarnation)),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("timeline-message".into()),
            })
            .unwrap();
        assert_eq!(message.kind, "message.sent");
        let timeline_sequence = |entry_id: &str| {
            100 + [
                "timeline-entry/provider-content",
                "timeline-entry/wrong-incarnation",
                "timeline-entry/missing-incarnation",
                "timeline-entry/tool-call",
                "timeline-entry/tool-result",
                "timeline-entry/redaction",
                "timeline-entry/truncation",
                "timeline-entry/usage-without-source-semantics",
            ]
            .iter()
            .position(|known| *known == entry_id)
            .expect("the test numbers every entry") as u64
        };
        let timeline = |operation: &str,
                        entry_id: &str,
                        revision: u64,
                        role: &str,
                        entry_type: &str,
                        final_entry: bool,
                        body: Value| {
            BTreeMap::from([
                ("operation".into(), Value::String(operation.into())),
                ("entry_id".into(), Value::String(entry_id.into())),
                ("revision".into(), Value::from(revision)),
                ("role".into(), Value::String(role.into())),
                ("entry_type".into(), Value::String(entry_type.into())),
                ("final".into(), Value::Bool(final_entry)),
                ("body".into(), body),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                // The driver numbers each entry once; revisions keep that number.
                ("sequence".into(), Value::from(timeline_sequence(entry_id))),
            ])
        };
        append(
            "harness.timeline",
            timeline(
                "append",
                "timeline-entry/provider-content",
                1,
                "assistant",
                "content",
                false,
                json!({"media_type":"text/plain", "text":"draft"}),
            ),
            "timeline-content-append",
        );
        append(
            "harness.timeline",
            timeline(
                "replace",
                "timeline-entry/provider-content",
                2,
                "assistant",
                "content",
                false,
                json!({"media_type":"text/plain", "text":"revised"}),
            ),
            "timeline-content-replace",
        );
        append(
            "harness.timeline",
            timeline(
                "finalize",
                "timeline-entry/provider-content",
                3,
                "assistant",
                "content",
                true,
                json!({"media_type":"text/plain", "text":"final"}),
            ),
            "timeline-content-finalize",
        );
        let mut wrong_incarnation = timeline(
            "append",
            "timeline-entry/wrong-incarnation",
            1,
            "assistant",
            "content",
            true,
            json!({"media_type":"text/plain", "text":"must not cross incarnations"}),
        );
        wrong_incarnation.insert(
            "incarnation_id".into(),
            Value::String("timeline-runtime:old".into()),
        );
        append(
            "harness.timeline",
            wrong_incarnation,
            "timeline-wrong-incarnation",
        );
        let mut missing_incarnation = timeline(
            "append",
            "timeline-entry/missing-incarnation",
            1,
            "assistant",
            "content",
            true,
            json!({"media_type":"text/plain", "text":"must be rejected"}),
        );
        missing_incarnation.remove("incarnation_id");
        let missing = state.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.timeline".into(),
            actor: Some(subject.into()),
            fields: missing_incarnation,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("timeline-missing-incarnation".into()),
        });
        assert_eq!(missing.unwrap_err().code, "missing-claim-field");
        for (entry_id, role, entry_type, body) in [
            (
                "timeline-entry/tool-call",
                "assistant",
                "tool_call",
                json!({"call_id":"call/1", "name":"shell", "arguments":{"command":"true"}}),
            ),
            (
                "timeline-entry/tool-result",
                "tool",
                "tool_result",
                json!({"call_id":"call/1", "status":"success", "media_type":"text/plain", "content":"ok"}),
            ),
            (
                "timeline-entry/redaction",
                "system",
                "redaction",
                json!({"reason":"credential", "withheld_bytes":12}),
            ),
            (
                "timeline-entry/truncation",
                "system",
                "truncation",
                json!({"reason":"limit", "omitted_from_sequence":90, "omitted_to_sequence":99}),
            ),
            (
                "timeline-entry/usage-without-source-semantics",
                "system",
                "usage",
                json!({"total_tokens":7}),
            ),
        ] {
            append(
                "harness.timeline",
                timeline("append", entry_id, 1, role, entry_type, true, body),
                &format!(
                    "timeline-{}",
                    entry_id.trim_start_matches("timeline-entry/")
                ),
            );
        }
        append(
            "harness.diagnostic",
            BTreeMap::from([
                ("severity".into(), Value::String("warning".into())),
                ("code".into(), Value::String("provider-warning".into())),
                ("reason".into(), Value::String("retry later".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            "timeline-diagnostic",
        );
        append(
            "harness.usage",
            BTreeMap::from([
                (
                    "semantics".into(),
                    Value::String("session_cumulative".into()),
                ),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("total_tokens".into(), Value::from(42)),
            ]),
            "timeline-usage",
        );

        let snapshot = new_client_snapshot(&state);
        let session_id = client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let client_session = ClientSession::local(None).unwrap();
        let mut cursor = None;
        let mut entries = Vec::new();
        let mut wrote_during_pagination = false;
        loop {
            let query = ClientListQuery {
                limit: Some(3),
                cursor: cursor.clone(),
                ..ClientListQuery::default()
            };
            let page = timeline_value(
                &state,
                &snapshot,
                &client_session,
                session_id.trim_start_matches("session/"),
                &query,
            )
            .unwrap()
            .0;
            let page_items = page["items"].as_array().unwrap().iter().cloned();
            entries.splice(0..0, page_items);
            cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_some() && !wrote_during_pagination {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: "agent/unrelated-timeline-writer".into(),
                        kind: "runtime.observed".into(),
                        actor: Some("agent/unrelated-timeline-writer".into()),
                        fields: BTreeMap::from([
                            (
                                "runtime_id".into(),
                                Value::String("unrelated-runtime".into()),
                            ),
                            (
                                "incarnation_id".into(),
                                Value::String("unrelated-runtime:i1".into()),
                            ),
                            ("status".into(), Value::String("running".into())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
                wrote_during_pagination = true;
            }
            if cursor.is_none() {
                break;
            }
        }
        let user_message = entries
            .iter()
            .find(|entry| {
                entry["type"] == "message" && entry["body"]["message_id"] == message.subject
            })
            .unwrap();
        assert_eq!(
            user_message["body"]["tags"],
            json!(["dictated", "test-label"])
        );
        assert_eq!(
            state
                .store
                .message(&message.subject)
                .unwrap()
                .unwrap()
                .content,
            "do the work"
        );
        let types = entries
            .iter()
            .filter_map(|entry| entry["type"].as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            types,
            BTreeSet::from([
                "message",
                "content",
                "tool_call",
                "tool_result",
                "status",
                "error",
                "usage",
                "redaction",
                "truncation",
            ])
        );
        let final_content = entries
            .iter()
            .find(|entry| entry["id"] == "timeline-entry/provider-content")
            .unwrap();
        assert_eq!(final_content["revision"], 3);
        assert_eq!(final_content["final"], true);
        assert_eq!(final_content["body"]["text"], "final");
        let inferred_usage = entries
            .iter()
            .find(|entry| entry["id"] == "timeline-entry/usage-without-source-semantics")
            .unwrap();
        assert_eq!(inferred_usage["body"]["semantics"], "response");
        assert_eq!(inferred_usage["body"]["driver"], "codex");
        assert_eq!(inferred_usage["body"]["attribution"]["agent_id"], subject);
        assert!(
            entries
                .iter()
                .all(|entry| entry["id"] != "timeline-entry/wrong-incarnation"),
            "explicit timeline claims are fenced to the live incarnation"
        );
        assert!(entries.windows(2).all(|pair| {
            pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()
        }));
    }

    #[test]
    fn timeline_missing_append_is_permanent_on_http_and_stream() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "timeline-retention-node");
        let subject = "agent/timeline-retention-owner";
        let incarnation = "timeline-retention-runtime:i1";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("timeline-retention-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("timeline-retention-runtime".into()),
            })
            .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.timeline".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("operation".into(), json!("replace")),
                    ("entry_id".into(), json!("timeline-entry/orphan")),
                    ("sequence".into(), json!(1)),
                    ("revision".into(), json!(2)),
                    ("role".into(), json!("assistant")),
                    ("entry_type".into(), json!("content")),
                    ("final".into(), json!(false)),
                    ("body".into(), json!({"text":"retained update"})),
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let session = ClientSession::local(None).unwrap();
        let snapshot = new_client_snapshot(&state);
        let session_id = client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..2 {
            let snapshot = new_client_snapshot(&state);
            let gap = timeline_value(
                &state,
                &snapshot,
                &session,
                session_id.trim_start_matches("session/"),
                &ClientListQuery::default(),
            )
            .unwrap_err();
            assert_eq!(gap.code, "timeline-history-incomplete");
            assert!(gap.message.contains("append operation is missing"));
            assert_eq!(gap.details["full_resync"], false);
            let raw = json!({"code":gap.code,"message":gap.message,"details":gap.details});
            assert_eq!(
                client_error_envelope(gap.status, &raw, "test")["retryable"],
                false
            );
            assert_eq!(
                conversation_stream_error("conversation", &gap)["retryable"],
                false
            );
        }
        // An actual cursor/window race still admits a fresh read.
        assert!(client_error_retryable(StatusCode::GONE, Some("cursor-gap")));
    }

    #[test]
    fn timeline_retention_requires_an_actual_typed_gap_interval() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "timeline-retention-node");
        let subject = "agent/timeline-retention-owner";
        let incarnation = "timeline-retention-runtime:i1";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("timeline-retention-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("timeline-retention-runtime".into()),
            })
            .unwrap();
        let entry = |sequence: u64, entry_type: &str, body: Value| ClaimInput {
            subject: subject.into(),
            kind: "harness.timeline".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("operation".into(), Value::String("append".into())),
                (
                    "entry_id".into(),
                    Value::String(format!("timeline-entry/retention-{sequence}")),
                ),
                ("sequence".into(), Value::from(sequence)),
                ("revision".into(), Value::from(1)),
                ("role".into(), Value::String("system".into())),
                ("entry_type".into(), Value::String(entry_type.into())),
                ("final".into(), Value::Bool(true)),
                ("body".into(), body),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("timeline-retention-{sequence}")),
        };
        let append_entry = |sequence: u64, entry_type: &str, body: Value| {
            state
                .store
                .append_claim(&entry(sequence, entry_type, body))
                .unwrap();
        };
        // One entry more than a timeline read returns, in one commit: a commit for each entry
        // took minutes on a busy disk.
        state.store.append_local_observations_for_test(
            &(1..=4_097)
                .map(|sequence| {
                    entry(
                        sequence,
                        "status",
                        json!({"status":"running", "detail":format!("event {sequence}")}),
                    )
                })
                .collect::<Vec<_>>(),
        );
        let session = ClientSession::local(None).unwrap();
        let snapshot = new_client_snapshot(&state);
        let session_id = client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let gap = timeline_value(
            &state,
            &snapshot,
            &session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .unwrap_err();
        assert_eq!(gap.status, StatusCode::GONE);
        assert_eq!(gap.code, "timeline-history-incomplete");
        assert_eq!(gap.details.get("full_resync"), Some(&Value::Bool(false)));
        assert!(!client_error_retryable(gap.status, Some(&gap.code)));

        append_entry(
            4_098,
            "truncation",
            json!({
                "reason":"producer-retention",
                "omitted_from_sequence":1,
                "omitted_to_sequence":1
            }),
        );
        let snapshot = new_client_snapshot(&state);
        let insufficient = timeline_value(
            &state,
            &snapshot,
            &session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .unwrap_err();
        assert_eq!(insufficient.code, "timeline-history-incomplete");

        append_entry(
            4_099,
            "truncation",
            json!({
                "reason":"producer-retention",
                "omitted_from_sequence":1,
                "omitted_to_sequence":3
            }),
        );
        let snapshot = new_client_snapshot(&state);
        let _ = timeline_value(
            &state,
            &snapshot,
            &session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .expect("the typed retention intervals cover the complete omitted logical prefix");
    }

    #[test]
    fn usage_attribution_and_rollups_follow_exact_step_and_mission_ownership() {
        let store = Store::open_memory("usage-attribution-node").unwrap();
        let desired = [
            crate::model::DesiredSubject {
                subject: "agent/usage-a".into(),
                kind: "agent".into(),
                desired: json!({}),
                member: None,
                owner_run: Some("mission-run/example/one".into()),
                owner_generation: Some("run-generation/gen-one".into()),
                owner_step: Some("step-run/gen-one/step-a".into()),
            },
            crate::model::DesiredSubject {
                subject: "agent/usage-b".into(),
                kind: "agent".into(),
                desired: json!({}),
                member: None,
                owner_run: Some("mission-run/example/one".into()),
                owner_generation: Some("run-generation/gen-one".into()),
                owner_step: Some("step-run/gen-one/step-b".into()),
            },
            crate::model::DesiredSubject {
                subject: "agent/usage-other".into(),
                kind: "agent".into(),
                desired: json!({}),
                member: None,
                owner_run: Some("mission-run/example/two".into()),
                owner_generation: Some("run-generation/gen-two".into()),
                owner_step: Some("step-run/gen-two/step-c".into()),
            },
        ];
        for (subject, total) in [
            ("agent/usage-a", 10_u64),
            ("agent/usage-b", 20),
            ("agent/usage-other", 99),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "harness.usage".into(),
                    actor: Some(subject.into()),
                    fields: BTreeMap::from([
                        (
                            "semantics".into(),
                            Value::String("session_cumulative".into()),
                        ),
                        ("driver".into(), Value::String("codex".into())),
                        (
                            "incarnation_id".into(),
                            Value::String(format!("{subject}:i1")),
                        ),
                        ("total_tokens".into(), Value::from(total)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("usage-{total}")),
                })
                .unwrap();
        }
        let attribution = timeline_attribution("agent/usage-a", &desired);
        assert_eq!(attribution["agent_id"], "agent/usage-a");
        assert_eq!(attribution["mission_run_id"], "mission-run/example/one");
        assert_eq!(attribution["generation_id"], "run-generation/gen-one");
        assert_eq!(attribution["step_id"], "step-run/gen-one/step-a");

        let step = aggregate_usage_for_step(&store, &desired, "step-run/gen-one/step-a", None)
            .unwrap()
            .unwrap();
        assert_eq!(step.total_tokens, 10);
        let mission = aggregate_usage_for_runs(
            &store,
            &desired,
            &BTreeSet::from(["mission-run/example/one"]),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(mission.total_tokens, 30);
        assert_eq!(mission.incarnation_count, 2);
    }

    #[test]
    fn terminal_attach_reconciles_a_pre_receipt_restart_without_secret_at_rest() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/terminal-owner".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/terminal-owner".into()),
                fields: BTreeMap::from([
                    (
                        "runtime_id".into(),
                        Value::String("terminal-runtime".into()),
                    ),
                    (
                        "incarnation_id".into(),
                        Value::String("terminal-runtime:i1".into()),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/terminal-attach-crash".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "terminal-attach-crash-0001".into(),
            fence: Fence {
                snapshot_id: "snapshot/test".into(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("terminal-runtime:i1".into()),
                runtime_desired_revision: None,
                terminal_sequence: Some(1),
                preview_token: None,
            },
            parameters: json!({ "target_id": "terminal/agent/terminal-owner" }),
        };
        let first = create_terminal_attachment(&state, &session, &request, "request-digest")
            .expect("pre-receipt attachment");
        let capability = first["stream_capability"].as_str().unwrap().to_owned();
        let attachment_id = first["attachment_id"].as_str().unwrap().to_owned();
        let stored = serde_json::to_string(
            &state
                .store
                .claims_page(None, None, 0, None, true, 100)
                .unwrap(),
        )
        .unwrap();
        assert!(!stored.contains(&capability));
        assert!(!stored.contains("capability="));
        assert_eq!(stored.matches("custom.client.terminal-attached").count(), 1);
        let key_metadata = fs::metadata(root.path().join("client-terminal.key")).unwrap();
        assert_eq!(key_metadata.mode() & 0o777, 0o600);
        assert_eq!(key_metadata.uid(), unsafe { libc::geteuid() });

        drop(state);
        let restarted = test_state(root.path());
        let reconciled =
            create_terminal_attachment(&restarted, &session, &request, "request-digest")
                .expect("retry reconciles the pre-receipt attachment");
        assert_eq!(reconciled["attachment_id"], attachment_id);
        assert_eq!(reconciled["stream_capability"], capability);
        let stored = restarted
            .store
            .claims_page(None, None, 0, None, true, 100)
            .unwrap();
        assert_eq!(
            stored
                .claims
                .iter()
                .filter(|claim| claim.kind == "custom.client.terminal-attached")
                .count(),
            1
        );

        // A projected-screen capability is a lease: every stream a client opens with it is
        // accepted until it is detached, and the capability stays available.
        for _ in 0..3 {
            consume_terminal_attachment(
                &restarted,
                &session,
                "terminal/agent/terminal-owner",
                "terminal-runtime:i1",
                Some(&capability),
            )
            .unwrap();
        }
        let consumed = terminal_attachment_response(&restarted, &session, &attachment_id).unwrap();
        assert_eq!(consumed["state"], "available");
        assert_eq!(consumed["reusable"], true);
        assert_eq!(consumed["ttl_s"], 300);
        assert!(consumed["retry_hint"].is_null());
        assert_eq!(consumed["stream_capability"], capability);
        let detach = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/terminal-detach-after-consume".into(),
            action_type: "terminal.detach".into(),
            idempotency_key: "terminal-detach-after-consume-0001".into(),
            fence: Fence {
                snapshot_id: "snapshot/test".into(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("terminal-runtime:i1".into()),
                runtime_desired_revision: None,
                terminal_sequence: None,
                preview_token: None,
            },
            parameters: json!({ "target_id": attachment_id }),
        };
        detach_terminal_attachment(&restarted, &session, &detach).unwrap();
        // Detaching revokes the lease: it opens nothing more and says to attach again.
        assert_eq!(
            consume_terminal_attachment(
                &restarted,
                &session,
                "terminal/agent/terminal-owner",
                "terminal-runtime:i1",
                Some(&capability),
            )
            .unwrap_err()
            .code,
            "forbidden"
        );
        let detached = terminal_attachment_response(&restarted, &session, &attachment_id).unwrap();
        assert_eq!(detached["state"], "detached");
        assert!(detached["stream_capability"].is_null());
        assert_eq!(detached["retry_hint"], "reattach");
        let lifecycle = restarted
            .store
            .claims_for(
                &terminal_attachment_subject(consumed["attachment_id"].as_str().unwrap()).unwrap(),
                None,
            )
            .unwrap()
            .into_iter()
            .filter(|claim| claim.kind.starts_with("custom.client.terminal-"))
            .collect::<Vec<_>>();
        assert_eq!(lifecycle.len(), 2);
        for claim in lifecycle {
            let projected = safe_event_projection(
                &restarted,
                &EventRecord {
                    store_index: claim.store_index,
                    kind: claim.kind.clone(),
                    subject: claim.subject.clone(),
                    body: claim.body.clone(),
                },
            );
            assert_eq!(projected.0, "upsert", "{}", claim.kind);
            assert_eq!(
                projected.1,
                ["terminal/agent/terminal-owner"],
                "{}",
                claim.kind
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terminal_actions_survive_unrelated_writes_but_reject_changed_screens() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let Some(pty) = std::env::split_paths(&path)
            .map(|dir| dir.join("pty"))
            .find(|p| p.is_file())
        else {
            assert!(std::env::var_os("CI").is_none(), "CI must provide pty");
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut state = test_state(root.path());
        state.pty_binary = pty.clone();
        let runtime = st_runtime::PtyRuntime::new(state.pty_root.clone())
            .with_binary(pty.to_string_lossy());
        struct Cleanup(st_runtime::PtyRuntime);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.stop("fence-test");
                let _ = self.0.remove("fence-test");
            }
        }
        let _cleanup = Cleanup(runtime.clone());
        let pty_root = state.pty_root.clone();
        let output = tokio::task::spawn_blocking(move || std::process::Command::new(pty)
            .env("PTY_ROOT", pty_root)
            .args(["run", "-d", "--force", "--id", "fence-test", "--tag", "keep=true", "--", "/bin/sh", "-c", "stty -echo; printf ready; while IFS= read -r line; do printf '\\r\\naccepted:%s' \"$line\"; done"])
            .output().unwrap()).await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let live = runtime
            .snapshot()
            .unwrap()
            .into_iter()
            .find(|live| live.name == "fence-test")
            .unwrap();
        let incarnation = format!("{}:{}", live.pid.unwrap(), live.created_at.unwrap());
        let observe = |incarnation: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "agent/fence-test".into(),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("runtime_id".into(), json!("fence-test")),
                        ("incarnation_id".into(), json!(incarnation)),
                        ("status".into(), json!("running")),
                        ("terminal".into(), json!(true)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        observe(&incarnation);
        let screen = terminal_screen_value(
            &state,
            "agent/fence-test",
            Some(&incarnation),
            None,
            Duration::ZERO,
        )
        .await
        .unwrap();
        // The owner's best-effort facts come from the session itself and stay out of the screen
        // revision.
        let facts = terminal_facts(&state, "agent/fence-test")
            .await
            .expect("a running session answers a stats query");
        assert_eq!(facts["tags"]["keep"], "true", "{facts}");
        assert_eq!(facts["process"]["alive"], true, "{facts}");
        assert!(facts["rows"].as_u64().unwrap() >= 1, "{facts}");
        assert!(facts["clients"]["total"].is_u64(), "{facts}");
        assert!(facts["uptime_s"].is_u64(), "{facts}");
        assert!(screen.get("facts").is_none());
        let snapshot = new_client_snapshot(&state);
        let fence = Fence {
            snapshot_id: snapshot.id.clone(),
            runtime_incarnation: Some(incarnation.clone()),
            terminal_sequence: Some(screen["next_sequence"].as_u64().unwrap()),
            ..Default::default()
        };
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let request =
            |action_type: &str, key: &str, fence: Fence, parameters: Value| ActionRequest {
                api_version: CLIENT_API_VERSION.into(),
                id: format!("action/{key}"),
                action_type: action_type.into(),
                idempotency_key: format!("terminal-fence-test-{key}"),
                fence,
                parameters,
            };
        // A graph update after the client read must not invalidate this terminal's view.
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/unrelated".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), json!("vanished"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(
            validate_fence(&state, &fence).is_ok(),
            "unrelated writes do not invalidate declaration mutations"
        );
        let mut attach_fence = fence.clone();
        attach_fence.terminal_sequence = None;
        let _ = action(
            State(state.clone()),
            Extension(snapshot.clone()),
            Extension(session.clone()),
            Json(request(
                "terminal.attach",
                "fence-attach",
                attach_fence,
                json!({"target_id":"terminal/agent/fence-test"}),
            )),
        )
        .await
        .unwrap();
        let _ = action(
            State(state.clone()),
            Extension(snapshot.clone()),
            Extension(session.clone()),
            Json(request(
                "terminal.input",
                "fence-input",
                fence.clone(),
                json!({"terminal_id":"terminal/agent/fence-test","mode":"line","value":"hello"}),
            )),
        )
        .await
        .unwrap();
        let changed = terminal_screen_value(
            &state,
            "agent/fence-test",
            Some(&incarnation),
            screen["revision"].as_str(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_ne!(screen["next_sequence"], changed["next_sequence"]);
        assert!(changed.to_string().contains("accepted:hello"));
        for action_type in ["terminal.input", "terminal.resize"] {
            let parameters = if action_type == "terminal.input" {
                json!({"terminal_id":"terminal/agent/fence-test","mode":"line","value":"must-not-land"})
            } else {
                json!({"terminal_id":"terminal/agent/fence-test","rows":20,"columns":80})
            };
            let error = action(
                State(state.clone()),
                Extension(snapshot.clone()),
                Extension(session.clone()),
                Json(request(action_type, action_type, fence.clone(), parameters)),
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "stale-fence");
        }
        let mut foreign = fence.clone();
        foreign.snapshot_id = foreign.snapshot_id.replacen(&state.node, "another-host", 1);
        assert!(validate_fence(&state, &foreign).is_err());
        let mut future = fence.clone();
        future.snapshot_id = format!(
            "snapshot/{}/{}/digest",
            state.node,
            state.store.index().unwrap() + 1
        );
        assert!(validate_fence(&state, &future).is_err());
        let mut revision = fence.clone();
        revision
            .subject_revisions
            .insert("agent/unrelated".into(), "old-revision".into());
        assert!(validate_fence(&state, &revision).is_err());
        observe("replacement-incarnation");
        let error = action(
            State(state.clone()),
            Extension(snapshot),
            Extension(session),
            Json(request(
                "terminal.attach",
                "fence-replaced",
                fence,
                json!({"target_id":"terminal/agent/fence-test"}),
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "stale-fence");
    }

    #[test]
    fn concurrent_same_key_attach_publishes_one_attachment_and_one_sanitized_receipt() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/concurrent-terminal-owner".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/concurrent-terminal-owner".into()),
                fields: BTreeMap::from([
                    (
                        "runtime_id".into(),
                        Value::String("concurrent-terminal-runtime".into()),
                    ),
                    (
                        "incarnation_id".into(),
                        Value::String("concurrent-terminal-runtime:i1".into()),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/concurrent-terminal-attach".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "concurrent-terminal-attach-key-0001".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("concurrent-terminal-runtime:i1".into()),
                runtime_desired_revision: None,
                terminal_sequence: Some(snapshot.store_index),
                preview_token: None,
            },
            parameters: json!({ "target_id": "terminal/agent/concurrent-terminal-owner" }),
        };
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let count = 8;
        let barrier = Arc::new(Barrier::new(count));
        let threads = (0..count)
            .map(|_| {
                let state = state.clone();
                let snapshot = snapshot.clone();
                let request = request.clone();
                let session = session.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(action(
                            State(state),
                            Extension(snapshot),
                            Extension(session),
                            Json(request),
                        ))
                        .map(|Json(result)| result)
                })
            })
            .collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let attachment_id = results[0]["terminal_attachment"]["attachment_id"]
            .as_str()
            .unwrap();
        let capability = results[0]["terminal_attachment"]["stream_capability"]
            .as_str()
            .unwrap();
        assert!(results.iter().all(|result| {
            result["terminal_attachment"]["attachment_id"] == attachment_id
                && result["terminal_attachment"]["stream_capability"] == capability
        }));

        let page = state
            .store
            .claims_page(None, None, 0, None, true, 100)
            .unwrap();
        assert_eq!(
            page.claims
                .iter()
                .filter(|claim| claim.kind == "custom.client.terminal-attached")
                .count(),
            1
        );
        assert_eq!(
            page.claims
                .iter()
                .filter(|claim| claim.kind == "custom.client.action-result")
                .count(),
            1
        );
        let stored = serde_json::to_string(&page).unwrap();
        assert!(!stored.contains(capability));
        assert!(!stored.contains("st3.cap."));
        assert!(!stored.contains("stream_capability"));
    }

    /// A member one build behind still routes to a terminal on a newer member whose seat reports
    /// a claim kind it does not know yet (`harness.limits`): the valid runtime observation
    /// decides, and the unknown claim waits for an upgrade.
    #[test]
    fn an_older_member_routes_to_a_terminal_despite_an_unknown_harness_claim() {
        let owner_root = tempfile::tempdir().unwrap();
        let follower_root = tempfile::tempdir().unwrap();
        let owner = test_state_named(owner_root.path(), "owner-node");
        let mut follower = test_state_named(follower_root.path(), "follower-node");
        let secret = follower_root.path().join("fleet-secret");
        std::fs::write(&secret, [7_u8; 32]).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        follower.client_relay = crate::peer::ClientRelay::from_config(&crate::config::Config {
            node: "follower-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![crate::config::PeerConfig {
                name: "owner-node".into(),
                url: "http://127.0.0.1:9".into(),
            }],
            ..Default::default()
        })
        .unwrap();
        let mut registry = st3_schema::registry().clone();
        registry.claims.remove("harness.limits").unwrap();
        follower.store.set_claim_registry(registry);
        let subject = "agent/fleet-terminal";
        let claim = |kind: &str, fields: Value, key: &str| ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some(subject.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        };
        owner
            .store
            .append_claim(&claim(
                "runtime.observed",
                json!({"runtime_id": "same-runtime-id", "incarnation_id": "same-runtime-id:i1",
                       "status": "running", "terminal": true}),
                "owner-running",
            ))
            .unwrap();
        owner
            .store
            .append_claim(&claim(
                "harness.limits",
                json!({"driver": "claude", "incarnation_id": "same-runtime-id:i1",
                       "weekly_percent": 40.0, "measured_at_unix_ms": 1_790_000_000_000_u64}),
                "owner-limits",
            ))
            .unwrap();
        follower
            .store
            .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
            .unwrap();
        let status = follower.store.status(Some(subject)).unwrap();
        assert_eq!(status.subjects[0].reachability, "reachable");
        let live = remote_terminal_live_session(&follower, subject, "same-runtime-id:i1")
            .unwrap_or_else(|error| panic!("{}: {}", error.code, error.message));
        assert_eq!(live.owner_host_id, "host/owner-node");
        assert_eq!(live.incarnation_id, "same-runtime-id:i1");
    }

    #[test]
    fn terminal_capabilities_and_pty_reads_are_bound_to_the_selected_owner_host() {
        let owner_root = tempfile::tempdir().unwrap();
        let follower_root = tempfile::tempdir().unwrap();
        let owner = test_state_named(owner_root.path(), "owner-node");
        let mut follower = test_state_named(follower_root.path(), "follower-node");
        let subject = "agent/fleet-terminal";
        let runtime = |status: &str, incarnation: &str, key: &str| ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("same-runtime-id".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("status".into(), Value::String(status.into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        };
        owner
            .store
            .append_claim(&runtime("running", "same-runtime-id:i1", "owner-running"))
            .unwrap();
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let projected = runtime_resources(&owner, true, &new_client_snapshot(&owner), &session)
            .unwrap()
            .into_iter()
            .find(|runtime| runtime["owner_id"] == subject)
            .unwrap();
        assert_eq!(projected["state"], "running");
        assert_eq!(projected["owner_host_id"], "host/owner-node");
        assert_eq!(projected["terminal_access"]["read"], "granted");
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/fleet-terminal-attach".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "fleet-terminal-attach-0001".into(),
            fence: Fence {
                snapshot_id: new_client_snapshot(&owner).id,
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("same-runtime-id:i1".into()),
                runtime_desired_revision: None,
                terminal_sequence: Some(owner.store.index().unwrap()),
                preview_token: None,
            },
            parameters: json!({ "target_id": "terminal/agent/fleet-terminal" }),
        };
        let attached =
            create_terminal_attachment(&owner, &session, &request, "fleet-request-digest").unwrap();
        assert_eq!(attached["owner_host_id"], "host/owner-node");
        let attachment_id = attached["attachment_id"].as_str().unwrap();
        let capability = attached["stream_capability"].as_str().unwrap();
        let owner_replay =
            create_terminal_attachment(&owner, &session, &request, "fleet-request-digest").unwrap();
        assert_eq!(owner_replay["stream_capability"], capability);

        follower
            .store
            .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
            .unwrap();
        let error = terminal_attachment_response(&follower, &session, attachment_id).unwrap_err();
        assert_eq!(error.code, "forbidden");
        let error = consume_terminal_attachment(
            &follower,
            &session,
            "terminal/agent/fleet-terminal",
            "same-runtime-id:i1",
            Some(capability),
        )
        .unwrap_err();
        assert_eq!(error.code, "forbidden");
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(terminal_screen_value(
                &follower,
                subject,
                Some("same-runtime-id:i1"),
                None,
                Duration::ZERO,
            ))
            .unwrap_err();
        assert_eq!(error.code, "runtime-not-local");

        let secret = follower_root.path().join("fleet-secret");
        std::fs::write(&secret, [7_u8; 32]).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        follower.client_relay = crate::peer::ClientRelay::from_config(&crate::config::Config {
            node: "follower-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![crate::config::PeerConfig {
                name: "owner-node".into(),
                url: "http://127.0.0.1:9".into(),
            }],
            ..Default::default()
        })
        .unwrap();
        let paired = ClientSession {
            actor: "person/alex/session/device-one".into(),
            authority_actor: "person/alex".into(),
            transport: "paired",
            scopes: ["terminal.read".into()].into_iter().collect(),
        };
        let mut remote_request = request.clone();
        remote_request.idempotency_key = "fleet-terminal-attach-device-one".into();
        let remote =
            create_terminal_attachment(&follower, &paired, &remote_request, "remote-digest")
                .unwrap();
        assert_eq!(remote["owner_host_id"], "host/owner-node");
        let remote_capability = remote["stream_capability"].as_str().unwrap();
        assert_ne!(remote_capability, capability);
        let another_device = ClientSession {
            actor: "person/alex/session/device-two".into(),
            ..paired.clone()
        };
        assert_eq!(
            consume_terminal_attachment(
                &follower,
                &another_device,
                "terminal/agent/fleet-terminal",
                "same-runtime-id:i1",
                Some(remote_capability)
            )
            .unwrap_err()
            .code,
            "forbidden"
        );
        assert_eq!(
            consume_terminal_attachment(
                &follower,
                &paired,
                "terminal/agent/fleet-terminal",
                "same-runtime-id:i1",
                Some(capability)
            )
            .unwrap_err()
            .code,
            "forbidden"
        );
        consume_terminal_attachment(
            &follower,
            &paired,
            "terminal/agent/fleet-terminal",
            "same-runtime-id:i1",
            Some(remote_capability),
        )
        .unwrap();
        assert_eq!(
            terminal_attachment_response(
                &follower,
                &paired,
                remote["attachment_id"].as_str().unwrap()
            )
            .unwrap()["state"],
            "available"
        );

        owner
            .store
            .append_claim(&runtime("exited", "same-runtime-id:i1", "owner-exited"))
            .unwrap();
        let error = terminal_live_session(&owner, subject, Some("same-runtime-id:i1")).unwrap_err();
        assert_eq!(error.code, "not-found");
        let exited = runtime_resources(&owner, true, &new_client_snapshot(&owner), &session)
            .unwrap()
            .into_iter()
            .find(|runtime| runtime["owner_id"] == subject)
            .unwrap();
        assert_eq!(exited["state"], "exited");
        assert_eq!(exited["terminal_access"]["read"], "unavailable");

        let stale_root = tempfile::tempdir().unwrap();
        let stale = test_state_named(stale_root.path(), "stale-node");
        stale
            .store
            .append_claim(&runtime("running", "same-runtime-id:i1", "stale-running"))
            .unwrap();
        owner
            .store
            .import_replication("stale-node", &stale.store.export_replication(0).unwrap())
            .unwrap();
        let status = owner.store.status(Some(subject)).unwrap();
        assert_eq!(status.subjects[0].reachability, "indeterminate");
        assert!(status.subjects[0].actual_origin.is_some());
        let error = terminal_live_session(&owner, subject, Some("same-runtime-id:i1")).unwrap_err();
        assert_eq!(error.code, "runtime-authority-indeterminate");
        let indeterminate = runtime_resources(&owner, true, &new_client_snapshot(&owner), &session)
            .unwrap()
            .into_iter()
            .find(|runtime| runtime["owner_id"] == subject)
            .unwrap();
        assert_eq!(indeterminate["state"], "unreachable");
        assert_eq!(indeterminate["terminal_access"]["read"], "unavailable");
        assert_eq!(indeterminate["operational"]["actionable"], false);
    }
}

fn glass_person(session: &ClientSession, write: bool) -> Result<String, ApiError> {
    require_scope(
        session,
        if write {
            "control.glasses"
        } else {
            "read.glasses"
        },
    )?;
    let person = &session.authority_actor;
    if !person.starts_with("person/") || person.matches('/').count() != 1 {
        return Err(forbidden("glasses require the session's concrete person"));
    }
    Ok(person.clone())
}

fn glass_subject(person: &str, id: &str) -> Result<String, ApiError> {
    if !st3_schema::glasses::valid_uuid(id) {
        return Err(validation("a glass ID must be a canonical lowercase UUID"));
    }
    Ok(format!("glass/{person}/{id}"))
}

pub(super) async fn glasses_list(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let person = glass_person(&session, false)?;
    if query.person.is_some() || query.actor.is_some() || query.history {
        return Err(validation(
            "glasses always select the session person and current state",
        ));
    }
    let store = state.store.clone();
    let through = snapshot.store_index;
    let items = blocking_store(move || store.glasses(&person, through)).await?;
    Ok(Json(client_page(
        &state, &snapshot, "glasses", items, &query,
    )?))
}

pub(super) async fn glass_get(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let person = glass_person(&session, false)?;
    let subject = glass_subject(&person, &id)?;
    let store = state.store.clone();
    let through = snapshot.store_index;
    blocking_store(move || store.glasses(&person, through))
        .await?
        .into_iter()
        .find(|glass| glass["id"].as_str() == Some(&subject))
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found("the glass is absent, retired, or outside the current quota")
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GlassPut {
    body: Value,
    #[serde(deserialize_with = "glass_base_revision")]
    base_revision: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GlassDelete {
    #[serde(deserialize_with = "glass_base_revision")]
    base_revision: Option<String>,
}
fn glass_base_revision<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

async fn glass_write(
    state: AppState,
    session: ClientSession,
    id: String,
    headers: HeaderMap,
    body: Option<Value>,
    base_revision: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let person = glass_person(&session, true)?;
    let subject = glass_subject(&person, &id)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .filter(|key| !key.is_empty() && key.len() <= 200)
        .ok_or_else(|| validation("glass writes require a bounded Idempotency-Key header"))?;
    let idempotency_key = format!("glass:{}:{key}", session.actor);
    let mut fields = BTreeMap::from([("base_revision".into(), json!(base_revision))]);
    let kind = if let Some(body) = body {
        st3_schema::glasses::validate_body(&body).map_err(|e| validation(e.message))?;
        fields.insert("body".into(), body);
        "glass.upserted"
    } else {
        "glass.deleted"
    };
    let store = state.store.clone();
    let claim = blocking_store(move || {
        Ok(store.append_claim(&ClaimInput {
            subject,
            kind: kind.into(),
            actor: Some(person),
            fields,
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(idempotency_key),
        }))
    })
    .await?
    .map_err(|error| {
        let is_idempotency = matches!(error.code, "idempotency-mismatch" | "idempotency-conflict");
        let mut error = ApiError::bad(error);
        if is_idempotency { error.code = "idempotency-conflict".into(); error.status = StatusCode::CONFLICT; }
        error
    })?;
    signal_changed(&state);
    Ok(Json(
        json!({"id":claim.subject, "kind":"glass", "revision":claim.id,
        "body":claim.body["fields"]["body"], "deleted":claim.kind == "glass.deleted",
        "base_revision":claim.body["fields"]["base_revision"],
        "replaced_revision":claim.body["fields"]["replaced_revision"],
        "updated_at":client_timestamp(claim.accepted_at_unix_ms)}),
    ))
}

pub(super) async fn glass_put(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<GlassPut>,
) -> Result<Json<Value>, ApiError> {
    glass_write(
        state,
        session,
        id,
        headers,
        Some(request.body),
        request.base_revision,
    )
    .await
}
pub(super) async fn glass_delete(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<GlassDelete>,
) -> Result<Json<Value>, ApiError> {
    glass_write(state, session, id, headers, None, request.base_revision).await
}
