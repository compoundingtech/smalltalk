//! Current runtime pages never reduce status: the shared list refresher publishes them.
use super::*;
use crate::store::published_list::{ListRead, Publication};
use crate::store::runtime_list::RuntimeRows;

pub(crate) fn register_metrics(store: &Arc<Store>) {
    if !crate::otel::metrics_enabled() {
        return;
    }
    let store = Arc::downgrade(store);
    opentelemetry::global::meter("st3.runtimes")
        .f64_observable_gauge("st.runtime_list.publication.age")
        .with_unit("s")
        .with_description("Age of the newest complete current runtime publication; absent before publication")
        .with_callback(move |observer| {
            if let Some(store) = store.upgrade()
                && let Some(publication) = store.published_runtime_list().newest()
            {
                observer.observe(client_now_ms().saturating_sub(publication.published_at_unix_ms) as f64 / 1000.0, &[]);
            }
        })
        .build();
}

pub(crate) fn fold(
    store: &Store,
    base: Option<&Publication<RuntimeRows>>,
) -> anyhow::Result<Option<(Publication<RuntimeRows>, bool)>> {
    let started = Instant::now();
    let result = store.read_snapshot(|index| {
        if base.is_some_and(|base| base.cut == index) {
            return Ok(None);
        }
        let time_unix_ms = store.projection_time_at(index)?;
        let status = store.status_for_claim_kind_at(
            "runtime.observed", Some(index), false, crate::store::ClaimsReduction::Runtime,
        )?;
        let items = runtime_resources_from_status_access(store, "runtime-publication", &client_timestamp(time_unix_ms), status, true, true)?;
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&items)?));
        Ok(Some((Publication {
            cut: index,
            published_at_unix_ms: client_now_ms(),
            valid_until_unix_ms: None,
            rows: RuntimeRows { items: Arc::new(items), digest, time_unix_ms },
        }, true)))
    });
    if crate::otel::metrics_enabled() && !matches!(&result, Ok(None)) {
        static DURATION: std::sync::LazyLock<opentelemetry::metrics::Histogram<f64>> = std::sync::LazyLock::new(||
            opentelemetry::global::meter("st3.runtimes")
                .f64_histogram("st.runtime_list.rebuild.duration")
                .with_unit("s")
                .with_description("Duration of current runtime list rebuild attempts, including failed attempts")
                .build());
        DURATION.record(started.elapsed().as_secs_f64(), &[]);
    }
    result
}

fn not_ready() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "runtime-list-not-ready".into(),
        message: "the runtime list is still being prepared; retry shortly".into(),
        details: Box::default(),
    }
}

pub(super) fn page(
    state: &AppState,
    snapshot: ClientSnapshot,
    session: &ClientSession,
    query: &ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    // The existing TTL cache owns the first publication's Arc, so a continuation remains
    // frozen across republishes without copying or hashing the full collection on a read.
    let (snapshot, mut page) = if query.cursor.is_some() {
        let page = client_page_read(state, &snapshot, "runtimes", Vec::new(), query, true)?;
        (snapshot, page)
    } else {
        let list = state.store.published_runtime_list();
        let publication = match list.current() {
            ListRead::Served(publication, _) => publication,
            ListRead::Ended => return Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "runtime-list-ended".into(),
                message: "the runtime list refresher stopped; restart the daemon".into(),
                details: Box::default(),
            }),
            ListRead::NeverStarted | ListRead::NotReady => {
                list.request_refresh();
                return Err(not_ready());
            }
        };
        let mut snapshot = client_snapshot_with_time(state, publication.cut, publication.rows.time_unix_ms);
        snapshot.published_at = Some(client_timestamp(publication.published_at_unix_ms));
        let limit = query.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS).clamp(1, CLIENT_MAX_PAGE_ITEMS);
        let items = &publication.rows.items;
        let end = limit.min(items.len());
        let has_more = end < items.len();
        let mut expires_at_unix_ms = client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS);
        let next_cursor = if has_more {
            let mut cache = client_page_cache().lock().expect("client page cache poisoned");
            cache.retain(|entry| entry.expires_at_unix_ms > client_now_ms());
            if let Some(cached) = cache.iter().find(|entry| {
                entry.snapshot_id == snapshot.id && entry.collection == "runtimes"
                    && entry.items_digest == publication.rows.digest
            }) {
                expires_at_unix_ms = cached.expires_at_unix_ms;
            } else {
                while cache.len() >= CLIENT_PAGE_CACHE_CAPACITY {
                    cache.pop_front();
                }
                cache.push_back(CachedClientPage {
                    snapshot_id: snapshot.id.clone(),
                    collection: "runtimes".into(),
                    items_digest: publication.rows.digest.clone(),
                    items: Arc::clone(items),
                    expires_at_unix_ms,
                });
            }
            Some(encode_client_cursor(&ClientPageCursor {
                snapshot: snapshot.clone(), collection: "runtimes".into(), offset: end, limit,
                history: false, person: query.person.clone(), actor: query.actor.clone(),
                owner_run: query.owner_run.clone(), status: query.status.clone(),
                owner: None, state: None, native_only: query.native_only,
                items_digest: publication.rows.digest.clone(), before_index: None,
                after_key: None, expires_at_unix_ms,
            })?)
        } else { None };
        let page = ClientResourcePage {
            kind: "page".into(), collection: "runtimes".into(),
            filters: client_page_filters("runtimes", query),
            items: items[..end].to_vec(),
            page: ClientPageInfo { limit, has_more, next_cursor,
                cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)) },
            sync: client_sync_notice(state), replicated: None,
        };
        (snapshot, page)
    };
    // Publications record capability eligibility, never a particular caller's authority.
    // Restrict only the cloned page, leaving the shared publication and cache untouched.
    for item in &mut page.items {
        if let Some(access) = item["terminal_access"].as_object_mut() {
            for (name, allowed) in [("read", session.allows("terminal.read")),
                ("input", session.allows("terminal.control")), ("resize", session.allows("terminal.control"))] {
                if !allowed {
                    access.insert(name.into(), json!("unavailable"));
                }
            }
        }
    }
    Ok((Extension(snapshot), Json(page)))
}
