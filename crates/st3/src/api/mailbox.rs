//! One local daemon stream per delivery component. Ownership and receipts are fenced in SQLite,
//! so an old channel reconnecting after a replacement cannot consume or acknowledge its mail.
use super::*;
use crate::mailbox::{Fence, Frame, Receipt};

mod authority;

pub(super) async fn subscribe(
    State(state): State<AppState>,
    Query(fence): Query<Fence>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    websocket: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    authorize(&fence, peer.as_ref().map(|p| &p.0))?;
    let checked_state = state.clone();
    let binding = fence.clone();
    let peer = peer.expect("authorize checked the native peer").0;
    let checked_peer = peer.clone();
    let native = blocking_action(move || {
        if !cfg!(target_os = "linux") { return checked_state.store.check_mailbox(&binding).map(|_| false); }
        if authority::with_argv_channel(&checked_state, &binding, &checked_peer, |member, validate| {
            if binding.epoch == 0 {
                return Err(St3Error::new("stale-mailbox-session", "an argv subscription must already be bound"));
            }
            checked_state.store.bind_argv_mailbox_checked(&binding, member, validate)
        })?.is_some() { return Ok(false); }
        authority::with_authority(&checked_state, &binding, &checked_peer, |owner, validate| {
        if checked_state.store.check_mailbox(&binding).is_ok() {
            // Upgrade a still-current pre-lease binding without changing its capability.
            checked_state.store.bind_mailbox_with_lease_checked(&binding, Some(owner), validate).map(|_| ())
        } else {
            checked_state.store.repair_mailbox_checked(&binding, owner, validate).map(|_| ())
        }
        }).map(|_| true)
    }).await?;
    // Wake the predecessor immediately, even when no graph content changed.
    signal_local_change(&state);
    Ok(websocket.on_upgrade(move |socket| stream(state, fence, socket, peer, native)))
}

pub(super) async fn bind(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Fence>,
) -> Result<Json<Fence>, ApiError> {
    authorize(&request, peer.as_ref().map(|p| &p.0))?;
    let bind_state = state.clone();
    let peer = peer.expect("authorize checked the native peer").0;
    let bound = blocking_action(move || {
        if !cfg!(target_os = "linux") { return bind_state.store.bind_mailbox(&request); }
        if let Some(bound) = authority::with_argv_channel(&bind_state, &request, &peer, |member, validate| {
            bind_state.store.bind_argv_mailbox_checked(&request, member, validate)
        })? { return Ok(bound); }
        authority::with_authority(&bind_state, &request, &peer, |owner, validate| {
            bind_state.store.bind_mailbox_with_lease_checked(&request, Some(owner), validate)
        })
    }).await?;
    signal_local_change(&state);
    Ok(Json(bound))
}

// Explicit already-admitted protocol fixtures lack a physical PTY/provider lease.
// Selected Rust integration fixtures use these routes; the installed API router cannot.
#[cfg(feature = "test-support")]
pub(super) async fn bind_admitted_fixture(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Fence>,
) -> Result<Json<Value>, ApiError> {
    authorize(&request, peer.as_ref().map(|peer| &peer.0))?;
    let bound = blocking_action(move || state.store.bind_mailbox(&request)).await?;
    Ok(Json(json!({"api_version":"st3.v1","value":bound})))
}

#[cfg(feature = "test-support")]
pub(super) async fn subscribe_admitted_fixture(
    State(state): State<AppState>,
    Query(fence): Query<Fence>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    websocket: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    authorize(&fence, peer.as_ref().map(|peer| &peer.0))?;
    state.store.check_mailbox(&fence).map_err(ApiError::bad)?;
    signal_local_change(&state);
    Ok(websocket.on_upgrade(move |socket| stream_with_reader(state, fence, socket, raw_snapshot)))
}

pub(super) async fn attachment(
    State(state): State<AppState>,
    Query(fence): Query<Fence>,
    peer: Option<Extension<NativeDeliveryPeer>>,
) -> Result<Json<crate::mailbox::Attachment>, ApiError> {
    authorize(&fence, peer.as_ref().map(|p| &p.0))?;
    let store = state.store.clone();
    let checked_state = state.clone();
    let peer = peer.expect("authorize checked the native peer").0;
    let attached = blocking_action(move || {
        if store.has_mailbox_lease(&fence)? {
            authority::with_authority(&checked_state, &fence, &peer, |owner, validate| {
                validate()?;
                if !store.owns_mailbox_lease(&fence, owner)? {
                    return Err(St3Error::new("stale-mailbox-session", "another process owns this mailbox lease"));
                }
                store.check_mailbox(&fence)
            })?;
        }
        store.check_mailbox(&fence)?;
        Ok(super::claude_channel_attached(
            &store,
            &fence.subject,
            &fence.incarnation,
        ))
    })
    .await?;
    Ok(Json(crate::mailbox::Attachment { attached }))
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
                ("transport".into(), json!(peer.as_ref().unwrap().0.transport)),
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
    let checked_state = state.clone();
    let checked_peer = peer.expect("authorize checked the native peer").0;
    let kind = input.kind.clone();
    let (record, appended, work_wake) = blocking_action(move || {
        let (record, appended) = if store.has_mailbox_lease(&request.fence)? {
            authority::with_authority(&checked_state, &request.fence, &checked_peer, |owner, validate| {
                validate()?;
                if !store.owns_mailbox_lease(&request.fence, owner)? {
                    return Err(St3Error::new("stale-mailbox-session", "another process owns this mailbox lease"));
                }
                store.append_mailbox_receipt_outcome(&input, &request.fence)
            })?
        } else { store.append_mailbox_receipt_outcome(&input, &request.fence)? };
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

type MaintenanceIds = std::collections::BTreeSet<String>;
type SnapshotUpdate = (crate::store::MailboxWatermark, Snapshot, bool, MaintenanceIds);

/// Recover only recent mail which has never been offered. A prior staging claim is
/// already an offer attempt, even when its delivered receipt has not arrived yet.
fn never_offered(store: &Store, message: &crate::model::MessageView) -> anyhow::Result<bool> {
    Ok(message.status == "sent"
        && store
            .latest_claim(&message.subject, Some("message.staged"))?
            .is_none()
        && store
            .latest_claim(&message.subject, Some("message.delivered"))?
            .is_none())
}

/// A boot/reconnect holds historical and previously offered mail. Recent unoffered
/// mail survives the outage; once admitted it stays in this stream until receipts finish.
fn retain_live_mail(
    store: &Store,
    messages: &mut Vec<crate::model::MessageView>,
    since: u128,
    through: Option<u64>,
    recovered: &mut std::collections::BTreeSet<String>,
) -> anyhow::Result<()> {
    let mut live = Vec::new();
    for message in messages.drain(..) {
        let Some(sent) = store.latest_claim(&message.subject, Some("message.sent"))? else {
            continue;
        };
        let after_connection = through.is_none_or(|index| message.created_index > index)
            && sent.accepted_at_unix_ms >= since;
        if after_connection || recovered.contains(&message.subject) {
            live.push(message);
        } else if sent.accepted_at_unix_ms
            > since.saturating_sub(u128::from(super::mail_backlog::THRESHOLD_MS))
            && never_offered(store, &message)?
        {
            recovered.insert(message.subject.clone());
            live.push(message);
        }
    }
    *messages = live;
    Ok(())
}

/// Older drivers poll instead of subscribing. Preserve a recent message admitted
/// by this boot while its current staging attempt finishes; older attempts stay held.
pub(super) fn hold_pre_boot_mail(
    store: &Store,
    peer: Option<&NativeDeliveryPeer>,
    recipient: Option<&str>,
    messages: &mut Vec<crate::model::MessageView>,
) -> anyhow::Result<()> {
    let Some(peer) = peer.filter(|peer| recipient == Some(peer.agent.as_str())) else {
        return Ok(());
    };
    let Some((since, through)) = store.native_mail_boot_floor(&peer.agent)? else {
        for message in messages {
            message.status = "closed".into();
        }
        return Ok(());
    };
    // A closed projection only removes old native inbox files. Explicit conversation
    // reads still see the original graph status, with no synthetic receipt or close.
    for message in messages {
        let Some(sent) = store.latest_claim(&message.subject, Some("message.sent"))? else {
            message.status = "closed".into();
            continue;
        };
        let after_boot = message.created_index > through && sent.accepted_at_unix_ms >= since;
        let recent = sent.accepted_at_unix_ms
            > since.saturating_sub(u128::from(super::mail_backlog::THRESHOLD_MS));
        let current_offer = recent
            && store
                .latest_claim(&message.subject, Some("message.staged"))?
                .is_some_and(|claim| {
                    claim.store_index > through
                        && claim.accepted_at_unix_ms >= since
                        && claim.actor.as_deref() == Some(peer.agent.as_str())
                });
        if !after_boot && !(recent && (never_offered(store, message)? || current_offer)) {
            message.status = "closed".into();
        }
    }
    Ok(())
}

fn raw_snapshot(store: &Store, binding: &Fence) -> anyhow::Result<Snapshot> {
    store.read_snapshot(|through| {
        store.check_mailbox(binding).map_err(anyhow::Error::new)?;
        let seat = store
            .desired_subjects_named(std::slice::from_ref(&binding.subject))?
            .into_iter()
            .next();
        let messages = if binding.component == "delivery" {
            store.messages_through(Some(&binding.subject), false, through)?
        } else {
            Vec::new()
        };
        Ok((seat, messages))
    })
}

#[cfg(test)]
fn snapshot(store: &Store, binding: &Fence) -> anyhow::Result<Snapshot> {
    let (seat, mut messages) = raw_snapshot(store, binding)?;
    let _ = filter_messages(store, binding, &mut messages, &mut Default::default())?;
    Ok((seat, messages))
}

/// Shared with the full-snapshot oracle: point reads keep the same delivery policy.
fn filter_messages(
    store: &Store,
    binding: &Fence,
    messages: &mut Vec<crate::model::MessageView>,
    policy_rechecks: &mut std::collections::BTreeSet<String>,
) -> anyhow::Result<MaintenanceIds> {
    let mut allowed = Vec::new();
    let mut close_own = std::collections::BTreeSet::new();
    for message in messages.drain(..) {
        if message.to != binding.subject
            || !matches!(message.status.as_str(), "sent" | "staged" | "delivered")
        {
            continue;
        }
        if store
            .latest_claim(&message.subject, Some("message.closed"))?
            .is_some()
        {
            continue;
        }
        if let Some((locator, kind, id)) = crate::github_watch::named_object(&message) {
            // The in-flight post guard is local state, independent of graph notifications.
            policy_rechecks.insert(message.subject.clone());
            if crate::github_watch::post_in_flight(&binding.subject, &message) {
                continue;
            }
            if store.github_post_agent(&locator, &kind, id)?.as_deref()
                == Some(binding.subject.as_str())
            {
                close_own.insert(message.subject.clone());
                policy_rechecks.remove(&message.subject);
                continue;
            }
        }
        if store.rollout_message_allowed(&message)? {
            allowed.push(message);
        } else {
            // A reply's eligibility may depend on a parent's receipt. Recheck only these
            // held identities, rather than resnapshotting the mailbox on every timer tick.
            policy_rechecks.insert(message.subject.clone());
        }
    }
    *messages = allowed;
    Ok(close_own)
}

/// Update the connection's admitted mailbox, reading only changed durable identities.
fn update_snapshot<F>(
    store: &Store,
    binding: &Fence,
    read: F,
    previous: Option<(crate::store::MailboxWatermark, Snapshot)>,
    floor: (u128, u64),
    admitted: &mut std::collections::BTreeSet<String>,
    policy_rechecks: &mut std::collections::BTreeSet<String>,
) -> anyhow::Result<SnapshotUpdate>
where
    F: FnOnce(&Store, &Fence) -> anyhow::Result<Snapshot>,
{
    let (since, through) = floor;
    let mut close_own = std::collections::BTreeSet::new();
    let result = store.read_snapshot(|_| {
        let (mark, seat, mut messages) = if let Some((mark, (mut seat, mut messages))) = previous {
            let changes = store.mailbox_changes(binding, &mark, &messages)?;
            let mut updated = changes.seat || changes.resync || !changes.messages.is_empty();
            if changes.resync {
                let (seat, messages) =
                    crate::profile::task("task mailbox-snapshot", || read(store, binding))?;
                (changes.mark, seat, messages)
            } else {
                if changes.seat {
                    seat = store
                        .desired_subjects_named(std::slice::from_ref(&binding.subject))?
                        .into_iter()
                        .next();
                }
                let subjects = changes
                    .messages
                    .into_iter()
                    .chain(policy_rechecks.iter().cloned())
                    .collect::<std::collections::BTreeSet<_>>();
                let mut reorder = false;
                for subject in subjects {
                    policy_rechecks.remove(&subject);
                    let old = messages.iter().find(|message| message.subject == subject);
                    let mut changed: Vec<_> = store.message(&subject)?.into_iter().collect();
                    // Prune held pre-connection/offered mail without admitting it to recovery.
                    retain_live_mail(
                        store,
                        &mut changed,
                        since,
                        Some(through),
                        &mut admitted.clone(),
                    )?;
                    close_own.extend(filter_messages(store, binding, &mut changed, policy_rechecks)?);
                    retain_live_mail(store, &mut changed, since, Some(through), admitted)?;
                    if serde_json::to_value(old)? != serde_json::to_value(changed.first())? {
                        messages.retain(|message| message.subject != subject);
                        messages.extend(changed);
                        reorder = true;
                        updated = true;
                    }
                }
                if reorder {
                    store.order_mailbox_messages(&mut messages)?;
                }
                return Ok((changes.mark, (seat, messages), updated));
            }
        } else {
            let mark = store.mailbox_watermark(binding)?;
            let (seat, messages) =
                crate::profile::task("task mailbox-snapshot", || read(store, binding))?;
            (mark, seat, messages)
        };
        policy_rechecks.clear();
        // Policy rechecks retain only identities eligible for this connection. The full
        // snapshot applies the same filtering and only captures maintenance identities.
        let mut eligible = messages.clone();
        retain_live_mail(
            store,
            &mut eligible,
            since,
            Some(through),
            &mut admitted.clone(),
        )?;
        let eligible = eligible
            .into_iter()
            .map(|message| message.subject)
            .collect::<std::collections::BTreeSet<_>>();
        close_own.extend(filter_messages(store, binding, &mut messages, policy_rechecks)?);
        policy_rechecks.retain(|subject| eligible.contains(subject));
        retain_live_mail(store, &mut messages, since, Some(through), admitted)?;
        Ok((mark, (seat, messages), true))
    });
    // Reading never commits a closure or builds persistent derived state. The background
    // reactor revalidates these identities separately; its commits follow this read's cut.
    result.map(|(mark, snapshot, changed)| (mark, snapshot, changed, close_own))
}

/// Bounded maintenance for a connection's own-post identities, independent of socket reads.
/// One claim finishes (including COMMIT) before another starts; no reader spans the writer.
fn own_post_reactor(state: &AppState, fence: &Fence) -> tokio::sync::mpsc::Sender<String> {
    let (sender, mut pending) = tokio::sync::mpsc::channel::<String>(64);
    let state = state.clone();
    let fence = fence.clone();
    tokio::spawn(async move {
        while let Some(subject) = pending.recv().await {
            let store = state.store.clone();
            let binding = fence.clone();
            let result = tokio::task::spawn_blocking(move || {
                store.close_mailbox_own_post_wake(&binding, &subject)
            }).await;
            match result {
                Ok(Ok(true)) => signal_message_changed(&state, "message.closed", false),
                Ok(Ok(false)) => {},
                // A changed message or retired connection cannot authorize maintenance.
                // Durable notifications/timer rechecks handle later eligible identities.
                Ok(Err(error)) => eprintln!("st3: mailbox own-post maintenance refused: {error}"),
                Err(_) => return,
            }
            tokio::task::yield_now().await;
        }
    });
    sender
}

/// Maximum gap between durable change checks, including when a notification is missed.
const MAILBOX_RECHECK: Duration = Duration::from_secs(30);

async fn stream(state: AppState, fence: Fence, socket: WebSocket, peer: NativeDeliveryPeer, native: bool) {
    #[cfg(feature = "test-support")]
    let mut control = crate::test_support::fixture_mailbox_transport(&state.store, &fence.subject);
    #[cfg(feature = "test-support")]
    if fence.component == "delivery" {
        while *control.borrow_and_update() {
            if control.changed().await.is_err() { return; }
        }
    }
    let safety = futures_util::stream::unfold(safety_timer(&fence), |mut timer| async move {
        timer.tick().await;
        Some(((), timer))
    });
    let heartbeat = futures_util::stream::unfold(tokio::time::interval(Duration::from_secs(10)), |mut timer| async move {
        timer.tick().await;
        Some(((), timer))
    });
    let stream = stream_with_timers_inner(state.clone(), fence.clone(), socket, raw_snapshot, safety, heartbeat, StreamControl::new(native.then(|| peer.clone())));
    #[cfg(not(feature = "test-support"))]
    stream.await;
    #[cfg(feature = "test-support")]
    tokio::select! {
        _ = stream => {},
        _ = async {
            if fence.component != "delivery" { std::future::pending::<()>().await; }
            while control.changed().await.is_ok() {
                if *control.borrow_and_update() { return; }
            }
            std::future::pending::<()>().await;
        } => {},
    }
    if native {
        let _ = crate::api::read_deadline::spawn_blocking(move || {
            authority::loss_if_current(&state, &fence, &peer)
        }).await;
    }
}

fn safety_delay(fence: &Fence) -> Duration {
    use std::hash::BuildHasher;
    // A random per-process seed and the binding spread reconnecting streams across the full
    // period. Only the phase varies: the maximum gap stays thirty seconds.
    static SEED: std::sync::OnceLock<std::collections::hash_map::RandomState> =
        std::sync::OnceLock::new();
    let phase = SEED.get_or_init(Default::default).hash_one((
        &fence.subject,
        &fence.component,
        &fence.token,
    ));
    Duration::from_nanos(phase % MAILBOX_RECHECK.as_nanos() as u64)
}

fn safety_timer(fence: &Fence) -> tokio::time::Interval {
    let delay = safety_delay(fence);
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + delay, MAILBOX_RECHECK);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    timer
}

#[cfg(any(test, feature = "test-support"))]
async fn stream_with_reader<F>(state: AppState, fence: Fence, socket: WebSocket, read: F)
where
    F: Fn(&Store, &Fence) -> anyhow::Result<Snapshot> + Clone + Send + 'static,
{
    let safety = futures_util::stream::unfold(safety_timer(&fence), |mut timer| async move {
        timer.tick().await;
        Some(((), timer))
    });
    stream_with_rechecks(state, fence, socket, read, safety).await;
}

#[cfg(any(test, feature = "test-support"))]
async fn stream_with_rechecks<F, S>(
    state: AppState,
    fence: Fence,
    socket: WebSocket,
    read: F,
    safety: S,
) where
    F: Fn(&Store, &Fence) -> anyhow::Result<Snapshot> + Clone + Send + 'static,
    S: futures_util::Stream<Item = ()> + Send + 'static,
{
    let heartbeat = futures_util::stream::unfold(
        tokio::time::interval(Duration::from_secs(10)),
        |mut timer| async move {
            timer.tick().await;
            Some(((), timer))
        },
    );
    stream_with_timers(state, fence, socket, read, safety, heartbeat).await;
}

#[cfg(any(test, feature = "test-support"))]
async fn stream_with_timers<F, S, H>(
    state: AppState,
    fence: Fence,
    socket: WebSocket,
    read: F,
    safety: S,
    heartbeat: H,
) where
    F: Fn(&Store, &Fence) -> anyhow::Result<Snapshot> + Clone + Send + 'static,
    S: futures_util::Stream<Item = ()> + Send + 'static,
    H: futures_util::Stream<Item = ()> + Send + 'static,
{
    stream_with_timers_inner(state, fence, socket, read, safety, heartbeat, StreamControl::new(None)).await;
}

struct StreamControl {
    peer: Option<NativeDeliveryPeer>,
    #[cfg(test)]
    dirty_gate: Option<Arc<FixtureDirtyGate>>,
}
impl StreamControl {
    fn new(peer: Option<NativeDeliveryPeer>) -> Self {
        Self { peer, #[cfg(test)] dirty_gate: None }
    }
}

// Unit controls can pause snapshot processing while the real subscription and
// incoming-report handler remain live. Installed servers never have this gate.
#[cfg(test)]
struct FixtureDirtyGate {
    allowed: std::sync::atomic::AtomicBool,
    notifications: std::sync::atomic::AtomicUsize,
}

async fn stream_with_timers_inner<F, S, H>(
    state: AppState, fence: Fence, mut socket: WebSocket, read: F, safety: S, heartbeat: H,
    control: StreamControl,
) where
    F: Fn(&Store, &Fence) -> anyhow::Result<Snapshot> + Clone + Send + 'static,
    S: futures_util::Stream<Item = ()> + Send + 'static,
    H: futures_util::Stream<Item = ()> + Send + 'static,
{
    let peer = control.peer;
    futures_util::pin_mut!(safety, heartbeat);
    let since = client_now_ms();
    let through = match state.store.index() {
        Ok(index) => index,
        Err(_) => return,
    };
    // Subscribe before reading to close the replay-to-live race. Watch coalesces writes; every
    // wake reads a durable delta, so lag never depends on an ephemeral event cursor.
    let mut subscription = state.store.subscribe_mailbox(&fence, &state.event_notify);
    let mut previous_seat = Vec::new();
    let mut previous_mailbox = Vec::new();
    let mut previous_drain = None;
    let mut replay_nonce = Fence::new(&fence.subject, &fence.incarnation, "replay").token;
    let mut replay_challenge_sent = false;
    let mut replay_supported = false;
    let mut replay_proven = false;
    let mut recovered = std::collections::BTreeSet::new();
    let mut policy_rechecks = std::collections::BTreeSet::new();
    let own_posts = own_post_reactor(&state, &fence);
    let mut dirty = true;
    let mut last: Option<(crate::store::MailboxWatermark, Snapshot)> = None;
    loop {
        #[cfg(test)]
        let process_dirty = control.dirty_gate.as_ref().is_none_or(|gate|
            gate.allowed.load(std::sync::atomic::Ordering::SeqCst));
        #[cfg(not(test))]
        let process_dirty = true;
        if dirty && process_dirty {
            // A later close notification may record another loss after this connection's
            // first proof. Durable wakes (and the bounded safety heartbeat) let its current
            // consumed nonce repair that episode too, without reoffering native work.
            replay_proven = false;
            let store = state.store.clone();
            let binding = fence.clone();
            let read = read.clone();
            subscription.changed.borrow_and_update();
            let mut admitted = recovered.clone();
            let mut policies = policy_rechecks.clone();
            let mut previous = last.take();
            // This watch actor owns reconnection control. Finish that explicit command
            // before entering the read worker; snapshots never perform lease/fault writes.
            let repaired = if let Some(peer) = peer.clone() {
                let repair_state = state.clone();
                let repair_fence = fence.clone();
                match tokio::task::spawn_blocking(move || {
                    if repair_state.store.check_mailbox(&repair_fence).is_ok() { return false; }
                    let _ = authority::loss_if_current(&repair_state, &repair_fence, &peer);
                    matches!(authority::with_authority(&repair_state, &repair_fence, &peer, |owner, validate| {
                        repair_state.store.repair_mailbox_checked(&repair_fence, owner, validate)
                    }), Ok(true))
                }).await {
                    Ok(repaired) => repaired,
                    Err(_) => return,
                }
            } else { false };
            if repaired { previous = None; }
            let result = crate::api::read_deadline::spawn_blocking(move || {
                crate::profile::task("task mailbox-update", || {
                    let result = update_snapshot(
                        &store,
                        &binding,
                        read,
                        previous,
                        (since, through),
                        &mut admitted,
                        &mut policies,
                    );
                    (result, admitted, policies)
                })
            })
            .await;
            let (mark, (seat, mut messages), updated, closures) = match result {
                Ok((Ok(snapshot), admitted, policies)) => {
                    if repaired {
                        previous_mailbox.clear();
                        replay_nonce = Fence::new(&fence.subject, &fence.incarnation, "replay").token;
                        replay_challenge_sent = false;
                        replay_proven = false;
                    }
                    recovered = admitted;
                    policy_rechecks = policies;
                    snapshot
                }
                Ok((Err(error), _, _)) => {
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
                    return;
                }
                Err(_) => return,
            };
            for subject in closures {
                if let Err(error) = own_posts.try_send(subject) {
                    // A full bounded queue never blocks delivery or loses its maintenance
                    // identity: the normal safety tick will reconsider the held subject.
                    policy_rechecks.insert(error.into_inner());
                }
            }
            if !updated {
                last = Some((mark, (seat, messages)));
                dirty = false;
                continue;
            }
            last = Some((mark, (seat.clone(), messages.clone())));
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
            let subjects = messages
                .iter()
                .map(|message| message.subject.clone())
                .collect::<Vec<_>>();
            subscription.messages(&subjects);
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
            if fence.component == "delivery" && replay_supported && !replay_challenge_sent {
                if send(&mut socket, &Frame::Replay { nonce: replay_nonce.clone() }).await.is_err() { return; }
                replay_challenge_sent = true;
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
            event = subscription.changed.changed() => {
                if event.is_err() { return; }
                #[cfg(test)]
                if let Some(gate) = &control.dirty_gate {
                    gate.notifications.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                dirty = true;
            },
            tick = safety.next() => { if tick.is_none() { return; } dirty = true; },
            incoming = socket.recv() => match incoming {
                Some(Ok(WsMessage::Text(report))) => {
                    if state.store.check_mailbox(&fence).is_ok() {
                        let authenticated = if let Some(peer) = peer.clone() {
                            let checked_state = state.clone();
                            let binding = fence.clone();
                            let raw = report.to_string();
                            matches!(crate::api::read_deadline::spawn_blocking(move || {
                                authority::admit_report(&checked_state, &binding, &peer, &raw)
                            }).await, Ok(Ok(())))
                        } else { true };
                        // Native refusal discards all report-driven effects, including
                        // capability negotiation and rollout ACKs. Typed presence failure
                        // remains separate for an otherwise authenticated control report.
                        if !authenticated { continue; }
                        let recorded = record_admitted_report(&fence, &report);
                        let value = serde_json::from_str::<Value>(&report).unwrap_or(Value::Null);
                        replay_supported |= peer.is_some() && value["mailbox_replay_ack"] == true;
                        if fence.component == "delivery" && replay_supported && !previous_mailbox.is_empty() && !replay_challenge_sent {
                            if send(&mut socket, &Frame::Replay { nonce: replay_nonce.clone() }).await.is_err() { return; }
                            replay_challenge_sent = true;
                        }
                        if recorded && !replay_proven && replay_challenge_sent
                            && value["mailbox_replay_nonce"].as_str() == Some(&replay_nonce)
                            && value["ready"] == true
                            && let Some(peer) = peer.clone()
                        {
                            let repair_state = state.clone();
                            let binding = fence.clone();
                            let raw = report.to_string();
                            replay_proven = matches!(crate::api::read_deadline::spawn_blocking(move || {
                                authority::repair_proven(&repair_state, &binding, &peer, &raw)
                            }).await, Ok(Ok(())));
                        }
                        if let Ok(value) = serde_json::from_str::<Value>(&report)
                            && fence.component == "delivery"
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
            tick = heartbeat.next() => {
                if tick.is_none() { return; }
                // The next loop checks the durable cursor and owner in a blocking read.
                dirty = true;
                if socket.send(WsMessage::Ping(Vec::new().into())).await.is_err() { return; }
            },
        }
    }
}

// The stream checks durable custody and native peer authority before this recorder.
// Preserve typed recording success separately from ownership admission/drain ACKs.
fn record_admitted_report(fence: &Fence, report: &str) -> bool {
    let recorded = delivery_presence::record_fenced(fence, report);
    #[cfg(feature = "test-support")]
    if recorded {
        observe_admitted_report(fence, report);
    }
    recorded
}

#[cfg(test)]
fn record_checked_report(store: &Store, fence: &Fence, report: &str) -> bool {
    if store.check_mailbox(fence).is_err() {
        return false;
    }
    record_admitted_report(fence, report);
    true
}

/// An isolated fixture can observe daemon acceptance without writing graph state or
/// sending probe mail. This is deliberately after the durable ownership check.
#[cfg(feature = "test-support")]
fn observe_admitted_report(fence: &Fence, raw: &str) {
    use std::io::Write as _;
    use std::sync::Mutex;

    let Some(path) = std::env::var_os("ST3_TEST_ADMITTED_REPORTS") else {
        return;
    };
    let Some(mut report) = admitted_report(fence, raw) else {
        return;
    };
    // Sequence and append are ordered together, including reports from other streams.
    static SEQUENCE: Mutex<u64> = Mutex::new(0);
    let Ok(mut sequence) = SEQUENCE.lock() else {
        return;
    };
    *sequence += 1;
    report["sequence"] = json!(*sequence);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{report}");
    }
}

#[cfg(feature = "test-support")]
fn admitted_report(fence: &Fence, raw: &str) -> Option<Value> {
    use sha2::{Digest as _, Sha256};

    let report: Value = serde_json::from_str(raw).ok()?;
    if fence.component != "delivery" || report["transport"] != "omp-channel" {
        return None;
    }
    let pid = u32::try_from(report["pid"].as_u64()?).ok()?;
    let start_ticks = crate::sekrets::identity::process_start(i32::try_from(pid).ok()?)?;
    Some(json!({
        "subject": fence.subject,
        "incarnation": fence.incarnation,
        "component": fence.component,
        "epoch": fence.epoch,
        // Retain binding identity without publishing its credential.
        "token_sha256": hex::encode(Sha256::digest(fence.token.as_bytes())),
        "pid": pid,
        "start_ticks": start_ticks,
        "transport": report["transport"],
        "ready": report["ready"],
    }))
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

    // These snapshot/receipt controls model an already-admitted channel with a synthetic
    // identity. Their explicit test-only routes do not claim production peer authentication;
    // isolated native wrapper controls exercise the real kernel/process/provider boundary.
    fn admitted_fixture_router(state: AppState, peer: NativeDeliveryPeer) -> Router {
        let bind_state = state.clone();
        let bind_peer = peer.clone();
        let stream_state = state.clone();
        let stream_peer = peer.clone();
        Router::new()
            .route("/v1/mailbox/bind", post(move |Json(request): Json<Fence>| {
                let (state, peer) = (bind_state.clone(), bind_peer.clone());
                async move {
                    authorize(&request, Some(&peer))?;
                    let fence = blocking_action(move || state.store.bind_mailbox(&request)).await?;
                    Ok::<_, ApiError>(Json(json!({"api_version":"st3.v1","value":fence})))
                }
            }))
            .route("/v1/mailbox", get(move |Query(fence): Query<Fence>, websocket: WebSocketUpgrade| {
                let (state, peer) = (stream_state.clone(), stream_peer.clone());
                async move {
                    authorize(&fence, Some(&peer))?;
                    state.store.check_mailbox(&fence).map_err(ApiError::bad)?;
                    signal_local_change(&state);
                    Ok::<_, ApiError>(websocket.on_upgrade(move |socket| stream_with_reader(state, fence, socket, raw_snapshot)))
                }
            }))
            .fallback_service(router(state).layer(Extension(peer)))
    }

    #[cfg(all(feature = "test-support", target_os = "linux"))]
    #[test]
    fn admitted_report_requires_the_current_delivery_binding() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let subject = "agent/report-binding-control";
        state.store.append_claim(&ClaimInput {
            subject: subject.into(), kind: "runtime.observed".into(), actor: Some(subject.into()),
            fields: serde_json::from_value(json!({"status":"running", "runtime_id":"report-binding-control", "incarnation_id":"current"})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        let old = state
            .store
            .bind_mailbox(&Fence::new(subject, "current", "delivery"))
            .unwrap();
        let report =
            json!({"transport":"omp-channel", "pid":std::process::id(), "ready":true}).to_string();
        assert!(record_checked_report(&state.store, &old, &report));
        let current = state
            .store
            .bind_mailbox(&Fence::new(subject, "current", "delivery"))
            .unwrap();
        assert!(!record_checked_report(&state.store, &old, &report));
        for change in ["token", "epoch", "incarnation", "subject"] {
            let mut foreign = current.clone();
            match change {
                "token" => foreign.token = "foreign".into(),
                "epoch" => foreign.epoch += 1,
                "incarnation" => foreign.incarnation = "previous".into(),
                _ => foreign.subject = "agent/foreign-report-control".into(),
            }
            assert!(
                !record_checked_report(&state.store, &foreign, &report),
                "{change}"
            );
        }
        assert!(record_checked_report(&state.store, &current, &report));
        let observation = admitted_report(&current, &report).unwrap();
        assert_eq!(observation["epoch"], current.epoch);
        assert_eq!(observation["pid"], std::process::id());
        assert!(observation["start_ticks"].as_u64().is_some());
        assert!(observation.get("token").is_none());
        assert_ne!(observation["token_sha256"], current.token);
        let mut title = current;
        title.component = "title".into();
        assert!(admitted_report(&title, &report).is_none());
    }

    #[tokio::test]
    async fn attachment_checks_both_runtime_and_current_delivery_epoch() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let subject = "agent/attachment-epoch";
        for (kind, fields) in [
            ("runtime.observed", json!({"status":"running","runtime_id":"attachment-epoch","incarnation_id":"current"})),
            ("harness.observed", json!({"state":"idle","driver":"claude","incarnation_id":"current"})),
        ] {
            state.store.append_claim(&ClaimInput {
                subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
                fields: serde_json::from_value(fields).unwrap(), evidence: vec![],
                expected_subject: None, idempotency_key: Some(format!("attachment-epoch:{kind}")),
            }).unwrap();
        }
        let title = state.store.bind_mailbox(&Fence::new(subject, "current", "title")).unwrap();
        let delivery = state.store.bind_mailbox(&Fence::new(subject, "current", "delivery")).unwrap();
        let peer = NativeDeliveryPeer { agent: subject.into(), transport: "claude-channel", pid: 7, archives_inbox: true };
        let ready = json!({"transport":"claude-channel","ready":true,"channel":{"pid":8,"age_ms":0}}).to_string();
        delivery_presence::record_fenced(&delivery, &ready);
        assert!(attachment(State(state.clone()), Query(title.clone()), Some(Extension(peer.clone()))).await.unwrap().0.attached);
        let replacement = state.store.bind_mailbox(&Fence::new(subject, "current", "delivery")).unwrap();
        assert!(!attachment(State(state.clone()), Query(title.clone()), Some(Extension(peer.clone()))).await.unwrap().0.attached,
            "a live report from a superseded delivery owner must not prove attachment");
        delivery_presence::record_fenced(&replacement, &ready);
        assert!(attachment(State(state.clone()), Query(title.clone()), Some(Extension(peer.clone()))).await.unwrap().0.attached);
        let mut foreign = title;
        foreign.incarnation = "previous".into();
        assert!(attachment(State(state), Query(foreign), Some(Extension(peer))).await.is_err());
    }

    async fn next(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    ) -> Frame {
        loop {
            match tokio::time::timeout(MAILBOX_RECHECK / 2, socket.next())
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

    fn age_sent(store: &Store, database: &Path, subject: &str) {
        let claim = store
            .latest_claim(subject, Some("message.sent"))
            .unwrap()
            .unwrap();
        let connection = rusqlite::Connection::open(database).unwrap();
        crate::store::configure_projection_writer(&connection).unwrap();
        connection
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                rusqlite::params![
                    (client_now_ms()
                        - u128::from(super::super::mail_backlog::THRESHOLD_MS)
                        - 1_000)
                        .to_string(),
                    claim.id
                ],
            )
            .unwrap();
    }

    async fn boot_and_reconnect_hold_old_mail(transport: &'static str) {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        let seat = "agent/eval.worker";
        let send = |id: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
                    kind: "message.sent".into(),
                    actor: Some("person/eval".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("person/eval")),
                        ("to".into(), json!(seat)),
                        ("content".into(), json!("QUARTZ SIGNAL")),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("send:{id}")),
                })
                .unwrap();
            signal_changed(&state);
        };
        for (id, status) in [
            ("boot-sent", "sent"),
            ("boot-staged", "staged"),
            ("boot-delivered", "delivered"),
        ] {
            send(id);
            if status == "sent" {
                age_sent(
                    &state.store,
                    &root.path().join("graph.db"),
                    &format!("message/{id}"),
                );
            }
            if status != "sent" {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: format!("message/{id}"),
                        kind: format!("message.{status}"),
                        actor: Some(seat.into()),
                        fields: BTreeMap::from([("status".into(), json!(status))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("status:{id}")),
                    })
                    .unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
        crate::mailbox::tests::ready(&state.store, "session-1");
        let peer = NativeDeliveryPeer {
            agent: seat.into(),
            transport,
            pid: 37,
            archives_inbox: true,
        };
        let app = admitted_fixture_router(state.clone(), peer);
        let path = root.path().join("daemon.sock");
        let server_path = path.clone();
        let server = tokio::spawn(async move { serve_unix(&server_path, app).await.unwrap() });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = Client::new(Endpoint::Unix(path));
        let fence: Fence = client
            .post(
                "/v1/mailbox/bind",
                &Fence::new(seat, "session-1", "delivery"),
            )
            .await
            .unwrap();
        let mut socket = client.open_mailbox(&fence).await.unwrap();
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty()),
            "{transport} injected pre-boot mail"
        );

        // Legacy native polling receives archive projections; the graph and explicit reads
        // retain the original mail and lifecycle, rather than synthesizing receipts or closes.
        let legacy: Vec<crate::model::MessageView> = client
            .get("/v1/messages?to=agent%2Feval.worker")
            .await
            .unwrap();
        assert!(legacy.iter().all(|message| message.status == "closed"));
        let page: crate::model::MessagePage = client
            .get("/v1/messages/page?to=agent%2Feval.worker")
            .await
            .unwrap();
        assert!(page.items.iter().all(|message| message.status == "closed"));
        let manual = list_messages(
            State(state.clone()),
            Query(MessagesQuery {
                to: Some(seat.into()),
                include_closed: false,
            }),
            None,
        )
        .await
        .unwrap()
        .0;
        assert_eq!(manual.len(), 3);
        assert_eq!(
            manual
                .iter()
                .filter(|message| message.status == "closed")
                .count(),
            0
        );
        let old: crate::model::MessageView =
            client.get("/v1/messages/read/boot-staged").await.unwrap();
        assert_eq!(old.status, "staged");

        send("live-first");
        let legacy: Vec<crate::model::MessageView> = client
            .get("/v1/messages?to=agent%2Feval.worker")
            .await
            .unwrap();
        assert!(
            legacy
                .iter()
                .any(|message| message.subject == "message/live-first" && message.status == "sent")
        );
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.len() == 1 && messages[0].subject == "message/live-first"),
            "{transport} dropped live mail"
        );
        client
            .post::<_, ClaimRecord>(
                "/v1/mailbox/receipts",
                &Receipt {
                    fence: fence.clone(),
                    message: "message/live-first".into(),
                    lifecycle: "staged".into(),
                },
            )
            .await
            .unwrap();
        socket.close(None).await.unwrap();
        let mut socket = client.open_mailbox(&fence).await.unwrap();
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty()),
            "{transport} replayed pre-reconnect mail"
        );
        send("live-second");
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.len() == 1 && messages[0].subject == "message/live-second")
        );
        assert_eq!(
            state
                .store
                .message("message/live-first")
                .unwrap()
                .unwrap()
                .status,
            "staged"
        );
        assert!(
            state
                .store
                .claims_for("message/boot-staged", Some("message.closed"))
                .unwrap()
                .is_empty()
        );
        server.abort();
    }

    #[test]
    fn native_poll_recovers_unoffered_preboot_mail_until_its_current_offer_finishes() {
        for transport in [
            "claude-channel",
            "omp-channel",
            "pi-channel",
            "app-server",
            "opencode-server",
        ] {
            let root = tempfile::tempdir().unwrap();
            let database = root.path().join("graph.db");
            let store = Store::open(&database, "node").unwrap();
            let seat = "agent/eval.worker";
            let peer = NativeDeliveryPeer {
                agent: seat.into(),
                transport,
                pid: 37,
                archives_inbox: true,
            };
            for (id, phase) in [
                ("fresh", "sent"),
                ("hour-old", "sent"),
                ("offered", "staged"),
                ("accepted", "delivered"),
            ] {
                let subject = format!("message/{id}");
                store
                    .append_claim(&ClaimInput {
                        subject: subject.clone(),
                        kind: "message.sent".into(),
                        actor: Some("person/eval".into()),
                        fields: BTreeMap::from([
                            ("status".into(), json!("sent")),
                            ("from".into(), json!("person/eval")),
                            ("to".into(), json!(seat)),
                            ("content".into(), json!("QUARTZ SIGNAL")),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("send:{id}")),
                    })
                    .unwrap();
                if id == "hour-old" {
                    age_sent(&store, &database, &subject);
                }
                if phase != "sent" {
                    store
                        .append_claim(&ClaimInput {
                            subject,
                            kind: format!("message.{phase}"),
                            actor: Some(seat.into()),
                            fields: BTreeMap::from([("status".into(), json!(phase))]),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(format!("offer:{id}")),
                        })
                        .unwrap();
                }
            }
            crate::mailbox::tests::ready(&store, "first-boot");
            let mut messages = store.messages(Some(seat), false).unwrap();
            hold_pre_boot_mail(&store, Some(&peer), Some(seat), &mut messages).unwrap();
            assert!(messages.iter().all(|m| m.status
                == if m.subject == "message/fresh" {
                    "sent"
                } else {
                    "closed"
                }));
            for phase in ["staged", "delivered"] {
                store
                    .append_claim(&ClaimInput {
                        subject: "message/fresh".into(),
                        kind: format!("message.{phase}"),
                        actor: Some(seat.into()),
                        fields: BTreeMap::from([("status".into(), json!(phase))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("fresh:{phase}")),
                    })
                    .unwrap();
                let mut messages = store.messages(Some(seat), false).unwrap();
                hold_pre_boot_mail(&store, Some(&peer), Some(seat), &mut messages).unwrap();
                assert!(
                    messages
                        .iter()
                        .find(|m| m.subject == "message/fresh")
                        .is_some_and(|m| m.status == phase),
                    "{transport} removed recovery during current {phase}"
                );
            }
            crate::mailbox::tests::ready(&store, "second-boot");
            let mut messages = store.messages(Some(seat), false).unwrap();
            hold_pre_boot_mail(&store, Some(&peer), Some(seat), &mut messages).unwrap();
            assert!(messages.iter().all(|m| m.status == "closed"));
            assert_eq!(
                store.message("message/fresh").unwrap().unwrap().status,
                "delivered"
            );
            assert!(
                store
                    .claims_for("message/fresh", Some("message.read"))
                    .unwrap()
                    .is_empty()
            );
            assert!(
                store
                    .claims_for("message/fresh", Some("message.closed"))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn recovery_uses_original_send_age_including_late_imports_and_the_one_hour_boundary() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("graph.db");
        let store = Store::open(&database, "node").unwrap();
        let since = client_now_ms();
        let threshold = u128::from(super::super::mail_backlog::THRESHOLD_MS);
        let send_at = |id: &str, at: u128| {
            let claim = store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
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
                    idempotency_key: Some(format!("send:{id}")),
                })
                .unwrap();
            let connection = rusqlite::Connection::open(&database).unwrap();
            crate::store::configure_projection_writer(&connection).unwrap();
            connection
                .execute(
                    "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                    rusqlite::params![at.to_string(), claim.id],
                )
                .unwrap();
        };
        send_at("older", since - threshold - 1);
        send_at("exact-hour", since - threshold);
        send_at("recent", since - threshold + 1);
        let through = store.index().unwrap();
        send_at("late-old", since - threshold - 1);
        let mut messages = store.messages(Some("agent/eval.worker"), false).unwrap();
        let mut admitted = std::collections::BTreeSet::new();
        retain_live_mail(&store, &mut messages, since, Some(through), &mut admitted).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m.subject.as_str())
                .collect::<Vec<_>>(),
            ["message/recent"]
        );
    }

    async fn restart_recovers_recent_unoffered_mail_once(transport: &'static str) {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        let database = root.path().join("graph.db");
        state.store = Arc::new(Store::open(&database, "node").unwrap());
        let seat = "agent/eval.worker";
        crate::mailbox::tests::ready(&state.store, "session-1");
        let send = |id: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
                    kind: "message.sent".into(),
                    actor: Some("person/eval".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("person/eval")),
                        ("to".into(), json!(seat)),
                        ("content".into(), json!("QUARTZ SIGNAL")),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("send:{id}")),
                })
                .unwrap();
            signal_changed(&state);
        };
        for (id, status) in [
            ("hour-old", "sent"),
            ("already-staged", "staged"),
            ("already-delivered", "delivered"),
        ] {
            send(id);
            if status == "sent" {
                age_sent(&state.store, &database, &format!("message/{id}"));
            } else {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: format!("message/{id}"),
                        kind: format!("message.{status}"),
                        actor: Some(seat.into()),
                        fields: BTreeMap::from([("status".into(), json!(status))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("offer:{id}")),
                    })
                    .unwrap();
            }
        }
        let peer = NativeDeliveryPeer {
            agent: seat.into(),
            transport,
            pid: 37,
            archives_inbox: true,
        };
        let app = admitted_fixture_router(state.clone(), peer);
        let path = root.path().join("daemon.sock");
        let start = || {
            let app = app.clone();
            let path = path.clone();
            tokio::spawn(async move { serve_unix(&path, app).await.unwrap() })
        };
        let server = start();
        let client = Client::new(Endpoint::Unix(path.clone()));
        for _ in 0..100 {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let fence: Fence = client
            .post(
                "/v1/mailbox/bind",
                &Fence::new(seat, "session-1", "delivery"),
            )
            .await
            .unwrap();
        let mut socket = client.open_mailbox(&fence).await.unwrap();
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty())
        );
        socket.close(None).await.unwrap();
        send("just-before-restart");
        server.abort();
        let _ = server.await;
        let server = start();
        for _ in 0..100 {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut socket = client.open_mailbox(&fence).await.unwrap();
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages }
            if messages.len() == 1 && messages[0].subject == "message/just-before-restart"),
            "{transport} lost recent unoffered mail across daemon restart"
        );
        let legacy: Vec<crate::model::MessageView> = client
            .get("/v1/messages?to=agent%2Feval.worker")
            .await
            .unwrap();
        assert!(
            legacy
                .iter()
                .find(|m| m.subject == "message/hour-old")
                .is_some_and(|m| m.status == "closed")
        );
        for lifecycle in ["staged", "delivered", "read"] {
            let receipt = Receipt {
                fence: fence.clone(),
                message: "message/just-before-restart".into(),
                lifecycle: lifecycle.into(),
            };
            client
                .post::<_, ClaimRecord>("/v1/mailbox/receipts", &receipt)
                .await
                .unwrap();
            if lifecycle == "staged" {
                assert!(
                    matches!(next(&mut socket).await, Frame::Mailbox { messages }
                    if messages.len() == 1 && messages[0].status == "staged"),
                    "{transport} removed an admitted recovery before its receipts finished"
                );
            }
            client
                .post::<_, ClaimRecord>("/v1/mailbox/receipts", &receipt)
                .await
                .unwrap();
            assert_eq!(
                state
                    .store
                    .claims_for(
                        "message/just-before-restart",
                        Some(&format!("message.{lifecycle}"))
                    )
                    .unwrap()
                    .len(),
                1
            );
        }
        socket.close(None).await.unwrap();
        let mut socket = client.open_mailbox(&fence).await.unwrap();
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty()),
            "{transport} replayed recovered or historical mail"
        );
        assert!(
            state
                .store
                .claims_for("message/hour-old", Some("message.staged"))
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .store
                .claims_for("message/already-staged", Some("message.delivered"))
                .unwrap()
                .is_empty()
        );
        server.abort();
    }

    #[tokio::test]
    async fn claude_restart_recovers_recent_unoffered_mail_once() {
        restart_recovers_recent_unoffered_mail_once("claude-channel").await;
    }

    #[tokio::test]
    async fn omp_restart_recovers_recent_unoffered_mail_once() {
        restart_recovers_recent_unoffered_mail_once("omp-channel").await;
    }

    #[tokio::test]
    async fn pi_restart_recovers_recent_unoffered_mail_once() {
        restart_recovers_recent_unoffered_mail_once("pi-channel").await;
    }

    #[tokio::test]
    async fn codex_restart_recovers_recent_unoffered_mail_once() {
        restart_recovers_recent_unoffered_mail_once("app-server").await;
    }

    #[tokio::test]
    async fn opencode_restart_recovers_recent_unoffered_mail_once() {
        restart_recovers_recent_unoffered_mail_once("opencode-server").await;
    }

    #[tokio::test]
    async fn claude_boot_and_reconnect_never_inject_old_mail() {
        boot_and_reconnect_hold_old_mail("claude-channel").await;
    }
    #[tokio::test]
    async fn omp_boot_and_reconnect_never_inject_old_mail() {
        boot_and_reconnect_hold_old_mail("omp-channel").await;
    }
    #[tokio::test]
    async fn pi_boot_and_reconnect_never_inject_old_mail() {
        boot_and_reconnect_hold_old_mail("pi-channel").await;
    }
    #[tokio::test]
    async fn codex_boot_and_reconnect_never_inject_old_mail() {
        boot_and_reconnect_hold_old_mail("app-server").await;
    }

    #[tokio::test]
    async fn opencode_boot_and_reconnect_never_inject_old_mail() {
        boot_and_reconnect_hold_old_mail("opencode-server").await;
    }

    #[tokio::test]
    async fn a_restarted_native_mailbox_holds_a_watch_wake_queued_while_the_seat_was_stopped() {
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
            state.store.record_resource_observation(&thread.observer(), &state.store.selected_desired_revision(&thread.observer()).unwrap().unwrap(), None, &thread.resource(), None, &json!({"repository_id": 7, "issues": [{"number": 12, "new": false, "state": "open", "recent_comments": comments}]}), 0, &subscriptions, None).unwrap();
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
        age_sent(
            &state.store,
            &root.path().join("graph.db"),
            &queued[0].subject,
        );
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
        let app = admitted_fixture_router(state.clone(), peer);
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
            matches!(next(&mut mailbox).await, Frame::Mailbox { messages } if messages.is_empty())
        );
        assert_eq!(
            state.store.messages(Some(seat), false).unwrap()[0].status,
            "sent"
        );
        server.abort();
    }

    #[tokio::test]
    async fn admitted_transports_deliver_live_mail_and_receipts_over_a_real_unix_push_stream_without_files()
     {
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
                admitted_fixture_router(state.clone(), peer)
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
            // A staged offer stays held after reconnect, even when its original send is recent.
            client
                .post::<_, ClaimRecord>(
                    "/v1/mailbox/receipts",
                    &Receipt {
                        fence: fence.clone(),
                        message: send.subject.clone(),
                        lifecycle: "staged".into(),
                    },
                )
                .await
                .unwrap();
            socket.close(None).await.unwrap();
            let mut socket = client.open_mailbox(&fence).await.unwrap();
            next(&mut socket).await;
            assert!(
                matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty())
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
    async fn current_owner_recovers_recent_unoffered_mail_after_an_injected_snapshot_failure() {
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
                                        _ => raw_snapshot(store, fence),
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
        assert!(matches!(frame, Frame::Mailbox { messages }
            if messages.len() == 1 && messages[0].subject == input.subject));
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

    #[tokio::test]
    async fn a_write_during_the_first_snapshot_is_delivered_without_waiting_for_safety() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        crate::mailbox::tests::ready(&state.store, "session-1");
        let fence = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let app =
            Router::new()
                .route(
                    "/v1/mailbox",
                    get(
                        move |State(state): State<AppState>,
                              Query(fence): Query<Fence>,
                              websocket: WebSocketUpgrade| {
                            let injected = injected.clone();
                            async move {
                                let events = state.event_notify.clone();
                                websocket.on_upgrade(move |socket| {
                                    stream_with_rechecks(
                                        state,
                                        fence,
                                        socket,
                                        move |store, fence| {
                                            let result = raw_snapshot(store, fence)?;
                                            if !injected
                                                .swap(true, std::sync::atomic::Ordering::SeqCst)
                                            {
                                                std::thread::scope(|scope| {
                                                    scope
                                                        .spawn(|| {
                                                            store.append_claim(&ClaimInput {
                                                subject: "message/during-snapshot".into(),
                                                kind: "message.sent".into(),
                                                actor: Some("person/fixture".into()),
                                                fields: BTreeMap::from([
                                                    ("status".into(), json!("sent")),
                                                    ("from".into(), json!("person/fixture")),
                                                    ("to".into(), json!("agent/eval.worker")),
                                                    (
                                                        "content".into(),
                                                        json!("Written after the read."),
                                                    ),
                                                ]),
                                                evidence: vec![],
                                                expected_subject: None,
                                                idempotency_key: None,
                                            })
                                                        })
                                                        .join()
                                                        .unwrap()
                                                })?;
                                                events.send_modify(|generation| *generation += 1);
                                            }
                                            Ok(result)
                                        },
                                        futures_util::stream::pending(),
                                    )
                                })
                            }
                        },
                    ),
                )
                .with_state(state);
        let path = root.path().join("daemon.sock");
        let server_path = path.clone();
        let server = tokio::spawn(async move { serve_unix(&server_path, app).await.unwrap() });
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = Client::new(Endpoint::Unix(path));
        let mut socket = client.open_mailbox(&fence).await.unwrap();
        assert!(
            matches!(next(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty())
        );
        let frame = tokio::time::timeout(MAILBOX_RECHECK / 2, next(&mut socket))
            .await
            .unwrap();
        assert!(
            matches!(frame, Frame::Mailbox { messages } if messages.len() == 1 && messages[0].subject == "message/during-snapshot")
        );
        server.abort();
    }

    struct ControlledStream {
        dirty_gate: Arc<FixtureDirtyGate>,
        socket: tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
        recheck: tokio::sync::mpsc::UnboundedSender<()>,
        heartbeat: tokio::sync::mpsc::UnboundedSender<()>,
        reads: Arc<std::sync::atomic::AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for ControlledStream {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn controlled_stream(state: AppState, fence: &Fence, path: &Path) -> ControlledStream {
        controlled_stream_with_peer(state, fence, path, None).await
    }

    async fn controlled_stream_with_peer(
        state: AppState, fence: &Fence, path: &Path, peer: Option<NativeDeliveryPeer>,
    ) -> ControlledStream {
        let (recheck, receiver) = tokio::sync::mpsc::unbounded_channel();
        let receiver = Arc::new(std::sync::Mutex::new(Some(receiver)));
        let (heartbeat, heartbeats) = tokio::sync::mpsc::unbounded_channel();
        let heartbeats = Arc::new(std::sync::Mutex::new(Some(heartbeats)));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reads.clone();
        let dirty_gate = Arc::new(FixtureDirtyGate {
            allowed: std::sync::atomic::AtomicBool::new(true),
            notifications: std::sync::atomic::AtomicUsize::new(0),
        });
        let fixture_gate = dirty_gate.clone();
        let app = Router::new()
            .route(
                "/v1/mailbox",
                get(
                    move |State(state): State<AppState>,
                          Query(fence): Query<Fence>,
                          websocket: WebSocketUpgrade| {
                        let receiver = receiver.lock().unwrap().take().unwrap();
                        let heartbeats = heartbeats.lock().unwrap().take().unwrap();
                        let counted = counted.clone();
                        let fixture_gate = fixture_gate.clone();
                        let peer = peer.clone();
                        async move {
                            websocket.on_upgrade(move |socket| {
                                let ticks = futures_util::stream::unfold(
                                    receiver,
                                    |mut receiver| async move {
                                        receiver.recv().await.map(|()| ((), receiver))
                                    },
                                );
                                stream_with_timers_inner(
                                    state,
                                    fence,
                                    socket,
                                    move |store, fence| {
                                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        raw_snapshot(store, fence)
                                    },
                                    ticks,
                                    futures_util::stream::unfold(
                                        heartbeats,
                                        |mut receiver| async move {
                                            receiver.recv().await.map(|()| ((), receiver))
                                        },
                                    ),
                                    StreamControl { peer, dirty_gate: Some(fixture_gate) },
                                )
                            })
                        }
                    },
                ),
            )
            .route(
                "/v1/internal/replication/receive",
                post(super::super::replication_receive),
            )
            .layer(from_fn_with_state(
                (state.clone(), ClientTransportBoundary::Unix),
                response_envelope,
            ))
            .with_state(state);
        let server_path = path.to_owned();
        let server = tokio::spawn(async move { serve_unix(&server_path, app).await.unwrap() });
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = Client::new(Endpoint::Unix(path.to_owned()));
        let mut socket = client.open_mailbox(fence).await.unwrap();
        let mut initial = next(&mut socket).await;
        if matches!(initial, Frame::Seat { .. }) { initial = next(&mut socket).await; }
        assert!(matches!(initial, Frame::Mailbox { messages } if messages.is_empty()));
        ControlledStream {
            dirty_gate,
            socket,
            recheck,
            heartbeat,
            reads,
            server,
        }
    }

    #[cfg(all(feature = "test-support", target_os = "linux"))]
    #[tokio::test]
    async fn native_report_refusal_cannot_ack_drain_or_negotiate_replay() {
        report_control_effects(true).await;
    }

    #[cfg(all(feature = "test-support", target_os = "linux"))]
    #[tokio::test]
    async fn legacy_report_keeps_control_admission_separate_from_typed_presence() {
        report_control_effects(false).await;
    }

    #[cfg(all(feature = "test-support", target_os = "linux"))]
    async fn report_control_effects(native: bool) {
        use crate::store::owned_sets::{Options, Source};
        use st_drivers::{harness_events, harness_state};
        use std::sync::atomic::Ordering::SeqCst;
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open_memory("node").unwrap());
        let name = if native { "eval.report-effects-native" } else { "eval.report-effects-legacy" };
        let subject = format!("agent/{name}");
        let input = crate::graph::parse_owned_set_intent(&format!(
            "version 2\nagent {name:?} {{ host \"node\"; workspace \"/tmp\"; harness \"omp\" {{}} }}"
        ), "node").unwrap();
        let policy = crate::rollout::Policy::when_idle(1_800_000, false);
        let mut options = Options {
            set: name.into(), source: Source { repository: "fixture/report".into(),
                r#ref: "refs/heads/main".into(), sha: "1".repeat(40), sequence: 1 },
            expected_set: "absent".into(), rollout: Some(policy.clone()),
            adopt: Default::default(), allow_empty: false, confirm_retire: None,
            expected_subjects: Default::default(),
        };
        let preview = state.store.owned_set_preview(&input, &options).unwrap();
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        options.expected_subjects = preview.expected_subjects;
        state.store.apply_owned_set(&input, &options, "report-publish", "person/test").unwrap();
        let selected = state.store.rollout_selection(&subject).unwrap().unwrap();
        let member = selected.desired.member.as_ref().unwrap();
        state.store.append_claim(&ClaimInput {
            subject: subject.clone(), kind: "runtime.observed".into(), actor: Some(subject.clone()),
            fields: serde_json::from_value(json!({"status":"running", "host":"node",
                "runtime_id":member.runtime_id, "incarnation_id":"current"})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        let agent_dir = crate::hooks::claude_agent_dir(&state.state_dir.join("drivers"), &subject, "node");
        harness_events::enable(&agent_dir, "current").unwrap();
        let sequence = harness_state::claim(&agent_dir, name, "omp", "report-session").unwrap();
        let raw = harness_events::read_runtime_state(&agent_dir, "current").unwrap().unwrap();
        let peer = NativeDeliveryPeer { agent: subject.clone(), transport: "omp-channel",
            pid: std::process::id(), archives_inbox: true };
        // An explicitly already-admitted fixture lease isolates report consumption.
        // This is not a physical bind/launch authentication certificate.
        let owner = crate::mailbox::Authority { provider: "omp".into(), session: "report-session".into(),
            sequence, pid: peer.pid, process_token: st_runtime::process_start_token(peer.pid).unwrap() };
        let fence = state.store.bind_mailbox_with_lease(&Fence::new(&subject, "current", "delivery"), Some(&owner)).unwrap();
        state.store.request_rollout(&subject, &selected.desired_token, member, "current",
            "person/test", &policy, "report-drain").unwrap();
        let operation = state.store.rollout(&subject).unwrap().unwrap();
        let mut stream = controlled_stream_with_peer(state.clone(), &fence, &root.path().join("report.sock"),
            native.then(|| peer.clone())).await;
        assert!(matches!(next(&mut stream.socket).await,
            Frame::Drain { operation: Some(id) } if id == operation.id));
        stream.dirty_gate.allowed.store(false, SeqCst);
        let report = |reason: &str| json!({"transport":"omp-channel", "pid":peer.pid,
            "ready":false, "reason":reason});
        let baseline = report("baseline");
        assert!(authority::admit_report(&state, &fence, &peer, &baseline.to_string()).is_ok());
        stream.socket.send(Message::Text(baseline.to_string().into())).await.unwrap();
        stream.socket.send(Message::Ping(vec![0].into())).await.unwrap();
        expect_pong(&mut stream.socket, &[0]).await;
        assert_eq!(delivery_presence::assess_current(&subject,"omp",Some("current")).reason.as_deref(), Some("baseline"));
        let tap_count = || std::env::var_os("ST3_TEST_ADMITTED_REPORTS").map(|path| {
            std::fs::read_to_string(path).unwrap_or_default().lines().filter(|line|
                serde_json::from_str::<Value>(line).is_ok_and(|value| value["subject"] == subject)).count()
        });
        let initial_taps = tap_count();
        let index = state.store.index().unwrap();
        if native {
            for (number, refusal) in ["pid", "transport", "provider"].into_iter().enumerate() {
                let mut rejected = report("must-not-be-recorded");
                rejected["mailbox_replay_ack"] = json!(true);
                rejected["drain_operation"] = json!(operation.id);
                match refusal {
                    "pid" => rejected["pid"] = json!(u64::from(peer.pid) + 1),
                    "transport" => rejected["transport"] = json!("claude-channel"),
                    _ => {
                        let mut foreign: Value = serde_json::from_slice(&raw).unwrap();
                        foreign["harness"] = json!("codex");
                        harness_events::write_snapshot(&agent_dir, "harness-state", &serde_json::to_vec(&foreign).unwrap()).unwrap();
                    }
                }
                assert!(state.store.check_mailbox(&fence).is_ok(), "Store fence must remain current");
                assert!(authority::admit_report(&state, &fence, &peer, &rejected.to_string()).is_err(), "{refusal}");
                stream.socket.send(Message::Text(rejected.to_string().into())).await.unwrap();
                let barrier = [number as u8 + 1];
                stream.socket.send(Message::Ping(barrier.to_vec().into())).await.unwrap();
                expect_pong(&mut stream.socket, &barrier).await;
                assert!(state.store.rollout(&subject).unwrap().unwrap().drain_ack.is_none(), "{refusal}");
                assert_eq!(state.store.index().unwrap(), index, "no drain/recovery claim: {refusal}");
                assert_eq!(delivery_presence::assess_current(&subject,"omp",Some("current")).reason.as_deref(), Some("baseline"));
                assert_eq!(tap_count(), initial_taps, "no admitted tap: {refusal}");
                harness_events::write_snapshot(&agent_dir, "harness-state", &raw).unwrap();
            }
        }
        // Native authority can accept a control even when optional typed presence is invalid.
        let mut malformed = report("must-not-replace-presence");
        malformed["image"] = json!(42);
        malformed["drain_operation"] = json!(operation.id);
        assert!(authority::admit_report(&state, &fence, &peer, &malformed.to_string()).is_ok());
        stream.socket.send(Message::Text(malformed.to_string().into())).await.unwrap();
        stream.socket.send(Message::Ping(vec![4].into())).await.unwrap();
        expect_pong(&mut stream.socket, &[4]).await;
        assert!(state.store.rollout(&subject).unwrap().unwrap().drain_ack.is_some());
        assert_eq!(delivery_presence::assess_current(&subject,"omp",Some("current")).reason.as_deref(), Some("baseline"));
        assert_eq!(tap_count(), initial_taps);
        let mut accepted = report("later-admissible");
        accepted["mailbox_replay_ack"] = json!(true);
        stream.socket.send(Message::Text(accepted.to_string().into())).await.unwrap();
        stream.socket.send(Message::Ping(vec![5].into())).await.unwrap();
        if native { assert!(matches!(next(&mut stream.socket).await, Frame::Replay { .. })); }
        expect_pong(&mut stream.socket, &[5]).await;
        assert_eq!(delivery_presence::assess_current(&subject,"omp",Some("current")).reason.as_deref(), Some("later-admissible"));
        assert_eq!(tap_count(), initial_taps.map(|count| count + 1));
    }

    #[tokio::test]
    async fn argv_bound_stream_refuses_changed_declaration_reads_and_reports() {
        argv_changed_stream_control(true).await;
    }

    #[tokio::test]
    async fn argv_desired_change_wakes_and_fences_bound_stream() {
        argv_changed_stream_control(false).await;
    }

    async fn argv_changed_stream_control(hold_dirty: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        for change in ["stop", "host", "native", "argv"] {
            let root = tempfile::tempdir().unwrap();
            let mut state = super::super::tests::state(root.path());
            state.store = Arc::new(Store::open_memory("node").unwrap());
            let name = format!("eval.argv-continuation-{change}");
            let subject = format!("agent/{name}");
            let initial = format!("version 2\nagent {name:?} {{ host \"node\"; workspace \"/tmp\"; argv \"python3\" \"probe\"; }}");
            let intent = crate::graph::parse_intent(&initial,"node").unwrap();
            state.store.apply_internal(&intent,"argv-stream-initial").unwrap();
            let runtime = state.store.append_claim(&ClaimInput {
                subject: subject.clone(), kind: "runtime.observed".into(), actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([("status".into(),json!("running")),("incarnation_id".into(),json!("current")),
                    ("runtime_id".into(),json!("argv-stream")),("host".into(),json!("node"))]),
                evidence: vec![], expected_subject: None, idempotency_key: None,
            }).unwrap();
            let member = state.store.desired_subjects_named(std::slice::from_ref(&subject)).unwrap().remove(0).member.unwrap();
            // Synthetic already-authenticated bind isolates continuation fencing;
            // actual kernel/process admission is covered by production argv fixtures.
            let fence = state.store.bind_argv_mailbox_checked(&Fence::new(&subject,"current","delivery"),&member,&|| Ok(())).unwrap();
            assert!(raw_snapshot(&state.store,&fence).is_ok());
            let mut stream = controlled_stream(state.clone(),&fence,&root.path().join("argv.sock")).await;
            let original = format!("accepted-{change}");
            let report = |reason: &str| Message::Text(json!({"transport":"omp-channel","pid":37,"ready":false,"reason":reason}).to_string().into());
            stream.socket.send(report(&original)).await.unwrap();
            let deadline = tokio::time::Instant::now()+Duration::from_secs(2);
            while delivery_presence::assess_current(&subject,"omp",Some("current")).reason.as_deref()!=Some(&original) {
                assert!(tokio::time::Instant::now()<deadline,"unchanged argv report was not admitted");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            stream.socket.send(Message::Ping(vec![0].into())).await.unwrap();
            expect_pong(&mut stream.socket, &[0]).await;
            let notifications = stream.dirty_gate.notifications.load(SeqCst);
            let reads = stream.reads.load(SeqCst);
            // Keep every wake route intact, but hold dirty processing so neither
            // subject/owner notifications nor a resync/timer can rescue this check.
            stream.dirty_gate.allowed.store(!hold_dirty, SeqCst);
            let replacement = match change {
                "stop" => format!("version 2\nstop {subject:?}"),
                "host" => format!("version 2\nagent {name:?} {{ host \"other\"; workspace \"/tmp\"; argv \"python3\" \"probe\"; }}"),
                "native" => format!("version 2\nagent {name:?} {{ host \"node\"; workspace \"/tmp\"; harness \"omp\" {{}} }}"),
                "argv" => format!("version 2\nagent {name:?} {{ host \"node\"; workspace \"/tmp\"; argv \"python3\" \"different-program\"; }}"),
                _ => unreachable!(),
            };
            let intent = crate::graph::parse_intent(&replacement,"node").unwrap();
            state.store.apply_internal(&intent,"argv-stream-changed").unwrap();
            // Direct Store writes do not publish the API post-commit feed. Supply
            // the same committed-change signal used by production callers.
            signal_changed(&state);
            assert_eq!(state.store.latest_claim(&subject,Some("runtime.observed")).unwrap().unwrap().id,runtime.id);
            assert!(raw_snapshot(&state.store,&fence).is_err());
            if !hold_dirty {
                assert!(matches!(next(&mut stream.socket).await, Frame::Fenced { .. }));
                continue;
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                while stream.dirty_gate.notifications.load(SeqCst) == notifications {
                    tokio::task::yield_now().await;
                }
            }).await.expect("healthy desired-change dispatcher did not reach the stream");
            assert_eq!(state.store.mailbox_wake_health().status, "pass");
            stream.recheck.send(()).unwrap();
            stream.heartbeat.send(()).unwrap();
            stream.socket.send(report("must-not-be-recorded")).await.unwrap();
            stream.socket.send(Message::Ping(vec![1].into())).await.unwrap();
            expect_pong(&mut stream.socket, &[1]).await;
            assert_eq!(stream.reads.load(SeqCst), reads, "dirty snapshot processing must remain held");
            assert_eq!(delivery_presence::assess_current(&subject,"omp",Some("current")).reason.as_deref(),Some(original.as_str()));
            assert!(state.store.mailbox_lease_authority(&fence).unwrap().is_none());
        }
    }

    async fn expect_pong(socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>, payload: &[u8]) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match socket.next().await.expect("stream closed before report acknowledgement").unwrap() {
                    Message::Pong(bytes) if bytes.as_ref() == payload => return,
                    Message::Ping(_) | Message::Pong(_) => {},
                    other => panic!("expected post-report Pong, received {other:?}"),
                }
            }
        }).await.expect("report processing was not observed");
    }

    #[tokio::test]
    async fn replication_worker_receive_delivers_and_fences_without_safety_ticks() {
        const FLEET: &str = "18ba3167-11fb-472c-8ff8-e46e0fefb1e4";
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "target").unwrap());
        state.store.bind_fleet(FLEET).unwrap();
        let source = Store::open_memory("source").unwrap();
        source.bind_fleet(FLEET).unwrap();
        crate::mailbox::tests::ready(&source, "session-1");
        let request = || ReplicationReceiveRequest {
            peer: "source".into(),
            fleet_id: FLEET.into(),
            exchange: source
                .export_replication_exchange(FLEET, &state.store.replication_inventory().unwrap())
                .unwrap(),
            // The native replication worker supplies this; it enables the worker's heal path.
            round_trip_ms: Some(1),
        };
        // Establish the remote incarnation before binding the live stream.
        let Json(initial) = super::super::replication_receive(State(state.clone()), Json(request()))
            .await
            .unwrap();
        assert!(initial.changed);
        let fence = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let path = root.path().join("daemon.sock");
        let mut stream = controlled_stream(state.clone(), &fence, &path).await;
        let client = Client::unix(&path);
        source
            .append_claim(&ClaimInput {
                subject: "message/worker-arrival".into(),
                kind: "message.sent".into(),
                actor: Some("person/fixture".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/fixture")),
                    ("to".into(), json!("agent/eval.worker")),
                    ("content".into(), json!("An arriving fixture note.")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        // Use the same Unix endpoint/request as peer::Daemon::receive, not a local claim API
        // or a manually signaled event. Its receive/admit/project/notify path must route this.
        let arrival: ReplicationReceiveResponse = client
            .post("/v1/internal/replication/receive", &request())
            .await
            .unwrap();
        assert!(arrival.changed);
        assert!(matches!(
            next(&mut stream.socket).await,
            Frame::Mailbox { messages }
                if messages.len() == 1 && messages[0].subject == "message/worker-arrival"
        ));
        assert_eq!(stream.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Binding epochs/tokens are daemon-local and never replicated. A replicated live
        // incarnation replacement must instead wake the seat dependency and fence this boot.
        crate::mailbox::tests::ready(&source, "session-2");
        let replacement: ReplicationReceiveResponse = client
            .post("/v1/internal/replication/receive", &request())
            .await
            .unwrap();
        assert!(replacement.changed);
        assert!(matches!(
            next(&mut stream.socket).await,
            Frame::Fenced { .. }
        ));
        assert_eq!(
            stream.reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fencing must come from the durable delta check without a full snapshot"
        );
        assert_eq!(
            state.store.check_mailbox(&fence).unwrap_err().code,
            "stale-mailbox-session"
        );
        // No stream.recheck.send() occurred: neither assertion can pass via the safety timer.
    }

    #[tokio::test(start_paused = true)]
    async fn safety_timer_staggers_streams_with_a_bounded_interval() {
        let mut phases = std::collections::HashSet::new();
        for seat in 0..128 {
            let phase = safety_delay(&Fence::new(
                &format!("agent/fixture-{seat}"),
                "boot",
                "delivery",
            ));
            assert!(phase < MAILBOX_RECHECK);
            phases.insert(phase);
        }
        assert!(
            phases.len() > 100,
            "reconnecting streams must spread their first read"
        );
        let mut timer = safety_timer(&Fence::new("agent/fixture", "boot", "delivery"));
        let first = timer.tick().await;
        let second = timer.tick().await;
        assert_eq!(second - first, MAILBOX_RECHECK);
    }

    #[tokio::test]
    async fn safety_recheck_delivers_a_deliberately_missed_dependency() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        crate::mailbox::tests::ready(&state.store, "session-1");
        let fence = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let mut stream =
            controlled_stream(state.clone(), &fence, &root.path().join("daemon.sock")).await;
        state
            .store
            .miss_mailbox_recipient_for_test("agent/eval.worker");
        // A second recipient is a dispatcher barrier: after its wake the missing key has
        // definitely been processed. The safety source is controlled, so no timer races it.
        let mut barrier = state.store.subscribe_mailbox(
            &Fence::new("agent/barrier", "boot", "delivery"),
            &state.event_notify,
        );
        for (subject, to) in [
            ("message/missed-dependency", "agent/eval.worker"),
            ("message/barrier", "agent/barrier"),
        ] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.sent".into(),
                    actor: Some("person/fixture".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("person/fixture")),
                        ("to".into(), json!(to)),
                        ("content".into(), json!("A fixture note.")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        signal_changed(&state);
        tokio::time::timeout(MAILBOX_RECHECK / 2, barrier.changed.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stream.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        stream.recheck.send(()).unwrap();
        assert!(
            matches!(next(&mut stream.socket).await, Frame::Mailbox { messages } if messages.len() == 1 && messages[0].subject == "message/missed-dependency")
        );
        state
            .store
            .miss_mailbox_owner_for_test("agent/eval.worker", "delivery");
        state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        stream.recheck.send(()).unwrap();
        assert!(matches!(
            next(&mut stream.socket).await,
            Frame::Fenced { .. }
        ));
    }

    #[tokio::test]
    async fn a_direct_binding_wake_fences_a_live_stale_stream_without_a_recheck() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        crate::mailbox::tests::ready(&state.store, "session-1");
        let old = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let mut stream =
            controlled_stream(state.clone(), &old, &root.path().join("daemon.sock")).await;
        // No API/global-feed notification, no safety ticks, and no graph write: only the
        // bind transaction's targeted owner wake can promptly fence this live stream.
        let index = state.store.index().unwrap();
        let new = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        assert_eq!(state.store.index().unwrap(), index);
        assert!(state.store.check_mailbox(&new).is_ok());
        let frame = tokio::time::timeout(MAILBOX_RECHECK / 2, next(&mut stream.socket))
            .await
            .unwrap();
        assert!(matches!(frame, Frame::Fenced { .. }));
        assert_eq!(stream.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missed_owner_route_stays_silent_until_an_explicit_heartbeat_delta_check() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        crate::mailbox::tests::ready(&state.store, "session-1");
        let old = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let mut stream =
            controlled_stream(state.clone(), &old, &root.path().join("daemon.sock")).await;
        state
            .store
            .miss_mailbox_owner_for_test(&old.subject, &old.component);
        let index = state.store.index().unwrap();
        let new = state
            .store
            .bind_mailbox(&Fence::new(&old.subject, &old.incarnation, &old.component))
            .unwrap();
        assert_eq!(state.store.index().unwrap(), index);
        state.store.check_mailbox(&new).unwrap();
        // Both production timers are controlled and no global notification is emitted.
        // The original fifteen-second frame window must remain silent with the route removed.
        assert!(
            tokio::time::timeout(MAILBOX_RECHECK / 2, stream.socket.next())
                .await
                .is_err()
        );
        assert_eq!(stream.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        stream.heartbeat.send(()).unwrap();
        assert!(matches!(
            next(&mut stream.socket).await,
            Frame::Fenced { .. }
        ));
        assert_eq!(stream.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn initial_and_repair_mailbox_snapshots_include_commits_before_atomic_publication() {
        use std::sync::atomic::Ordering;

        let store = Store::open_memory("node").unwrap();
        crate::mailbox::tests::ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let floor = (0, store.index().unwrap());
        let send = |subject: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.sent".into(),
                    actor: Some("person/fixture".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("person/fixture")),
                        ("to".into(), json!(fence.subject)),
                        (
                            "content".into(),
                            json!("Visible before atomic publication."),
                        ),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        let reads = std::cell::Cell::new(0);
        let read = |store: &Store, fence: &Fence| {
            reads.set(reads.get() + 1);
            raw_snapshot(store, fence)
        };
        send("message/commit-gap-first");
        let committed = store.index().unwrap();
        // Reproduce the writer's reachable state between SQLite COMMIT and the atomic store.
        // The real append committed the message; only process-index publication is held back.
        store.committed_index.store(floor.1, Ordering::Release);
        assert!(
            store
                .messages(Some(&fence.subject), false)
                .unwrap()
                .is_empty()
        );
        let mut admitted = Default::default();
        let mut policies = Default::default();
        let (mark, view, _, _) = update_snapshot(
            &store,
            &fence,
            read,
            None,
            floor,
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert_eq!(
            view.1.len(),
            1,
            "the cursor must not skip the committed message"
        );
        store.committed_index.store(committed, Ordering::Release);
        let previous_committed = committed;

        let replacement = send("message/commit-gap-second");
        // This global repair has no claim on either message subject. It must request a full
        // resync even when the selected seat declaration and owner token stay unchanged.
        store
            .append_claim(&ClaimInput {
                subject: "repair/mailbox-fixture".into(),
                kind: "record.repaired".into(),
                actor: Some("person/fixture".into()),
                fields: BTreeMap::from([
                    ("record".into(), json!("record/mailbox-fixture")),
                    ("replacement".into(), json!(replacement.id)),
                    ("reason".into(), json!("A mailbox repair control.")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let committed = store.index().unwrap();
        store.committed_index.store(previous_committed, Ordering::Release);
        let (mark, view, _, _) = update_snapshot(
            &store,
            &fence,
            read,
            Some((mark, view)),
            floor,
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert_eq!(
            reads.get(),
            2,
            "record.repaired must resynchronize the mailbox"
        );
        assert_eq!(
            view.1.len(),
            2,
            "resync must include commits beyond the atomic"
        );
        store.committed_index.store(committed, Ordering::Release);
        let (_, view, updated, _) = update_snapshot(
            &store,
            &fence,
            read,
            Some((mark, view)),
            floor,
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(!updated);
        assert_eq!(view.1.len(), 2);
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn incremental_mailbox_matches_full_oracle_across_coalesced_lifecycle_changes() {
        let store = Store::open_memory("node").unwrap();
        crate::mailbox::tests::ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let reads = std::cell::Cell::new(0);
        let read = |store: &Store, fence: &Fence| {
            reads.set(reads.get() + 1);
            raw_snapshot(store, fence)
        };
        let since = client_now_ms();
        let through = store.index().unwrap();
        let mut admitted = std::collections::BTreeSet::new();
        let mut policies = std::collections::BTreeSet::new();
        let (mark, view, _, _) = update_snapshot(
            &store,
            &fence,
            read,
            None,
            (since, through),
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        let mut previous = Some((mark, view));
        let append = |subject: &str, phase: &str, to: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: format!("message.{phase}"),
                    actor: Some(
                        if phase == "closed" {
                            "daemon/runtime"
                        } else {
                            "person/fixture"
                        }
                        .into(),
                    ),
                    fields: if phase == "sent" {
                        BTreeMap::from([
                            ("status".into(), json!(phase)),
                            ("from".into(), json!("person/fixture")),
                            ("to".into(), json!(to)),
                            ("content".into(), json!("An oracle fixture.")),
                        ])
                    } else {
                        BTreeMap::from([("status".into(), json!(phase))])
                    },
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(format!("{subject}:{phase}")),
                })
                .unwrap();
        };
        // Two sends and multiple receipts arrive before one cursor read; no notification is used.
        append("message/one", "sent", &fence.subject);
        append("message/two", "sent", &fence.subject);
        append("message/one", "staged", "");
        append("message/one", "delivered", "");
        append("message/foreign", "sent", "agent/other");
        for phase in [None, Some("read"), Some("closed")] {
            if let Some(phase) = phase {
                append(
                    if phase == "closed" {
                        "message/two"
                    } else {
                        "message/one"
                    },
                    phase,
                    "",
                );
            }
            let (mark, view, updated, _) = update_snapshot(
                &store,
                &fence,
                read,
                previous.take(),
                (since, through),
                &mut admitted,
                &mut policies,
            )
            .unwrap();
            let mut oracle = snapshot(&store, &fence).unwrap();
            retain_live_mail(&store, &mut oracle.1, since, Some(through), &mut admitted).unwrap();
            assert_eq!(
                serde_json::to_value(&view).unwrap(),
                serde_json::to_value(&oracle).unwrap()
            );
            assert!(updated);
            previous = Some((mark, view));
        }
        // An idle cursor check performs no full read and produces no mailbox frame.
        for _ in 0..4 {
            let (mark, view, updated, _) = update_snapshot(
                &store,
                &fence,
                read,
                previous.take(),
                (since, through),
                &mut admitted,
                &mut policies,
            )
            .unwrap();
            assert!(!updated);
            previous = Some((mark, view));
        }
        // Unrelated graph traffic advances the cursor without reconstructing this mailbox.
        append("message/unrelated", "sent", "agent/other");
        let (mark, view, updated, _) = update_snapshot(
            &store,
            &fence,
            read,
            previous.take(),
            (since, through),
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(!updated);
        previous = Some((mark, view));
        // Seat observations retain the full incarnation fence but do not read message history.
        store
            .append_claim(&ClaimInput {
                subject: fence.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(fence.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), json!("ready")),
                    ("driver".into(), json!("omp")),
                    ("incarnation_id".into(), json!("session-1")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some("seat-refresh".into()),
            })
            .unwrap();
        let (_, view, updated, _) = update_snapshot(
            &store,
            &fence,
            read,
            previous,
            (since, through),
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(updated);
        assert!(
            view.1.is_empty(),
            "read and closed messages must both leave the mailbox"
        );
        assert_eq!(
            reads.get(),
            1,
            "only the initial snapshot reads the complete mailbox"
        );
    }

    #[test]
    fn held_watch_wake_resumes_after_a_local_post_guard_without_a_graph_write() {
        let store = Store::open_memory("node").unwrap();
        crate::mailbox::tests::ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let thread =
            crate::github_watch::ThreadRef::parse("fixture/incremental-mailbox#7").unwrap();
        let post = crate::github_watch::PostInFlight::begin(&fence.subject, &thread);
        store
            .append_claim(&ClaimInput {
                subject: "message/held-watch".into(),
                kind: "message.sent".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("daemon/runtime")),
                    ("to".into(), json!(fence.subject)),
                    ("content".into(), json!("A held watch fixture.")),
                    (
                        "tags".into(),
                        json!([
                            "github-watch",
                            "github-comment:fixture/incremental-mailbox:comment:23:7"
                        ]),
                    ),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let since = client_now_ms();
        let through = store.index().unwrap();
        let reads = std::cell::Cell::new(0);
        let read = |store: &Store, fence: &Fence| {
            reads.set(reads.get() + 1);
            raw_snapshot(store, fence)
        };
        let mut admitted = Default::default();
        let mut policies = Default::default();
        let (mark, view, _, _) = update_snapshot(
            &store,
            &fence,
            read,
            None,
            (since, through),
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(view.1.is_empty());
        assert!(policies.contains("message/held-watch"));
        let index = store.index().unwrap();
        drop(post);
        assert_eq!(store.index().unwrap(), index);
        let (mark, view, updated, _) = update_snapshot(
            &store,
            &fence,
            read,
            Some((mark, view)),
            (since, through),
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(updated);
        assert_eq!(view.1.len(), 1);
        let (_, view, updated, _) = update_snapshot(
            &store,
            &fence,
            read,
            Some((mark, view)),
            (since, through),
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(!updated);
        assert_eq!(view.1[0].subject, "message/held-watch");
        assert_eq!(
            reads.get(),
            1,
            "local policy changes recheck identities, never the full mailbox"
        );
    }

    #[tokio::test]
    async fn an_own_post_read_is_pure_and_background_closure_has_no_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open_memory("node").unwrap());
        let store = &state.store;
        crate::mailbox::tests::ready(store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let thread = crate::github_watch::ThreadRef::parse("fixture/mailbox-own-post#8").unwrap();
        store
            .record_github_post(
                &fence.subject,
                &thread,
                "comment",
                24,
                "https://example.test/fixture/comment/24",
                "fixture",
            )
            .unwrap();
        let mut wake = ClaimInput {
                subject: "message/own-post".into(),
                kind: "message.sent".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("daemon/runtime")),
                    ("to".into(), json!(fence.subject)),
                    ("content".into(), json!("An own post fixture.")),
                    (
                        "tags".into(),
                        json!([
                            "github-watch",
                            "github-comment:fixture/mailbox-own-post:comment:24:8"
                        ]),
                    ),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            };
        store.append_claim(&wake).unwrap();
        let floor = (client_now_ms(), store.index().unwrap());
        let mut admitted = Default::default();
        let mut policies = Default::default();
        let before = store.index().unwrap();
        let (_, view, _, closures) = update_snapshot(
            store,
            &fence,
            raw_snapshot,
            None,
            floor,
            &mut admitted,
            &mut policies,
        )
        .unwrap();
        assert!(view.1.is_empty());
        assert_eq!(store.index().unwrap(), before, "a mailbox read must not write");
        assert_eq!(store.message("message/own-post").unwrap().unwrap().status, "sent");
        assert_eq!(closures, std::collections::BTreeSet::from(["message/own-post".into()]));
        assert!(store.close_mailbox_own_post_wake(&fence, "message/own-post").unwrap());
        assert_eq!(store.message("message/own-post").unwrap().unwrap().status, "closed");
        assert_eq!(
            store
                .claims_for("message/own-post", Some("message.closed"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .claims_for("message/own-post", Some("message.read"))
                .unwrap()
                .is_empty()
        );
        // A queued identity cannot let a superseded channel close a successor's mailbox.
        wake.subject = "message/own-post-after-takeover".into();
        store.append_claim(&wake).unwrap();
        let successor = store.bind_mailbox(&Fence::new(
            &fence.subject, &fence.incarnation, "delivery",
        )).unwrap();
        let before = store.index().unwrap();
        assert_eq!(store.close_mailbox_own_post_wake(&fence, &wake.subject).unwrap_err().code,
            "stale-mailbox-session");
        assert_eq!(store.index().unwrap(), before);
        assert_eq!(store.message(&wake.subject).unwrap().unwrap().status, "sent");
        assert!(store.close_mailbox_own_post_wake(&successor, &wake.subject).unwrap());

        // The actual bounded actor must wake clients and replication after COMMIT,
        // without making a housekeeping closure schedule a reconcile pass.
        wake.subject = "message/own-post-actor".into();
        store.append_claim(&wake).unwrap();
        let wake_file = state.state_dir.join("replication.wake");
        let _ = std::fs::remove_file(&wake_file);
        let mut feed = state.event_notify.subscribe();
        let actor = own_post_reactor(&state, &successor);
        actor.try_send(wake.subject.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), feed.changed()).await.unwrap().unwrap();
        assert_eq!(store.message(&wake.subject).unwrap().unwrap().status, "closed");
        assert!(wake_file.exists(), "a canonical closure must wake replication dialers");
        assert!(tokio::time::timeout(Duration::from_millis(20), state.notify.notified()).await.is_err());
        drop(actor);

    }

    #[tokio::test]
    async fn two_subscribers_receive_coalesced_delivery_and_read_removal_without_resnapshot() {
        let root = tempfile::tempdir().unwrap();
        let mut state = super::super::tests::state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        crate::mailbox::tests::ready(&state.store, "session-1");
        let fence = state
            .store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let mut first =
            controlled_stream(state.clone(), &fence, &root.path().join("first.sock")).await;
        let mut second =
            controlled_stream(state.clone(), &fence, &root.path().join("second.sock")).await;
        state
            .store
            .append_claim(&ClaimInput {
                subject: "message/coalesced".into(),
                kind: "message.sent".into(),
                actor: Some("person/fixture".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/fixture")),
                    ("to".into(), json!(fence.subject)),
                    ("content".into(), json!("Coalesced fixture.")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let receipt = |phase: &str| ClaimInput {
            subject: "message/coalesced".into(),
            kind: format!("message.{phase}"),
            actor: Some(fence.subject.clone()),
            fields: BTreeMap::from([("status".into(), json!(phase))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("coalesced:{phase}")),
        };
        for phase in ["staged", "delivered"] {
            state
                .store
                .append_mailbox_receipt_outcome(&receipt(phase), &fence)
                .unwrap();
        }
        signal_changed(&state);
        for stream in [&mut first, &mut second] {
            assert!(
                matches!(next(&mut stream.socket).await, Frame::Mailbox { messages }
                if messages.len() == 1 && messages[0].status == "delivered")
            );
        }
        let (original, appended) = state
            .store
            .append_mailbox_receipt_outcome(&receipt("read"), &fence)
            .unwrap();
        assert!(appended);
        let (retry, appended) = state
            .store
            .append_mailbox_receipt_outcome(&receipt("read"), &fence)
            .unwrap();
        assert!(!appended);
        assert_eq!(original.id, retry.id);
        signal_changed(&state);
        for stream in [&mut first, &mut second] {
            assert!(
                matches!(next(&mut stream.socket).await, Frame::Mailbox { messages } if messages.is_empty())
            );
            assert_eq!(stream.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        assert_eq!(
            state
                .store
                .claims_for("message/coalesced", Some("message.read"))
                .unwrap()
                .len(),
            1
        );
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

#[cfg(test)]
#[path = "mailbox_profile.rs"]
mod profile;
