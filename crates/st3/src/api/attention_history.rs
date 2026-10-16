//! Bounded retained source history, separate from the open attention publication.
use super::*;
use crate::store::attention_history::{MAX_CANDIDATES, Resolution};

pub(super) const CURSOR_PREFIX: &str = "page/attention-history/";

#[derive(Serialize, Deserialize)]
struct Cursor {
    position: ClientPageCursor,
    mac: String,
}

fn mac(position: &ClientPageCursor) -> Result<hmac::Hmac<Sha256>, ApiError> {
    use hmac::Mac as _;
    static KEY: std::sync::LazyLock<[u8; 32]> = std::sync::LazyLock::new(|| {
        let mut key = [0; 32];
        getrandom::fill(&mut key).expect("creating attention history cursor key");
        key
    });
    let mut mac =
        <hmac::Hmac<Sha256> as hmac::Mac>::new_from_slice(&*KEY).expect("32-byte HMAC key");
    mac.update(&serde_json::to_vec(position).map_err(ApiError::internal)?);
    Ok(mac)
}

fn encode(position: ClientPageCursor) -> Result<String, ApiError> {
    use hmac::Mac as _;
    let signature = hex::encode(mac(&position)?.finalize().into_bytes());
    let bytes = serde_json::to_vec(&Cursor {
        position,
        mac: signature,
    })
    .map_err(ApiError::internal)?;
    Ok(format!(
        "{CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn decode(encoded: &str) -> Result<ClientPageCursor, ApiError> {
    use hmac::Mac as _;
    let expired =
        || client_page_expired("attention history cursor is unavailable; restart pagination");
    if encoded.len() > 16_384 {
        return Err(expired());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.strip_prefix(CURSOR_PREFIX).ok_or_else(expired)?)
        .map_err(|_| expired())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| expired())?;
    mac(&cursor.position)?
        .verify_slice(&hex::decode(cursor.mac).map_err(|_| expired())?)
        .map_err(|_| expired())?;
    Ok(cursor.position)
}

pub(super) fn cursor_snapshot(encoded: &str) -> Result<ClientSnapshot, ApiError> {
    decode(encoded).map(|position| position.snapshot)
}

pub(super) fn closed_claim_id(id: &str) -> Option<&str> {
    id.strip_prefix("attention/closed/")
        .or_else(|| id.strip_prefix("closed/"))
}

fn ensure_source(row: &Resolution) -> anyhow::Result<()> {
    let fields = &row.claim.body["fields"];
    let episode = if row.claim.kind == "work.person-cancelled" {
        fields["episode"].as_str()
    } else if fields["code"] == "native-prompt-answered" && fields["status"] == "dismiss" {
        fields["delegation"]["episode"].as_str()
    } else {
        None
    };
    if row.original.is_none()
        && episode.is_some_and(|episode| row.claim.predecessors.iter().any(|id| id == episode))
    {
        return Err(anyhow::Error::new(St3Error::new(
            "attention-history-not-ready",
            "the retained resolution's original source is unavailable at this cut; refresh history after replication catches up",
        )));
    }
    Ok(())
}

fn history_error(mut error: ApiError) -> ApiError {
    if error.code == "attention-history-not-ready" {
        error.status = StatusCode::SERVICE_UNAVAILABLE;
    }
    error
}

/// Authorization is based on the original recipient (update) or the recorded
/// represented person (denial), not the seat's possibly changed current owner.
fn resource(row: Resolution, person: Option<&str>) -> Option<Value> {
    let original = row.original?;
    let fields = &row.claim.body["fields"];
    let source = &original.body["fields"];
    let actor = row.claim.actor.as_deref()?;
    let (owner, kind, title, detail, alert) = if row.claim.kind == "work.person-cancelled"
        && original.kind == "work.person-asked"
        && source["request"]["type"] == "update"
        && original.subject == row.claim.subject
        && original.actor.as_deref() == Some(actor)
        && fields["episode"] == original.id
    {
        let owner = source["person"].as_str()?;
        (
            owner,
            "person-step",
            source["title"].as_str().unwrap_or("Update").to_owned(),
            format!(
                "{}\n\nWithdrawn by the sender {actor}.\nReason: {}\nEpisode: {}",
                source["reason"].as_str().unwrap_or_default(),
                fields["summary"].as_str().unwrap_or_default(),
                original.id
            ),
            false,
        )
    } else if row.claim.kind == "harness.diagnostic"
        && fields["code"] == "native-prompt-answered"
        && fields["status"] == "dismiss"
        && original.kind == "harness.diagnostic"
        && source["code"] == crate::driver_hook::PERMISSION_DENIED_CODE
        && original.subject == row.claim.subject
        && fields["delegation"]["episode"] == original.id
        && fields["acted_for"] == fields["delegation"]["person"]
    {
        let owner = fields["acted_for"].as_str()?;
        let proof = &fields["delegation"];
        (
            owner,
            "harness-prompt",
            format!(
                "Denied {} call dismissed",
                source["status"].as_str().unwrap_or("tool")
            ),
            format!(
                "{}\n\nDismissed by {actor} for {owner}; the call remains denied.\nInstruction: {}\nQuote: {}\nPolicy: {}\nEpisode: {}",
                source["reason"].as_str().unwrap_or_default(),
                proof["message"].as_str().unwrap_or_default(),
                proof["quote"].as_str().unwrap_or_default(),
                proof["policy"].as_str().unwrap_or_default(),
                original.id
            ),
            true,
        )
    } else {
        return None;
    };
    if person.is_some_and(|person| person != owner) {
        return None;
    }
    let mut item = json!({"id":format!("attention/closed/{}",row.claim.id),"kind":"attention","attention_kind":kind,
        "source_id":original.subject,"source_kind":kind,"person_id":owner,"episode":original.id,
        "revision":row.claim.id,"updated_at":client_timestamp(row.claim.accepted_at_unix_ms),
        "title":title,"detail":detail,"priority":"normal","state":"resolved",
        "requested_at":client_timestamp(original.at),"targets":[row.claim.subject],"actions":[],"alert":alert,
        "operational":{"layer":"history","actionable":false,"reasons":["source-resolved"]}});
    if kind == "person-step" {
        item["update"] = source["request"].clone();
    }
    if let Some(requester) = original.actor {
        item["requester_id"] = json!(requester);
    }
    Some(item)
}

pub(super) async fn detail(
    state: AppState,
    id: String,
    person: Option<String>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    let (snapshot, item) = blocking_store(move || {
        state.store.read_snapshot(|cut| {
            let snapshot = client_snapshot_at(&state, cut);
            let row = state
                .store
                .attention_history_resolution(&id, cut, person.as_deref())?;
            if row.as_ref().is_some_and(|row| row.omitted)
                && person.as_deref().is_some_and(|person| {
                    row.as_ref().and_then(|row| row.owner.as_deref()) != Some(person)
                })
            {
                // An unavailable source cannot establish this person's authority.
                return Ok((snapshot, None));
            }
            if row.as_ref().is_some_and(|row| row.omitted) {
                anyhow::bail!(St3Error::new(
                    "attention-history-not-ready",
                    "this history item exceeds its read bounds or cannot be decoded"
                ));
            }
            let row = row.and_then(|row| row.resolution);
            if let Some(row) = &row {
                // With an absent source, only a denial's recorded person can
                // establish detail authority. Never expose another person's record.
                if row.original.is_none()
                    && person.as_deref().is_some_and(|person| {
                        row.claim.body["fields"]["acted_for"].as_str() != Some(person)
                    })
                {
                    return Ok((snapshot, None));
                }
                ensure_source(row)?;
            }
            let item = row.and_then(|row| resource(row, person.as_deref()));
            anyhow::ensure!(
                serde_json::to_vec(&item)?.len() <= CLIENT_MAX_RESPONSE_BYTES,
                "attention history detail exceeds response limit"
            );
            Ok((snapshot, item))
        })
    })
    .await
    .map_err(history_error)?;
    item.map(|item| (Extension(snapshot), Json(item)))
        .ok_or_else(|| ApiError::not_found("attention history item does not exist"))
}

pub(super) async fn page(
    state: AppState,
    snapshot: ClientSnapshot,
    query: ClientListQuery,
    collection: &'static str,
) -> Result<ClientPageResponse, ApiError> {
    if query.actor.is_some()
        || query.owner_run.is_some()
        || query.status.is_some()
        || query.owner.is_some()
        || query.native_only
        || query
            .state
            .as_deref()
            .is_some_and(|state| state != "resolved")
    {
        return Err(ApiError::bad(St3Error::new(
            "validation-failed",
            "attention history supports person and resolved-state filters",
        )));
    }
    let cursor = query.cursor.as_deref().map(decode).transpose()?;
    let limit = query
        .limit
        .map(|limit| limit.clamp(1, MAX_CANDIDATES))
        .or_else(|| cursor.as_ref().map(|cursor| cursor.limit))
        .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS.clamp(1, MAX_CANDIDATES));
    if let Some(cursor) = &cursor
        && (cursor.collection != format!("{collection}-history")
            || !cursor.history
            || cursor.person != query.person
            || cursor.snapshot.id != snapshot.id
            || cursor.snapshot.store_index != snapshot.store_index
            || cursor.limit != limit
            || !(1..=MAX_CANDIDATES).contains(&cursor.limit)
            || cursor.state != query.state
            || cursor.expires_at_unix_ms <= client_now_ms()
            || cursor.before_index.is_none())
    {
        return Err(client_page_expired(
            "attention history cursor does not match its filter or snapshot",
        ));
    }
    let reader = state.clone();
    let person = query.person.clone();
    let (snapshot, items, has_more, before, expires, omitted) = blocking_store(move || {
        reader.store.read_snapshot(|current| {
            let snapshot = cursor.as_ref().map_or_else(
                || client_snapshot_at(&reader, current),
                |cursor| cursor.snapshot.clone(),
            );
            anyhow::ensure!(
                snapshot.store_index <= current,
                "history cut is no longer available"
            );
            let before = cursor
                .as_ref()
                .and_then(|cursor| cursor.before_index)
                .unwrap_or(i64::MAX as u64);
            let mut rows = reader.store.attention_history_candidates(
                snapshot.store_index,
                before,
                limit,
                person.as_deref(),
            )?;
            let has_more = rows.len() > limit;
            rows.truncate(limit);
            let before = rows.last().map_or(before, |row| row.store_index);
            let mut items = Vec::new();
            let mut omitted = 0usize;
            let mut response_bytes = 0usize;
            for candidate in rows {
                omitted += usize::from(candidate.omitted);
                let Some(row) = candidate.resolution else {
                    continue;
                };
                // Store filters known foreign owners before decoding. Unknown-source
                // rows cannot become visible items or make this entire page fail.
                if ensure_source(&row).is_err() {
                    omitted += 1;
                    continue;
                }
                if let Some(item) = resource(row, person.as_deref()) {
                    let size = serde_json::to_vec(&item)?.len();
                    // Leave room for page/envelope metadata. Refusal is per item,
                    // and the scanned index still advances past it.
                    if size > (CLIENT_MAX_RESPONSE_BYTES / 2).saturating_sub(response_bytes) {
                        omitted += 1;
                        continue;
                    }
                    response_bytes += size;
                    items.push(item);
                }
            }
            let expires = cursor.map_or_else(
                || client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
                |cursor| cursor.expires_at_unix_ms,
            );
            Ok((snapshot, items, has_more, before, expires, omitted))
        })
    })
    .await
    .map_err(history_error)?;
    let next_cursor = has_more
        .then(|| {
            encode(ClientPageCursor {
                snapshot: snapshot.clone(),
                collection: format!("{collection}-history"),
                offset: 0,
                limit,
                history: true,
                person: query.person.clone(),
                actor: None,
                owner_run: None,
                status: None,
                owner: None,
                state: query.state.clone(),
                native_only: false,
                items_digest: "retained-source.v1".into(),
                before_index: Some(before),
                after_key: None,
                expires_at_unix_ms: expires,
            })
        })
        .transpose()?;
    let mut filters = client_page_filters("attention", &query);
    filters.insert("state".into(), "resolved".into());
    Ok((
        Extension(snapshot),
        Json(ClientResourcePage {
            kind: "page".into(),
            collection: collection.into(),
            filters,
            items,
            page: ClientPageInfo {
                limit,
                has_more,
                next_cursor,
                cursor_expires_at: has_more.then(|| client_timestamp(expires)),
                omitted_items: Some(omitted),
            },
            sync: client_sync_notice(&state),
            replicated: None,
        }),
    ))
}
