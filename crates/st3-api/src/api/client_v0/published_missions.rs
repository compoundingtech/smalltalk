//! HTTP mission pages share the same complete cards as the background-published windows.
use super::*;
use crate::store::mission_list::MissionRows;
use crate::store::published_list::{ListRead, Publication, PublicationId};

pub const NOT_READY: &str = "mission-list-not-ready";
pub const ENDED: &str = "mission-list-ended";

type MissionPublication = (Arc<Publication<MissionRows>>, PublicationId);

pub fn not_ready() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: NOT_READY.into(),
        message: "the mission list is still being prepared; retry shortly".into(),
        details: Box::default(),
    }
}

pub fn current(store: &Store) -> Result<Option<MissionPublication>, ApiError> {
    let list = store.published_missions_list();
    match list.current() {
        ListRead::Served(publication, id) => Ok(Some((publication, id))),
        ListRead::NeverStarted if !store.collection_view_published("missions") => Ok(None),
        ListRead::NeverStarted | ListRead::NotReady => {
            list.request_refresh();
            Err(not_ready())
        }
        ListRead::Ended => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: ENDED.into(),
            message: "the mission list refresher stopped; restart the daemon".into(),
            details: Box::default(),
        }),
    }
}

struct KeptPage {
    id: PublicationId,
    contract: String,
    digest: String,
    publication: Arc<Publication<MissionRows>>,
    expires_at_unix_ms: u128,
}

static PAGES: std::sync::LazyLock<Mutex<VecDeque<KeptPage>>> =
    std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));

fn contract(snapshot: &ClientSnapshot, query: &ClientListQuery, limit: usize) -> String {
    serde_json::to_string(&(snapshot, limit, query.history, &query.person, &query.actor,
        &query.owner_run, &query.status, query.native_only)).expect("a page contract serializes")
}

pub fn page(
    state: &AppState,
    requested_snapshot: ClientSnapshot,
    query: &ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    // Check the lifecycle even for a kept continuation. A stopped refresher never serves rows.
    let (newest, newest_id) = current(&state.store)?.ok_or_else(not_ready)?;
    let list = state.store.published_missions_list();
    let generation = list.generation();
    let now = client_now_ms();
    let mut limit = query.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS).clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let cursor = query.cursor.as_deref().map(decode_client_cursor).transpose()?;
    let (publication, id, snapshot, offset, expires, digest, page_contract) = if let Some(cursor) = cursor {
        if cursor.collection != "missions" || cursor.history || cursor.limit == 0
            || cursor.limit > CLIENT_MAX_PAGE_ITEMS
            || query.limit.is_some_and(|_| limit != cursor.limit)
            || serde_json::to_string(&cursor.snapshot).expect("a snapshot serializes")
                != serde_json::to_string(&requested_snapshot).expect("a snapshot serializes")
            || cursor.expires_at_unix_ms <= now
            || cursor.person != query.person || cursor.actor != query.actor
            || cursor.owner_run != query.owner_run || cursor.status != query.status
            || cursor.native_only != query.native_only
            || cursor.owner.is_some() || cursor.state.is_some()
            || cursor.after_key.is_some() || cursor.before_index.is_some()
        {
            return Err(client_page_expired("the mission page cursor does not match this snapshot or filter"));
        }
        limit = cursor.limit;
        let page_contract = contract(&cursor.snapshot, query, limit);
        let mut pages = PAGES.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pages.retain(|page| page.expires_at_unix_ms > now
            && (page.id.list != list.instance() || page.id.generation == generation));
        let kept = pages.iter().find(|page| page.id.list == list.instance()
            && page.digest == cursor.items_digest && page.contract == page_contract
            && page.expires_at_unix_ms == cursor.expires_at_unix_ms)
            .ok_or_else(|| client_page_expired("the mission publication expired; restart pagination"))?;
        (Arc::clone(&kept.publication), kept.id, cursor.snapshot, cursor.offset,
            kept.expires_at_unix_ms, kept.digest.clone(), page_contract)
    } else {
        let (publication, acquired_id) = (newest, newest_id);
        let mut snapshot = client_snapshot_with_time(state, publication.cut, publication.rows.time_unix_ms);
        snapshot.published_at = Some(client_timestamp(publication.published_at_unix_ms));
        let digest = format!("published-missions:{}:{}:{}", acquired_id.list, acquired_id.generation, acquired_id.sequence);
        let page_contract = contract(&snapshot, query, limit);
        (publication, acquired_id, snapshot, 0, now.saturating_add(CLIENT_PAGE_TTL_MS), digest, page_contract)
    };
    let rows = &publication.rows;
    if offset > rows.order.len() {
        return Err(client_page_expired("the mission page offset is outside its publication"));
    }
    // Only the bounded response is cloned. All attention, definitions, run overviews, step
    // previews and faults were folded into these cards before publication.
    let mut items = rows.order.iter().skip(offset).take(limit)
        .map(|id| rows.cards.get(id).map(|card| (**card).clone())
            .ok_or_else(|| ApiError::internal("a published mission is missing its card")))
        .collect::<Result<Vec<_>, _>>()?;
    let truncated = bound_mission_cards(&mut items).map_err(ApiError::internal)?;
    let end = offset.saturating_add(items.len());
    let has_more = truncated || end < rows.order.len();
    if list.generation() != id.generation {
        return Err(not_ready());
    }
    let mut expires = expires;
    let next_cursor = if has_more {
        let mut pages = PAGES.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pages.retain(|page| page.expires_at_unix_ms > now
            && (page.id.list != list.instance() || page.id.generation == list.generation()));
        if let Some(kept) = pages.iter().find(|page| page.id == id && page.contract == page_contract) {
            expires = kept.expires_at_unix_ms;
        } else {
            while pages.len() >= CLIENT_PAGE_CACHE_CAPACITY { pages.pop_front(); }
            pages.push_back(KeptPage { id, contract: page_contract, digest: digest.clone(),
                publication: Arc::clone(&publication), expires_at_unix_ms: expires });
        }
        Some(encode_client_cursor(&ClientPageCursor {
            snapshot: snapshot.clone(), collection: "missions".into(), offset: end, limit,
            history: false, person: query.person.clone(), actor: query.actor.clone(),
            owner_run: query.owner_run.clone(), status: query.status.clone(), owner: None, state: None,
            native_only: query.native_only, items_digest: digest, before_index: None,
            after_key: None, expires_at_unix_ms: expires,
        })?)
    } else { None };
    Ok((Extension(snapshot), Json(ClientResourcePage {
        kind: "page".into(), collection: "missions".into(), filters: BTreeMap::new(), items,
        page: ClientPageInfo { limit, has_more, next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires)) },
        sync: client_sync_notice(state), replicated: None,
    })))
}

pub fn register_metrics(store: &Arc<Store>) {
    if !crate::otel::metrics_enabled() { return; }
    let store = Arc::downgrade(store);
    opentelemetry::global::meter("st3.missions")
        .f64_observable_gauge("st.mission_list.publication.age")
        .with_unit("s")
        .with_description("Age of the newest complete current mission publication; absent before publication")
        .with_callback(move |observer| {
            if let Some(store) = store.upgrade()
                && let Some(publication) = store.published_missions_list().newest()
            {
                let age_ms = u64::try_from(client_now_ms().saturating_sub(publication.published_at_unix_ms))
                    .unwrap_or(u64::MAX);
                observer.observe(Duration::from_millis(age_ms).as_secs_f64(), &[]);
            }
        }).build();
}

pub fn record_rebuild(elapsed: Duration) {
    if !crate::otel::metrics_enabled() { return; }
    static DURATION: std::sync::LazyLock<opentelemetry::metrics::Histogram<f64>> = std::sync::LazyLock::new(||
        opentelemetry::global::meter("st3.missions")
            .f64_histogram("st.mission_list.rebuild.duration")
            .with_unit("s")
            .with_description("Duration of current mission list rebuild attempts, including failed attempts")
            .build());
    DURATION.record(elapsed.as_secs_f64(), &[]);
}
