use super::*;
use std::collections::VecDeque;
use std::os::unix::fs::MetadataExt as _;
use std::sync::{LazyLock, Mutex, Weak};

const MAX_PREPARED: usize = 200;
const MAX_PREPARED_BYTES: usize = 64 * 1024 * 1024;
const PREPARED_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, PartialEq, Eq)]
struct NativeStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: std::time::SystemTime,
    changed: (i64, i64),
}

fn native_stamp(path: &std::path::Path) -> Option<NativeStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(NativeStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified: metadata.modified().ok()?,
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[derive(PartialEq, Eq)]
enum SourceVerdict {
    Claims,
    Readable { driver: String, anchor: Option<String>, path: std::path::PathBuf },
    Unavailable { driver: String, anchor: String, reason: String, not_yet: bool },
}

impl SourceVerdict {
    fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Readable { path, .. } => Some(path),
            Self::Claims | Self::Unavailable { .. } => None,
        }
    }
}

struct Prepared {
    store: Weak<Store>,
    authority: String,
    session_id: String,
    store_index: u64,
    local_position: u64,
    verdict: SourceVerdict,
    binding: Option<(String, Option<String>)>,
    stamp: Option<NativeStamp>,
    value: Arc<Value>,
    bytes: usize,
    created: std::time::Instant,
}

struct InitialRead {
    value: Arc<Value>,
    prepared: Option<Prepared>,
    seen: TranscriptSeen,
}

fn cache() -> &'static Mutex<VecDeque<Arc<Prepared>>> {
    static CACHE: LazyLock<Mutex<VecDeque<Arc<Prepared>>>> = LazyLock::new(Default::default);
    &CACHE
}

fn same_store(prepared: &Prepared, state: &AppState) -> bool {
    prepared.store.ptr_eq(&Arc::downgrade(&state.store))
}

fn current(prepared: &Prepared, state: &AppState) -> Result<bool, ApiError> {
    // Conservatively invalidate on every store/local commit. In particular, unrelated
    // message.sent claims can move session_messages' global 10,000-claim retention bound.
    Ok(prepared.created.elapsed() < PREPARED_TTL
        && prepared.store_index == state.store.index().map_err(ApiError::internal)?
        && prepared.local_position == local_latest_position(state)?
        && prepared.verdict.path().and_then(native_stamp) == prepared.stamp
        && source_verdict(state, prepared.binding.as_ref(), &prepared.session_id)? == prepared.verdict)
}

fn source_verdict(
    state: &AppState,
    binding: Option<&(String, Option<String>)>,
    session_id: &str,
) -> Result<SourceVerdict, ApiError> {
    let Some((owner, incarnation)) = binding else {
        return Ok(crate::external_sessions::find(state.native_session_home.as_deref(), session_id)
            .map_err(ApiError::internal)?.map_or(SourceVerdict::Claims, |external| SourceVerdict::Readable {
                driver: external.driver.as_str().to_owned(), anchor: None, path: external.transcript,
            }));
    };
    let Some(incarnation) = incarnation else { return Ok(SourceVerdict::Claims); };
    let Some(managed) = managed_transcript(state, owner, incarnation)? else {
        return Ok(SourceVerdict::Claims);
    };
    // Compare the exact source verdict, including a missing binding's reason. Absence is
    // never assumed: a native file or a different driver binding can appear without a claim.
    Ok(match managed.transcript {
        Ok(external) => SourceVerdict::Readable {
            driver: managed.driver, anchor: Some(managed.anchor.id), path: external.transcript,
        },
        Err(missing) => SourceVerdict::Unavailable {
            driver: managed.driver, anchor: managed.anchor.id,
            reason: missing.reason, not_yet: missing.not_yet,
        },
    })
}

pub(super) fn initial(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
) -> Result<Value, ApiError> {
    // Authentication belongs to the read, not to the preparation that happened earlier.
    require_scope(session, "read.projections")?;
    if let Some(prepared) = find_current(state, session, session_id)? {
        let mut value = (*prepared.value).clone();
        value["preparation"] = json!("ready");
        if let Some(cursor) = value["next_cursor"].as_str() {
            remember_cursor(cursor, prepared.stamp.as_ref().map(|stamp| (stamp.length, stamp.modified)));
        }
        return Ok(value);
    }

    // All claim and local-observation reads, including the cursor watermarks, use one
    // pinned SQLite snapshot. Native files have a separate before/after identity fence.
    let result = state.store.read_snapshot(|index| {
        Ok(build(state, session, session_id, index, false))
    }).map_err(ApiError::internal)??;
    let InitialRead { value, prepared, seen } = result
        .ok_or_else(|| ApiError::internal("a foreground conversation read returned no page"))?;
    if let Some(cursor) = value["next_cursor"].as_str() {
        remember_cursor(cursor, seen);
    }
    if let Some(prepared) = prepared { publish(prepared); }
    let mut value = Arc::unwrap_or_clone(value);
    value["preparation"] = json!("miss");
    Ok(value)
}

fn publish(prepared: Prepared) {
    let mut entries = cache().lock().expect("prepared conversation cache poisoned");
    entries.retain(|entry| entry.store.strong_count() > 0
        && entry.created.elapsed() < PREPARED_TTL
        && !(entry.store.ptr_eq(&prepared.store) && entry.authority == prepared.authority
            && entry.session_id == prepared.session_id));
    let mut bytes = entries.iter().map(|entry| entry.bytes).sum::<usize>();
    while entries.len() >= MAX_PREPARED || bytes.saturating_add(prepared.bytes) > MAX_PREPARED_BYTES {
        let Some(evicted) = entries.pop_front() else { break; };
        bytes = bytes.saturating_sub(evicted.bytes);
    }
    entries.push_back(Arc::new(prepared));
}

fn find_current(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
) -> Result<Option<Arc<Prepared>>, ApiError> {
    let prepared = cache().lock().expect("prepared conversation cache poisoned")
        .iter().find(|prepared| same_store(prepared, state)
            && prepared.authority == session.authority_actor
            && prepared.session_id == session_id).cloned();
    match prepared {
        Some(prepared) if current(&prepared, state)? => Ok(Some(prepared)),
        _ => Ok(None),
    }
}

pub(super) fn warm(state: &AppState, session: &ClientSession, session_id: &str) -> Result<bool, ApiError> {
    require_scope(session, "read.projections")?;
    if find_current(state, session, session_id)?.is_some() { return Ok(true); }
    // Unmanaged native discovery is not cached. Missing managed sources retain their exact
    // authoritative notice and are re-resolved before every hit.
    let result = state.store.read_snapshot(|index| {
        Ok(build(state, session, session_id, index, true))
    }).map_err(ApiError::internal)??;
    let Some(InitialRead { prepared, .. }) = result else { return Ok(false); };
    if let Some(prepared) = prepared { publish(prepared); }
    Ok(find_current(state, session, session_id)?.is_some())
}

fn build(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
    index: u64,
    preparation_only: bool,
) -> Result<Option<InitialRead>, ApiError> {
    let managed = super::super::managed_session_owner_at(&state.store, index, session_id)
        .map_err(ApiError::internal)?;
    let cacheable = managed.is_some();
    if preparation_only && !cacheable { return Ok(None); }
    let binding = managed.map(|(owner, incarnation, _)| (owner, incarnation));
    let verdict = source_verdict(state, binding.as_ref(), session_id)?;
    let stamp = verdict.path().and_then(native_stamp);
    let local_position = local_latest_position(state)?;
    let value = Arc::new(conversation_read_now(state, session, session_id, None)?);
    if verdict.path().and_then(native_stamp) != stamp
        || source_verdict(state, binding.as_ref(), session_id)? != verdict
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "stale-fence".into(),
            message: "the native conversation changed while its initial page was read".into(),
            details: Box::default(),
        });
    }
    let bytes = frame_bytes(&value);
    let seen = stamp.as_ref().map(|stamp| (stamp.length, stamp.modified));
    // A bound path that failed during the actual native read is not a stable source verdict.
    let read_failed = matches!(verdict, SourceVerdict::Readable { .. })
        && value["initial_page"]["items"].as_array().is_some_and(|items| {
            items.iter().any(|entry| entry["body"]["code"] == "transcript-not-bound")
        });
    let prepared = (cacheable && !read_failed && bytes <= CLIENT_MAX_RESPONSE_BYTES).then(|| Prepared {
        store: Arc::downgrade(&state.store),
        authority: session.authority_actor.clone(),
        session_id: session_id.to_owned(),
        store_index: index,
        local_position,
        verdict,
        binding,
        stamp,
        value: value.clone(),
        bytes,
        created: std::time::Instant::now(),
    });
    Ok(Some(InitialRead { value, prepared, seen }))
}

pub(super) async fn slot() -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    static SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
        LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));
    SLOTS.clone().acquire_owned().await.map_err(ApiError::internal)
}

pub(super) async fn prepare(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
) -> Result<bool, ApiError> {
    require_scope(session, "read.projections")?;
    let permit = slot().await?;
    let (state, session, session_id) = (state.clone(), session.clone(), session_id.to_owned());
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        warm(&state, &session, &session_id)
    }).await.map_err(ApiError::internal)?
}

/// Preparation belongs to a held agents window, never to hover or a guessed selection.
/// Closing the socket cancels this owner; in-flight blocking reads retain their slot.
pub(super) struct Owner {
    targets: watch::Sender<Vec<String>>,
    task: tokio::task::JoinHandle<()>,
}

impl Owner {
    pub(super) fn new(state: AppState, session: ClientSession) -> Self {
        let (targets, mut wanted) = watch::channel(Vec::<String>::new());
        let task = tokio::spawn(async move {
            let mut changed = state.event_notify.subscribe();
            let mut native_tick = tokio::time::interval(Duration::from_millis(500));
            native_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    result = wanted.changed() => if result.is_err() { return; },
                    result = changed.changed() => if result.is_err() { return; },
                    _ = native_tick.tick() => {},
                }
                let sessions = wanted.borrow_and_update().clone();
                for session_id in sessions {
                    if !wanted.borrow().contains(&session_id) { continue; }
                    let remote = match conversation_owner_host(&state, &session, &session_id) {
                        Ok(remote) => remote,
                        Err(_) => continue,
                    };
                    let _ = prepare_conversation_value(&state, &session, &session_id, remote.as_deref()).await;
                }
            }
        });
        Self { targets, task }
    }

    pub(super) fn update(&self, subscriptions: &BTreeMap<String, CollectionSubscription>) {
        let sessions = subscriptions.values()
            .filter(|subscription| subscription.request.collection == "agents")
            .flat_map(|subscription| subscription.previous.values())
            .filter_map(|agent| agent["current_session_id"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>().into_iter().take(MAX_PREPARED).collect::<Vec<_>>();
        self.targets.send_if_modified(|current| {
            if *current == sessions { false } else { *current = sessions; true }
        });
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.task.abort();
    }
}
