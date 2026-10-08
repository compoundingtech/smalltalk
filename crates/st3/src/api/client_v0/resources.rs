use super::*;

#[derive(Clone, Debug, Default, Deserialize)]
pub(in crate::api) struct ResourcesQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    opened_by: Option<String>,
    kind: Option<String>,
    subject_prefix: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ResourcesCursor {
    snapshot: ClientSnapshot,
    resources_version: String,
    opened_by: Option<String>,
    kind: Option<String>,
    subject_prefix: Option<String>,
    after_subject: String,
    limit: usize,
    expires_at_unix_ms: u128,
}

fn decode_cursor(encoded: &str) -> Result<ResourcesCursor, ApiError> {
    let malformed = || validation("the resources page cursor is malformed");
    let encoded = encoded
        .strip_prefix("resources-page/")
        .ok_or_else(malformed)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| malformed())?;
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

pub(in crate::api) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ResourcesQuery>,
) -> Result<ClientPageResponse, ApiError> {
    require_scope(&session, "read.projections")?;
    if query.opened_by.as_deref().is_some_and(|subject| {
        !(subject.starts_with("agent/") && subject.len() > "agent/".len()
            || subject.starts_with("mission-run/") && subject.len() > "mission-run/".len())
    }) {
        return Err(validation(
            "opened_by must name an agent or mission-run subject",
        ));
    }
    let requested_limit = query
        .limit
        .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
        .clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let cursor = query.cursor.as_deref().map(decode_cursor).transpose()?;
    if let Some(cursor) = &cursor
        && (cursor.opened_by != query.opened_by
            || cursor.kind != query.kind
            || cursor.subject_prefix != query.subject_prefix
            || !(1..=CLIENT_MAX_PAGE_ITEMS).contains(&cursor.limit)
            || query.limit.is_some_and(|_| requested_limit != cursor.limit)
            || client_now_ms() > cursor.expires_at_unix_ms)
    {
        return Err(client_page_expired(
            "the resources page cursor expired or does not match its filters",
        ));
    }
    let limit = cursor
        .as_ref()
        .map_or(requested_limit, |cursor| cursor.limit);
    let expires_at_unix_ms = cursor.as_ref().map_or_else(
        || client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
        |cursor| cursor.expires_at_unix_ms,
    );
    let reader = state.clone();
    let filters = query.clone();
    let (snapshot, resources_version, mut items) = super::super::blocking_store(move || {
        let store = reader.store.clone();
        store.read_snapshot(|index| {
            let current = client_snapshot_at(&reader, index);
            let resources_version = store.resource_collection_version()?;
            // Unrelated commits leave this projection unchanged. Resource changes expire the
            // sequence rather than mixing old and new observations across pages.
            if cursor.as_ref().is_some_and(|cursor| {
                cursor.resources_version != resources_version
                    || cursor.snapshot.projection_version != current.projection_version
                    || cursor.snapshot.host_id != current.host_id
            }) {
                return Ok(None);
            }
            let snapshot = cursor
                .as_ref()
                .map_or(current, |cursor| cursor.snapshot.clone());
            let items = store.resource_collection_page(
                filters.opened_by.as_deref(),
                filters.kind.as_deref(),
                filters.subject_prefix.as_deref(),
                cursor.as_ref().map(|cursor| cursor.after_subject.as_str()),
                limit.saturating_add(1),
            )?;
            Ok(Some((snapshot, resources_version, items)))
        })
    })
    .await?
    .ok_or_else(|| client_page_expired("the resource snapshot changed; restart pagination"))?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = if has_more {
        let cursor = ResourcesCursor {
            snapshot: snapshot.clone(),
            resources_version,
            opened_by: query.opened_by.clone(),
            kind: query.kind.clone(),
            subject_prefix: query.subject_prefix.clone(),
            after_subject: items
                .last()
                .and_then(|item| item["id"].as_str())
                .expect("a nonempty resource page")
                .to_owned(),
            limit,
            expires_at_unix_ms,
        };
        Some(format!(
            "resources-page/{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&cursor).map_err(ApiError::internal)?)
        ))
    } else {
        None
    };
    let filters = [
        ("opened_by", query.opened_by),
        ("kind", query.kind),
        ("subject_prefix", query.subject_prefix),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
    .collect();
    let page = ClientResourcePage {
        kind: "page".into(),
        collection: "resources".into(),
        filters,
        items,
        page: ClientPageInfo {
            limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)),
        },
        sync: client_sync_notice(&state),
        replicated: None,
        history: None,
    };
    Ok((Extension(snapshot), Json(page)))
}
