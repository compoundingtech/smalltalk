//! Recorded local prompt closures compose with canonical history using two consumed
//! frontiers. A lookahead never advances a source until its row is emitted or excluded.
use super::*;

/// A general projection grant does not grant access to another person's native
/// command. Apply this before returning rows from any open client reader.
pub(super) fn retain_authorized(items: &mut Vec<Value>, authority: &str) {
    items.retain(|item| {
        item["source_kind"] != "harness-prompt"
            || (authority.starts_with("person/") && item["person_id"].as_str() == Some(authority))
    });
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HistoryCursor {
    cut: u64,
    at_unix_ms: u128,
    after: Option<(i64, String)>,
    canonical_done: bool,
    authorized: bool,
}

pub(super) async fn history_page(
    state: &AppState,
    snapshot: ClientSnapshot,
    query: &ClientListQuery,
    custom_forms: bool,
    prompt_authorized: bool,
) -> Result<ClientPageResponse, ApiError> {
    let person = attention_history_person(query.person.as_deref())?;
    let (snapshot, limit, expiry, mut canonical_after, history_snapshot, mut prompt_cursor) =
        if let Some(encoded) = &query.cursor {
            let cursor = decode_client_cursor(encoded)?;
            let expected = attention_cursor_mac(&cursor)?;
            let authentic = cursor.items_digest.len() == expected.len()
                && cursor
                    .items_digest
                    .bytes()
                    .zip(expected.bytes())
                    .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                    == 0;
            if !authentic
                || cursor.history_snapshot.is_none()
                || cursor.prompt_history.is_none()
                || cursor.collection != "attention"
                || cursor.history != query.history
                || cursor.person != query.person
                || cursor.actor != query.actor
                || cursor.owner_run != query.owner_run
                || cursor.status != query.status
                || cursor.state != query.state
                || cursor.owner != query.owner
                || cursor.native_only != query.native_only
                || cursor.snapshot.id != snapshot.id
                || cursor.snapshot.host_id != client_host_id(&state.node)
                || query
                    .limit
                    .is_some_and(|n| n.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
                || client_now_ms() > cursor.expires_at_unix_ms
                || cursor
                    .prompt_history
                    .as_ref()
                    .is_some_and(|c| c.authorized != prompt_authorized)
            {
                return Err(client_page_expired(
                    "the attention history cursor expired or does not match this person, snapshot or filter",
                ));
            }
            let after = cursor
                .after_key
                .as_ref()
                .zip(cursor.before_index)
                .map(|((ms, id), version)| (*ms as i64, id.clone(), version));
            (
                cursor.snapshot,
                cursor.limit,
                cursor.expires_at_unix_ms,
                after,
                cursor.history_snapshot,
                cursor.prompt_history.unwrap(),
            )
        } else {
            (
                snapshot,
                query.limit.unwrap_or(5).clamp(1, CLIENT_MAX_PAGE_ITEMS),
                client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
                None,
                None,
                HistoryCursor {
                    cut: state
                        .store
                        .prompt_history_cut()
                        .map_err(ApiError::internal)?,
                    at_unix_ms: client_now_ms(),
                    after: None,
                    canonical_done: false,
                    authorized: prompt_authorized,
                },
            )
        };
    let reader = state.clone();
    let index = snapshot.store_index;
    // Native closures advance independently of the replicated projection clock.
    // Pin their own read clock alongside their observation cutoff in the MAC.
    let at = prompt_cursor.at_unix_ms;
    let read = blocking_store(move || {
        reader.store.request_attention_history()?;
        // Pin the canonical epoch even when this page emits only native prompt rows.
        let mut canonical = reader.store.attention_history_page(
            &person,
            index,
            history_snapshot.as_ref(),
            1,
            canonical_after.as_ref(),
        )?;
        let pinned = canonical.snapshot.clone();
        let mut availability = canonical.availability.clone();
        let mut items = Vec::new();
        let mut more = true;
        // A page can be short when bounded work excludes obsolete or unauthorized
        // records. Both consumed frontiers still progress on the following page.
        let budget = limit.saturating_mul(2).max(32);
        for _ in 0..budget {
            let prompt = if prompt_cursor.authorized {
                reader.store.prompt_history_next(
                    &person,
                    prompt_cursor.cut,
                    at,
                    prompt_cursor.after.as_ref(),
                )?
            } else {
                None
            };
            if prompt.as_ref().is_some_and(|(_, item)| item.is_none()) {
                prompt_cursor.after = prompt.map(|(key, _)| key);
                continue;
            }
            if !prompt_cursor.canonical_done && canonical.items.is_empty() {
                if let Some(next) = canonical.next.take() {
                    canonical_after = Some(next);
                    canonical = reader.store.attention_history_page(
                        &person,
                        index,
                        Some(&pinned),
                        1,
                        canonical_after.as_ref(),
                    )?;
                    continue;
                }
                prompt_cursor.canonical_done = true;
            }
            let canonical_key = (!prompt_cursor.canonical_done)
                .then(|| canonical.keys.first())
                .flatten()
                .map(|(ms, id, _)| (*ms, id.clone()));
            if canonical_key.is_none() && prompt.is_none() {
                more = false;
                break;
            }
            // Unconsumed lookahead retains its original frontier, including the last
            // canonical row: next=None alone does not mean that row was consumed.
            if items.len() == limit {
                break;
            }
            if prompt.as_ref().is_some_and(|(key, _)| {
                canonical_key
                    .as_ref()
                    .is_none_or(|canonical| key > canonical)
            }) {
                let (key, item) = prompt.unwrap();
                prompt_cursor.after = Some(key);
                items.push(item.unwrap());
            } else {
                items.push(canonical.items.remove(0));
                canonical_after = Some(canonical.keys.remove(0));
                if canonical.next.is_some() {
                    canonical = reader.store.attention_history_page(
                        &person,
                        index,
                        Some(&pinned),
                        1,
                        canonical_after.as_ref(),
                    )?;
                } else {
                    prompt_cursor.canonical_done = true;
                }
            }
        }
        availability.complete = false;
        availability.note = Some(format!(
            "{} Prompt history is host-local.",
            availability
                .note
                .as_deref()
                .unwrap_or("Recorded history has incomplete coverage.")
        ));
        Ok((
            items,
            more,
            canonical_after,
            pinned,
            prompt_cursor,
            availability,
        ))
    })
    .await
    .map_err(|error| {
        if error.message.contains("attention-history-epoch-changed") {
            client_page_expired("attention history was reset by replay or trim; restart pagination")
        } else {
            error
        }
    })?;
    let (mut items, has_more, canonical_after, pinned, prompt_cursor, availability) = read;
    client_attention_compatibility(&mut items, custom_forms);
    let next_cursor = if has_more {
        let mut cursor = ClientPageCursor {
            history_snapshot: Some(pinned),
            prompt_history: Some(prompt_cursor),
            snapshot: snapshot.clone(),
            collection: "attention".into(),
            offset: 0,
            limit,
            history: query.history,
            person: query.person.clone(),
            actor: query.actor.clone(),
            owner_run: query.owner_run.clone(),
            status: query.status.clone(),
            owner: query.owner.clone(),
            state: query.state.clone(),
            native_only: query.native_only,
            items_digest: String::new(),
            before_index: canonical_after.as_ref().map(|key| key.2),
            after_key: canonical_after.map(|(ms, id, _)| (ms as u128, id)),
            expires_at_unix_ms: expiry,
        };
        cursor.items_digest = attention_cursor_mac(&cursor)?;
        Some(encode_client_cursor(&cursor)?)
    } else {
        None
    };
    Ok((
        Extension(snapshot),
        Json(ClientResourcePage {
            kind: "page".into(),
            collection: "attention".into(),
            filters: client_page_filters("attention", query),
            items,
            page: ClientPageInfo {
                limit,
                has_more,
                next_cursor,
                cursor_expires_at: has_more.then(|| client_timestamp(expiry)),
            },
            sync: client_sync_notice(state),
            replicated: None,
            history: Some(availability),
        }),
    ))
}
