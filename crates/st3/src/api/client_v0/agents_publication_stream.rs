//! Complete-roster transport adapter. Legacy window subscriptions keep their own pacing.
use super::*;
use super::agents_publication_frames::{self as frames, Delivered};
use crate::store::agents_publication::AgentsPublication;

#[derive(Default)]
pub(super) struct Subscription {
    // A shared ID/fingerprint summary, never the delivered full Value roster.
    pub(super) delivered: Option<Delivered>,
}

impl Subscription {
    pub(super) fn is_current(&self, current: Option<&AgentsPublication>) -> bool {
        self.delivered.as_ref().zip(current).is_some_and(|(base, current)| {
            base.epoch() == current.metadata().node_epoch && base.revision() == current.metadata().revision
        })
    }
}

pub(super) enum Delivery {
    Current,
    Frames(Box<frames::Prepared>),
}

pub(super) fn schedule_notice(due: &mut Option<tokio::time::Instant>) {
    due.get_or_insert(tokio::time::Instant::now() + Duration::from_millis(50));
}

pub(super) fn validate(request: &CollectionSubscribe) -> Result<(), ApiError> {
    if let Some(version) = request.agents_publication_version
        && (version != 1 || request.collection != "agents" || request.limit.is_some()
            || request.person.is_some() || request.actor.is_some() || request.status.is_some()
            || request.subject.is_some() || request.terminal.is_some()
            || request.incarnation.is_some() || request.capability.is_some()
            || request.conversation.is_some())
    {
        return Err(validation("revisioned agents subscriptions require version 1 and an unfiltered complete roster"));
    }
    Ok(())
}

async fn authorize(state: &AppState, session: &ClientSession) -> Result<(), ApiError> {
    if session.transport == "unix" {
        return require_scope(session, "read.projections");
    }
    let (state, session) = (state.clone(), session.clone());
    tokio::task::spawn_blocking(move || {
        let current = revalidate_session(&state, &session)?;
        require_scope(&current, "read.projections")
    }).await.map_err(ApiError::internal)?
}

pub(super) async fn prepare(
    state: AppState,
    session: ClientSession,
    request: CollectionSubscribe,
    previous: Option<Delivered>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Delivery, ApiError> {
    authorize(&state, &session).await?;
    let publication = match state.store.agents_publication() {
        Some(publication) => publication,
        None => {
            let mut publications = state.store.subscribe_agents_publications();
            tokio::time::timeout(AGENT_ROSTER_READ_WAIT, async {
                loop {
                    if let Some(publication) = publications.borrow_and_update().clone() {
                        return Ok(publication);
                    }
                    publications.changed().await.map_err(ApiError::internal)?;
                }
            }).await.map_err(|_| ApiError::bad(St3Error::new("agent-roster-not-ready", "the complete agents roster is still being prepared")))??
        }
    };
    if previous.as_ref().is_some_and(|base| base.epoch() == publication.metadata().node_epoch
        && base.revision() == publication.metadata().revision)
    {
        return Ok(Delivery::Current);
    }
    #[cfg(test)]
    hold_preparation_for_test(&publication).await;
    // Snapshot provenance is a pure function of the captured immutable metadata, not a
    // per-subscriber Store read or a moving current clock.
    let metadata = publication.metadata();
    let snapshot = ClientSnapshot {
        published_at: Some(client_timestamp(u128::from(metadata.materialized_at_ms))),
        ..client_snapshot_with_time(&state, metadata.status_watermark.store_index,
            u128::from(metadata.materialized_at_ms))
    };
    tokio::task::spawn_blocking(move || {
        // Caller cancellation must not release this physical preparation slot early.
        let _permit = permit;
        frames::prepare(publication, snapshot, &request.id, previous.as_ref(), CLIENT_MAX_RESPONSE_BYTES)
            .map(|prepared| Delivery::Frames(Box::new(prepared)))
            .map_err(|_| ApiError::bad(St3Error::new("agents-publication-row-too-large",
                "a complete agents row cannot fit in the frame budget")))
    }).await.map_err(ApiError::internal)?
}

async fn refusal(socket: &mut WebSocket, id: &str, error: ApiError) -> Refreshed {
    let retryable = client_error_retryable(error.status, Some(&error.code));
    if !send_collection(socket, json!({"kind":if retryable {"resync"} else {"error"},
        "id":id,"collection":"agents","code":error.code,"message":error.message,
        "retryable":retryable})).await
    { return Refreshed::Closed; }
    if retryable { Refreshed::Retry } else { Refreshed::Dropped }
}

pub(super) async fn deliver(
    socket: &mut WebSocket,
    state: &AppState,
    session: &ClientSession,
    subscription: &mut CollectionSubscription,
    delivery: Result<Delivery, ApiError>,
) -> Refreshed {
    let mut prepared = match delivery {
        Ok(Delivery::Current) => return Refreshed::Current,
        Ok(Delivery::Frames(prepared)) => prepared,
        Err(error) => return refusal(socket, &subscription.request.id, error).await,
    };
    // One bounded contiguous sequence owns the socket. No other data frames interleave.
    // This also bounds authorization/encoding time, not just each individual socket send.
    let result = tokio::time::timeout(COLLECTION_SEND_TIMEOUT, async {
        for index in 0..prepared.frame_count() {
            if let Err(error) = authorize(state, session).await {
                return refusal(socket, &subscription.request.id, error).await;
            }
            let id = subscription.request.id.clone();
            let encoded = tokio::task::spawn_blocking(move || {
                let frame = prepared.encode_frame(index, &id);
                (prepared, frame)
            }).await;
            let Ok((returned, frame)) = encoded else { return Refreshed::Closed; };
            prepared = returned;
            let Ok(payload) = frame else { return Refreshed::Closed; };
            if !send_collection_message(socket, WsMessage::Text(payload.into())).await {
                return Refreshed::Closed;
            }
        }
        let held = subscription.revisioned.as_mut().expect("revisioned delivery owns a revisioned subscription");
        held.delivered = Some(prepared.delivered());
        subscription.delivered = true;
        if let Some(opened) = subscription.opened.take() {
            super::super::record_stream_latency("agents", opened.elapsed(), false);
        }
        Refreshed::Current
    }).await;
    result.unwrap_or(Refreshed::Closed)
}

#[cfg(test)]
static PREPARATION_GATES: std::sync::LazyLock<parking_lot::Mutex<BTreeMap<uuid::Uuid, PreparationGate>>> =
    std::sync::LazyLock::new(Default::default);

#[cfg(test)]
#[derive(Clone)]
struct PreparationGate {
    revision: u64,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
pub(super) struct HeldPreparation {
    epoch: uuid::Uuid,
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
impl HeldPreparation {
    pub(super) fn new(publication: &AgentsPublication) -> Self {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let epoch = publication.metadata().node_epoch;
        PREPARATION_GATES.lock().insert(epoch, PreparationGate {
            revision: publication.metadata().revision, entered: entered.clone(), release: release.clone(),
        });
        Self { epoch, entered, release }
    }
}

#[cfg(test)]
impl Drop for HeldPreparation {
    fn drop(&mut self) { PREPARATION_GATES.lock().remove(&self.epoch); }
}

#[cfg(test)]
async fn hold_preparation_for_test(publication: &AgentsPublication) {
    let gate = PREPARATION_GATES.lock().get(&publication.metadata().node_epoch).cloned();
    if let Some(gate) = gate && gate.revision == publication.metadata().revision {
        gate.entered.notify_one();
        gate.release.notified().await;
    }
}
