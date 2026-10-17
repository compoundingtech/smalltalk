use super::*;

fn client_launch_approval_resources(
    state: &AppState,
    session: &PlanningSessionView,
) -> anyhow::Result<Vec<Value>> {
    let variants = client_launch_variant_resources(state, session)?;
    let tokens = variants
        .into_iter()
        .filter_map(|variant| {
            Some((
                variant["candidate_revision"].as_u64()?,
                (
                    variant["preview_token"].as_str()?.to_owned(),
                    variant["id"].as_str()?.to_owned(),
                ),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let claims = state.store.claims_for(&session.subject, None)?;
    let mut approvals = claims
        .into_iter()
        .filter(|claim| claim.kind == "planning-session.approved")
        .filter_map(|claim| {
            let revision = claim.body.pointer("/fields/candidate_revision")?.as_u64()?;
            let (token, variant_id) = tokens.get(&revision)?;
            let reviewer = claim
                .body
                .pointer("/fields/requester")
                .and_then(Value::as_str)
                .or(claim.actor.as_deref())?;
            Some(json!({
                "id": format!("launch-approval/{}/{}", session.id, revision),
                "kind": "launch-approval",
                "revision": claim.id,
                "updated_at": client_timestamp(claim.accepted_at_unix_ms),
                "launch_id": format!("launch/{}", session.id),
                "variant_id": variant_id,
                "reviewer": reviewer,
                "state": "approved",
                "preview_token": token,
                "decided_at": client_timestamp(claim.accepted_at_unix_ms),
                "operational": { "layer": "history", "actionable": false, "reasons": ["approved"] }
            }))
        })
        .collect::<Vec<_>>();
    approvals.sort_by(|left, right| {
        left["decided_at"]
            .as_str()
            .cmp(&right["decided_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(approvals)
}

fn client_history_resource(claim: ClaimRecord) -> Value {
    let mut targets = vec![claim.subject.clone()];
    if let Some(actor) = &claim.actor
        && actor.contains('/')
        && !actor.chars().any(char::is_whitespace)
    {
        targets.push(actor.clone());
    }
    json!({
        "id": format!("history/{}", claim.store_index),
        "kind": "history",
        "revision": claim.id,
        "updated_at": client_timestamp(claim.accepted_at_unix_ms),
        "event_type": claim.kind,
        "occurred_at": client_timestamp(claim.accepted_at_unix_ms),
        "store_index": claim.store_index,
        "summary": format!("{} on {}", claim.kind, claim.subject),
        "targets": targets,
        "operational": { "layer": "history", "actionable": false, "reasons": ["audit"] }
    })
}

pub async fn client_work(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    if query.history {
        return client_work_history(&state, snapshot, &query).await;
    }
    // While a refresher governs the work list (from when it holds the list's view, before it
    // starts), pages come from its publications and never fold the list on read. With none,
    // as with ST3_PUBLISHED_LISTS=off, they read directly, as before.
    if !matches!(
        state.store.published_work_list().current(),
        crate::store::published_list::ListRead::NeverStarted
    ) || state.store.collection_view_published("work")
    {
        return client_work_published(state, snapshot, query).await;
    }
    let actor = query.actor.clone();
    client_snapshot_page(&state, snapshot, "work", &query, move |state, snapshot| {
        client_work_resources(
            &state.store,
            actor.as_deref(),
            false,
            client_snapshot_time(snapshot),
            snapshot.store_index,
        )
    })
    .await
}

/// A current work page from the work list's publications. A first page serves the newest
/// current publication, or with `fresh=true` one at least as new as the request; a continuation
/// serves the publication its first page did. Neither folds the list: with nothing current to
/// serve it is not ready, and once the refresher has ended, unavailable.
async fn client_work_published(
    state: AppState,
    snapshot: ClientSnapshot,
    query: ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    use crate::store::published_list::ListRead;
    if let Some(encoded) = &query.cursor {
        let cursor = decode_client_cursor(encoded)?;
        // A first page read before the refresher started continues from the page cache.
        if !cursor.items_digest.starts_with(PUBLISHED_WORK_DIGEST) {
            let page = client_page(&state, &snapshot, "work", Vec::new(), &query)?;
            return Ok((Extension(snapshot), Json(page)));
        }
        let page = client_work_published_continuation(&state, &snapshot, &query, cursor)?;
        return Ok((Extension(snapshot), Json(page)));
    }
    let list = state.store.published_work_list();
    let (publication, id) = if query.fresh {
        let waited = Instant::now();
        let fresh = wait_for_work_list(&state.store).await;
        record_work_stage(request_latency::WorkStage::FreshWait, waited.elapsed());
        fresh?
    } else {
        match list.current() {
            ListRead::Served(publication, id) => (publication, id),
            ListRead::NotReady | ListRead::NeverStarted => {
                list.request_refresh();
                return Err(published_lists::work_list_not_ready());
            }
            ListRead::Ended => return Err(published_lists::work_list_ended()),
        }
    };
    let reader = state.clone();
    blocking_store(move || Ok(client_work_published_page(&reader, &publication, id, &query))).await?
}

/// Wait up to [`WORK_LIST_FRESH_WAIT`] for a current publication at or after the store's index
/// when the request arrived. Each way it can end answers differently: such a publication; not
/// fresh, once the wait is over; not ready, if the list is withdrawn or forgotten meanwhile or
/// its change channel closes; unavailable, if the refresher ends. Never stale rows as fresh.
async fn wait_for_work_list(
    store: &Arc<Store>,
) -> Result<
    (
        Arc<crate::store::published_list::Publication<crate::store::work_list::WorkRows>>,
        crate::store::published_list::PublicationId,
    ),
    ApiError,
> {
    use crate::store::published_list::ListRead;
    let list = store.published_work_list();
    let wanted = store.index().map_err(ApiError::internal)?;
    let mut changes = list.subscribe();
    let deadline = tokio::time::Instant::now() + WORK_LIST_FRESH_WAIT;
    let mut asked = false;
    loop {
        drop(changes.borrow_and_update());
        let newest = match list.current() {
            ListRead::Served(publication, id) if publication.cut >= wanted => return Ok((publication, id)),
            // Behind the request: ask once for a fold now, without the rest of the pause.
            ListRead::Served(publication, _) => {
                if !asked {
                    list.request_fresh();
                    asked = true;
                }
                publication.cut
            }
            ListRead::Ended => return Err(published_lists::work_list_ended()),
            ListRead::NotReady | ListRead::NeverStarted => return Err(published_lists::work_list_not_ready()),
        };
        match tokio::time::timeout_at(deadline, changes.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(published_lists::work_list_not_ready()),
            Err(_) => return Err(published_lists::work_list_not_fresh(wanted, Some(newest))),
        }
    }
}

/// The next page of a published work page: the same publication and actor rows, sliced at the
/// cursor. It expires as a cached page does, and also once a forget replaced the projections
/// its rows were folded from. A later publication of the same generation leaves it valid.
fn client_work_published_continuation(
    state: &AppState,
    snapshot: &ClientSnapshot,
    query: &ClientListQuery,
    cursor: ClientPageCursor,
) -> Result<ClientResourcePage, ApiError> {
    if cursor.collection != "work"
        || cursor.history != query.history
        || cursor.person != query.person
        || cursor.actor != query.actor
        || cursor.owner_run != query.owner_run
        || cursor.owner.is_some()
        || cursor.state.is_some()
        || cursor.status != query.status
        || cursor.native_only != query.native_only
        || query.limit.is_some_and(|limit| limit.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
        || cursor.snapshot.id != snapshot.id
        || cursor.snapshot.store_index != snapshot.store_index
    {
        return Err(client_page_expired(
            "the page cursor does not match this collection, snapshot, or filter",
        ));
    }
    if client_now_ms() > cursor.expires_at_unix_ms {
        return Err(client_page_expired("the page cursor expired"));
    }
    // Served only from the store's own list, in the generation its rows were folded in.
    let list = state.store.published_work_list();
    let (rows, id) = {
        let mut pages = published_work_pages().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        purge_published_work_pages(&mut pages, list);
        // The cursor is the client's to edit: it must repeat the limit, snapshot and filters
        // its first page issued, so it can never ask for more than one bounded page, and two
        // pages with one digest and expiry but different contracts never answer for each other.
        let contract = PublishedWorkContract::of(&cursor);
        if !(1..=CLIENT_MAX_PAGE_ITEMS).contains(&cursor.limit) {
            return Err(client_page_expired("the page cursor does not match the page it continues"));
        }
        let page = pages
            .iter()
            .find(|page| {
                page.digest == cursor.items_digest
                    && page.expires_at_unix_ms == cursor.expires_at_unix_ms
                    && page.contract == contract
            })
            .ok_or_else(|| client_page_expired("the page snapshot is no longer available; restart pagination"))?;
        (Arc::clone(&page.rows), page.id)
    };
    let current = |list: &crate::store::published_list::PublishedList<crate::store::work_list::WorkRows>| {
        list.instance() == id.list && list.generation() == id.generation
    };
    if !current(list) {
        return Err(client_page_expired("the work list was refolded since this page was read; restart pagination"));
    }
    let end = cursor.offset.saturating_add(cursor.limit).min(rows.len());
    let items = rows[cursor.offset.min(end)..end].iter().map(|row| (**row).clone()).collect::<Vec<_>>();
    let has_more = end < rows.len();
    // The final fence: a forget while the page was sliced replaced the rows it holds.
    if !current(list) {
        return Err(client_page_expired("the work list was refolded since this page was read; restart pagination"));
    }
    let next_cursor = if has_more {
        let mut next = cursor.clone();
        next.offset = cursor.offset.saturating_add(items.len());
        Some(encode_client_cursor(&next)?)
    } else {
        None
    };
    Ok(ClientResourcePage {
        kind: "page".into(),
        collection: "work".into(),
        filters: client_page_filters("work", query),
        items,
        page: ClientPageInfo {
            limit: cursor.limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(cursor.expires_at_unix_ms)),
        },
        sync: client_sync_notice(state),
        replicated: None,
    })
}

/// The work history, read a page at a time in the order it shows, each page inside one SQLite
/// snapshot. Continuation seeks after the last update time and subject, so unrelated writes
/// do not invalidate it or shift its offset.
async fn client_work_history(
    state: &AppState,
    snapshot: ClientSnapshot,
    query: &ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    let (offset, limit, expires_at_unix_ms, after_key) = if let Some(encoded) = &query.cursor {
        let cursor = decode_client_cursor(encoded)?;
        if cursor.collection != "work"
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
                "the page cursor does not match this collection, snapshot, or filter",
            ));
        }
        if client_now_ms() > cursor.expires_at_unix_ms {
            return Err(client_page_expired("the page cursor expired"));
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
            query
                .limit
                .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
                .clamp(1, CLIENT_MAX_PAGE_ITEMS),
            client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
            None,
        )
    };
    let reader = state.clone();
    let actor = query.actor.clone();
    let read = blocking_store(move || {
        reader.store.clone().read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader, index);
            let (items, has_more) = client_work_history_page_after(
                &reader.store,
                actor.as_deref(),
                client_snapshot_time(&snapshot),
                index,
                offset,
                limit,
                after_key.as_ref(),
            )?;
            Ok(Some((snapshot, items, has_more)))
        })
    })
    .await?;
    let Some((snapshot, items, has_more)) = read else {
        return Err(client_page_expired(
            "the snapshot changed; restart pagination from the first page",
        ));
    };
    let next_cursor = has_more
        .then(|| {
            encode_client_cursor(&ClientPageCursor {
                snapshot: snapshot.clone(),
                collection: "work".into(),
                offset: offset.saturating_add(items.len()),
                limit,
                history: query.history,
                person: query.person.clone(),
                actor: query.actor.clone(),
                owner_run: query.owner_run.clone(),
                status: query.status.clone(),
                owner: None,
                state: None,
                native_only: query.native_only,
                items_digest: "sql-page".into(),
                before_index: None,
                after_key: None,
                expires_at_unix_ms,
            })
        })
        .transpose()?;
    let page = ClientResourcePage {
        kind: "page".into(),
        collection: "work".into(),
        filters: client_page_filters("work", query),
        items,
        page: ClientPageInfo {
            limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)),
        },
        sync: client_sync_notice(state),
        replicated: None,
    };
    Ok((Extension(snapshot), Json(page)))
}

pub async fn client_work_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let actor = query.actor.clone();
    let snapshot_unix_ms = client_snapshot_time(&snapshot);
    let requested = id.clone();
    let item = blocking_store(move || {
        client_work_item(
            &store,
            &requested,
            actor.as_deref(),
            snapshot_unix_ms,
            snapshot.store_index,
        )
    })
    .await?;
    item.map(Json).ok_or_else(|| {
        ApiError::not_found(format!(
            "work `{}` does not exist",
            client_detail_id("work", &id)
        ))
    })
}

pub async fn client_agents(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    if query.min_cut.is_some() && query.cursor.is_some() {
        return Err(client_v0::validation("min_cut is only supported on the first agents page"));
    }
    if let Some(wanted) = query.min_cut {
        let head = state.store.index().map_err(ApiError::internal)?;
        if wanted > head {
            let mut error = agent_roster_cut_not_ready(wanted);
            error.details.insert("head".into(), json!(head));
            return Err(error);
        }
    }
    // A daemon's readers never fold the roster: they answer from its refresher's publications.
    if state.store.agent_roster_refresher_running() {
        return client_agents_published(state, snapshot, query).await;
    }
    // A warm request never queues behind a cold projection. Drop the probe's SQLite
    // snapshot before waiting, then recheck all cache fences in the admitted snapshot.
    // Embedded stores without a refresher build once and reject an unmet cut immediately;
    // only the daemon's publication path has a refresher to wait for.
    let requested = query.min_cut.unwrap_or(snapshot.store_index);
    let fresh = query.fresh || query.min_cut.is_some();
    let min_cut = query.min_cut;
    for admitted in [false, true] {
        let admission = if admitted {
            Some(state.store.admit_agent_resources("http").await)
        } else {
            None
        };
        let reader = state.clone();
        let snapshot = snapshot.clone();
        let query = query.clone();
        let filtered = query.status.is_some();
        let result = blocking_roster(move || {
            let _admission = admission;
            if let Some(admission) = &_admission {
                admission.enter_worker();
            }
            reader.store.clone().read_snapshot(|index| {
                let snapshot = if query.cursor.is_some() {
                    snapshot
                } else {
                    client_snapshot_at(&reader, index)
                };
                Ok(reader.store.with_owned_set_snapshot_reads(|| {
                    client_agents_page_at(&reader, snapshot, &query, admitted)
                }))
            })
        }).await??;
        if let Some(page) = result {
            let page = checked_agent_roster_page(Some(page), min_cut)?
                .expect("an exact roster page is present");
            let path = match (filtered, admitted) {
                (true, _) => "filtered_full",
                (false, true) => "exact_cold",
                (false, false) => "exact_warm",
            };
            crate::otel::record_roster_read(path, requested, Some(page.0.0.store_index), fresh);
            return Ok(page);
        }
    }
    unreachable!("an admitted roster read always builds missing cards")
}

/// An agents page from the refresher's publications. A first page answers at once from the
/// newest one, or says not ready; a legacy fresh read briefly waits for its cut or for proof
/// that a complete publication's cards are unchanged through it, while a named min_cut uses one
/// strict, fixed mutation-cut barrier. A continuation answers from the roster published at
/// exactly its first page's cut, or says the page expired. Neither folds a card.
async fn client_agents_published(
    state: AppState,
    snapshot: ClientSnapshot,
    query: ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    state.store.note_agent_roster_read();
    let requested = query.min_cut.unwrap_or(snapshot.store_index);
    if query.cursor.is_some() {
        let reader = state.clone();
        return blocking_roster(move || {
            Ok(client_agents_published_continuation(&reader, snapshot, &query))
        })
        .await?;
    }
    let fresh = query.fresh || query.min_cut.is_some();
    let fresh_wait = if let Some(wanted) = query.min_cut {
        let waited = Instant::now();
        let published = wait_for_agent_roster_cut(&state.store, query.history, wanted).await;
        record_roster_stage(request_latency::RosterStage::FreshWait, waited.elapsed());
        crate::otel::roster_attribute("st.roster.publication_wait_ms", waited.elapsed().as_secs_f64() * 1000.0);
        if !published {
            return Err(agent_roster_cut_not_ready(wanted));
        }
        None
    } else if query.fresh {
        let waited = Instant::now();
        let outcome = wait_for_agent_roster(&state.store, query.history).await;
        record_roster_stage(request_latency::RosterStage::FreshWait, waited.elapsed());
        Some(outcome)
    } else {
        None
    };
    let reader = state.clone();
    let history = query.history;
    let min_cut = query.min_cut;
    let built = Instant::now();
    let page = blocking_roster(move || {
        Ok(client_agents_published_page(&reader, &query, requested, fresh_wait))
    }).await;
    if fresh {
        record_roster_stage(request_latency::RosterStage::FreshPage, built.elapsed());
    }
    match checked_agent_roster_page(page??, min_cut)? {
        Some(page) => Ok(page),
        None => {
            crate::otel::record_roster_read("not_ready", requested, None, fresh);
            if history {
                state.store.request_agent_roster_history();
            } else {
                state.store.request_agent_roster_refresh();
            }
            Err(agent_roster_not_ready())
        }
    }
}

/// A page continuing a first page served from a publication: its cards come from the roster
/// published at exactly that page's cut. Once that roster is gone, pagination restarts.
fn client_agents_published_continuation(
    state: &AppState,
    snapshot: ClientSnapshot,
    query: &ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    let store = &state.store;
    let mut page = client_page_read(state, &snapshot, "agents", Vec::new(), query, true)?;
    let published = store.published_agent_roster_at(snapshot.store_index, query.history);
    let path = if query.status.is_some() { "filtered_full" } else { "published_complete" };
    let served = published.as_ref().map(|_| snapshot.store_index);
    crate::otel::record_roster_read(path, snapshot.store_index, served, query.fresh);
    let snapshot = match &published {
        Some((_, published_at)) => roster_snapshot(state, snapshot.store_index, *published_at),
        None => snapshot,
    };
    // A status-filtered first page kept its whole overlaid cards for its continuations.
    if query.status.is_some() {
        return Ok((Extension(snapshot), Json(page)));
    }
    let Some((cards, _)) = published else {
        return Err(client_page_expired(
            "the roster this page was read from is no longer published; restart pagination",
        ));
    };
    page.items = client_agent_cards_from_cached(
        store, page_cards(&cards, &page.items), &page.items, &snapshot.created_at,
    )?;
    Ok((Extension(snapshot), Json(page)))
}

fn client_agents_page_at(
    state: &AppState,
    snapshot: ClientSnapshot,
    query: &ClientListQuery,
    admitted: bool,
) -> Result<Option<ClientPageResponse>, ApiError> {
    let store = &state.store;
    let index = snapshot.store_index;
    let mut items = if query.cursor.is_some() {
        Vec::new()
    } else if query.status.is_some() {
        let mut cards = if admitted {
            client_agent_resources_cached(store, query.history, index).map_err(ApiError::internal)?
        } else {
            let Some(cards) = store.agent_resources_cached_at(index, query.history, None)
                .map_err(ApiError::internal)? else { return Ok(None) };
            (*cards).clone()
        };
        overlay_agent_resources(store, &mut cards, &snapshot.created_at)
            .map_err(ApiError::internal)?;
        cards
    } else if admitted {
        client_agent_page_refs(store, query.history, index).map_err(ApiError::internal)?
    } else {
        let Some(refs) = store.agent_page_refs_cached_at(index, query.history) else {
            return Ok(None);
        };
        (*refs).clone()
    };
    if let Some(status) = query.status.as_deref() {
        items.retain(|item| item["state"].as_str() == Some(status));
    }
    // On a first-page probe, determine coverage before publishing pagination metadata.
    let cached_cards = if !admitted && query.status.is_none() {
        let refs = if query.cursor.is_some() {
            client_page_read(state, &snapshot, "agents", Vec::new(), query, true)?.items
        } else {
            items.iter().take(query.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
                .clamp(1, CLIENT_MAX_PAGE_ITEMS)).cloned().collect()
        };
        let selected = refs.iter().filter_map(|r| r["id"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let Some(cards) = store.agent_resources_cached_at(index, query.history, Some(&selected))
            .map_err(ApiError::internal)? else { return Ok(None) };
        Some(cards.iter().filter(|card| card["id"].as_str()
            .is_some_and(|id| selected.contains(id))).cloned().collect())
    } else {
        None
    };
    let mut page = client_page_read(state, &snapshot, "agents", items, query, true)?;
    if query.status.is_none() {
        page.items = if let Some(cards) = cached_cards {
            client_agent_cards_from_cached(store, cards, &page.items, &snapshot.created_at)?
        } else {
            client_agent_cards_for_page(store, query.history, index, &page.items, &snapshot.created_at)?
        };
    }
    Ok(Some((Extension(snapshot), Json(page))))
}

pub async fn client_agents_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let history = query.history;
    let created_at = snapshot.created_at.clone();
    let snapshot_index = snapshot.store_index;
    let subject = client_detail_id("agent", &id);
    let items = blocking_store(move || {
        let selected = BTreeSet::from([subject]);
        let mut items = client_agent_resources_selected(
            &store,
            history,
            snapshot_index,
            Some((&selected, &[])),
        )?;
        add_agent_todos(&store, &mut items, snapshot_index)?;
        overlay_agent_resources(&store, &mut items, &created_at)?;
        for item in &mut items {
            if let Some(subject) = item["id"].as_str()
                && let Some(operation) = store.runtime_start_for(subject)?
            {
                let phase = if operation.revoked_by.is_some() { "revoked" }
                    else if operation.result.as_ref().is_some_and(|result| result["phase"] == "unknown") { "unknown" }
                    else if operation.finished { "completed" }
                    else if operation.dispatched { "running" } else { "queued" };
                let (running_jobs, revoked_running_jobs) = store.runtime_start_counts()?;
                let (unresolved_jobs, revoked_unresolved_jobs) = store.unresolved_runtime_start_counts()?;
                item["runtime_start"] = json!({
                    "operation_id": operation.id, "phase": phase,
                    "worker_running": operation.dispatched && !operation.finished,
                    "cleanup_verified": operation.cleanup_verified,
                    "running_jobs": running_jobs,
                    "revoked_running_jobs": revoked_running_jobs,
                    "unresolved_jobs": unresolved_jobs,
                    "revoked_unresolved_jobs": revoked_unresolved_jobs,
                });
            }
        }
        Ok(items)
    })
    .await?;
    client_detail(items, "agent", &id)
}

pub async fn client_sessions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let history = query.history;
    let native_only = query.native_only;
    client_snapshot_page(
        &state,
        snapshot,
        "sessions",
        &query,
        move |state, snapshot| {
            let mut items = client_session_resources(
                &state.store,
                history,
                &snapshot.created_at,
                snapshot.store_index,
                state.native_session_home.as_deref(),
                native_only,
            )?;
            if native_only {
                items.retain(|item| item.get("managed") == Some(&Value::Bool(false)));
            }
            Ok(items)
        },
    )
    .await
}

pub async fn client_sessions_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(id) = id.strip_suffix("/timeline") {
        // An agent's timeline is its current session's: st resolves it, not the client.
        let session_id = crate::relay_trace::result(crate::relay_trace::Phase::Session, || {
            client_v0::conversation_session_id(&state, id)
        })?;
        let id = session_id.as_str();
        let managed = crate::relay_trace::result(crate::relay_trace::Phase::Owner, || {
            managed_session_owner_at(&state.store, snapshot.store_index, &session_id)
        })
        .map_err(ApiError::internal)?;
        if let Some((_, _, origin)) = managed {
            let remote_host = origin
                .as_deref()
                .filter(|origin| *origin != state.store.origin())
                .map(client_host_id);
            if let Some(remote_host) = remote_host {
                let relay = state
                    .client_relay
                    .as_ref()
                    .ok_or_else(|| remote_unavailable_for_owner(&state, &remote_host))?;
                if !client_v0::acting_party(&session) {
                    return Err(ApiError::bad(St3Error::new(
                        "forbidden",
                        "remote session detail requires a concrete person or agent",
                    )));
                }
                let mut value = relay
                    .read(
                        &remote_host,
                        &crate::peer::ClientReadRequest {
                            authority_actor: session.authority_actor.clone(),
                            relay: None,
                            request: crate::peer::ClientReadOperation::Timeline {
                                session_id,
                                limit: query.limit.unwrap_or(50).clamp(1, 200),
                                cursor: query.cursor.clone(),
                            },
                        },
                    )
                    .await
                    .map_err(|error| {
                        client_v0::conversation_blocks::availability(remote_read_error(
                            &remote_host,
                            error,
                        ))
                    })?;
                client_v0::conversation_blocks::legacy(&mut value, &session);
                return Ok(Json(value));
            }
        }
        return client_v0::timeline_value(&state, &snapshot, &session, id, &query);
    }
    // Child transcripts are deliberately absent from the top-level session inventory.
    // Resolve their resource by the same parent/task identity used by timeline reads.
    let session_id = client_detail_id("session", &id);
    if let Some(child) = crate::subagent_sessions::resolve_child(
        state.native_session_home.as_deref(),
        &session_id,
    )
    .map_err(ApiError::internal)?
    {
        return Ok(Json(external_session_resource(child)));
    }
    client_detail(
        client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .map_err(ApiError::internal)?,
        "session",
        &id,
    )
}

/// A read that is not sent for a stated reason, in the shape of an unreachable owner.
fn remote_unavailable_because(host: &str, reason: &str, message: &str) -> ApiError {
    let mut error = remote_unavailable(host);
    error.details.insert("reason".into(), reason.into());
    error.message = message.into();
    error
}

pub async fn client_attention(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    let mut effective_query = query.clone();
    effective_query.person.clone_from(&person);
    let history = query.history;
    client_snapshot_page(
        &state,
        snapshot,
        "attention",
        &effective_query,
        move |state, _| {
            let mut items = client_attention_resources_with_previews(state, person.as_deref(), history)?;
            client_attention_compatibility(&mut items, session.custom_forms);
            Ok(items)
        },
    )
    .await
}

pub async fn client_attention_detail(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    let mut items = client_attention_resources_with_previews(&state, person.as_deref(), query.history)
        .map_err(ApiError::internal)?;
    client_attention_compatibility(&mut items, session.custom_forms);
    client_detail(items, "attention", &id)
}

pub async fn client_messages(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    let mut effective_query = query.clone();
    effective_query.person.clone_from(&person);
    let (history, actor) = (query.history, query.actor.clone());
    // An agent another host owns is listed by that host: its messages can reach this node late,
    // and a list that shows only what has arrived reads as empty while the replica lags.
    let owner = match actor.as_deref() {
        Some(actor) => remote_agent_owner(&state, actor).await?,
        None => None,
    };
    let mut replicated = None;
    if let (Some(owner), Some(actor)) = (owner.as_deref(), actor.as_deref()) {
        match relayed_messages_page(&state, &session, owner, actor, &query).await {
            Ok(mut page) => {
                page.replicated = Some(ClientReplicated {
                    owner_host_id: owner.to_owned(),
                    source: "owner".into(),
                    complete: true,
                    state: "current".into(),
                    reason: None,
                });
                return Ok((Extension(snapshot), Json(page)));
            }
            Err(error) if error.code == "remote-unavailable" => {
                let lagging = client_sync_notice(&state)
                    .is_some_and(|notice| notice.peers.iter().any(|peer| peer.host_id == owner));
                replicated = Some(ClientReplicated {
                    owner_host_id: owner.to_owned(),
                    source: "replica".into(),
                    complete: false,
                    state: if lagging { "lagging" } else { "unverified" }.into(),
                    reason: error
                        .details
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let (snapshot, Json(mut page)) = client_snapshot_page(
        &state,
        snapshot,
        "messages",
        &effective_query,
        move |state, _| {
            client_message_resources(&state.store, person.as_deref(), history, actor.as_deref())
        },
    )
    .await
    .map(|(Extension(snapshot), page)| (snapshot, page))?;
    page.replicated = replicated;
    Ok((Extension(snapshot), Json(page)))
}

/// The host that owns `actor`'s runtime, when that is another host.
async fn remote_agent_owner(state: &AppState, actor: &str) -> Result<Option<String>, ApiError> {
    if !actor.starts_with("agent/") {
        return Ok(None);
    }
    let (store, subject) = (state.store.clone(), actor.to_owned());
    let origin = blocking_store(move || {
        Ok(store
            .status(Some(&subject))?
            .subjects
            .first()
            .and_then(|subject| subject.actual_origin.clone()))
    })
    .await?;
    Ok(origin
        .filter(|origin| origin != state.store.origin())
        .map(|origin| client_host_id(&origin)))
}

/// One page of an agent's messages as its owner lists them.
async fn relayed_messages_page(
    state: &AppState,
    session: &client_v0::ClientSession,
    owner: &str,
    actor: &str,
    query: &ClientListQuery,
) -> Result<ClientResourcePage, ApiError> {
    if !client_v0::acting_party(session) {
        return Err(remote_unavailable_because(
            owner,
            "not-relayed",
            "a remote agent's messages are relayed only for a concrete person or agent",
        ));
    }
    let relay = state
        .client_relay
        .as_ref()
        .filter(|relay| relay.reaches(owner))
        .ok_or_else(|| remote_unavailable_for_owner(state, owner))?;
    let value = relay
        .read(
            owner,
            &crate::peer::ClientReadRequest {
                authority_actor: session.authority_actor.clone(),
                relay: None,
                request: crate::peer::ClientReadOperation::Messages {
                    actor: actor.to_owned(),
                    history: query.history,
                    limit: query.limit.map(|limit| limit.clamp(1, 200)),
                    cursor: query.cursor.clone(),
                },
            },
        )
        .await
        .map_err(|error| remote_read_error(owner, error))?;
    serde_json::from_value(value).map_err(ApiError::internal)
}

pub async fn client_messages_detail(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    client_detail(
        client_message_resources(&state.store, person.as_deref(), query.history, None)
            .map_err(ApiError::internal)?,
        "message",
        &id,
    )
}

pub async fn client_launches(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let items = client_launch_resources(&state, query.history).map_err(ApiError::internal)?;
    client_page(&state, &snapshot, "launches", items, &query).map(Json)
}

pub async fn client_launches_detail(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let (snapshot, value) = blocking_store(move || {
        state.store.read_snapshot(|index| {
            let value = client_launch_detail_at(&state, &id, query.history)?.ok_or_else(|| {
                anyhow::anyhow!(St3Error::new(
                    "not-found",
                    format!(
                        "launch `{}` does not exist",
                        client_detail_id("launch", &id)
                    ),
                ))
            })?;
            Ok((client_snapshot_at(&state, index), value))
        })
    })
    .await?;
    Ok((Extension(snapshot), Json(value)))
}

pub async fn client_launch_variants(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    let items = client_launch_variant_resources(&state, &session).map_err(ApiError::internal)?;
    client_page(
        &state,
        &snapshot,
        &format!("launches/{id}/variants"),
        items,
        &query,
    )
    .map(Json)
}

pub async fn client_launch_variant_detail(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    client_detail(
        client_launch_variant_resources(&state, &session).map_err(ApiError::internal)?,
        "launch-variant",
        &format!("{}/{variant}", session.id),
    )
}

pub async fn client_launch_decisions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    let items =
        client_launch_decision_resources(&state.store, &session).map_err(ApiError::internal)?;
    client_page(
        &state,
        &snapshot,
        &format!("launches/{id}/decisions"),
        items,
        &query,
    )
    .map(Json)
}

pub async fn client_launch_decision_detail(
    State(state): State<AppState>,
    AxumPath((id, decision)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    client_detail(
        client_launch_decision_resources(&state.store, &session).map_err(ApiError::internal)?,
        "launch-decision",
        &decision,
    )
}

pub async fn client_launch_approvals(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    let items = client_launch_approval_resources(&state, &session).map_err(ApiError::internal)?;
    client_page(
        &state,
        &snapshot,
        &format!("launches/{id}/approvals"),
        items,
        &query,
    )
    .map(Json)
}

pub async fn client_launch_approval_detail(
    State(state): State<AppState>,
    AxumPath((id, approval)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    client_detail(
        client_launch_approval_resources(&state, &session).map_err(ApiError::internal)?,
        "launch-approval",
        &format!("{}/{approval}", session.id),
    )
}

pub async fn client_history(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let requested_limit = query
        .limit
        .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
        .clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let (before_index, limit, offset, expires_at_unix_ms) = if let Some(encoded) = &query.cursor {
        let cursor = decode_client_cursor(encoded)?;
        if cursor.collection != "history"
            || cursor.snapshot.id != snapshot.id
            || cursor.snapshot.store_index != snapshot.store_index
            || cursor.history != query.history
            || cursor.person != query.person
            || cursor.actor != query.actor
            || cursor.owner_run != query.owner_run
            || cursor.status != query.status
            || cursor.native_only != query.native_only
            || query
                .limit
                .is_some_and(|limit| limit.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
        {
            return Err(client_page_expired(
                "the page cursor does not match this collection, snapshot, or filter",
            ));
        }
        if client_now_ms() > cursor.expires_at_unix_ms {
            return Err(client_page_expired("the page cursor expired"));
        }
        let before = cursor
            .before_index
            .ok_or_else(|| client_page_expired("the history cursor is malformed"))?;
        (
            before,
            cursor.limit,
            cursor.offset,
            cursor.expires_at_unix_ms,
        )
    } else {
        (
            snapshot.store_index.saturating_add(1),
            requested_limit,
            0,
            client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
        )
    };
    let store = state.store.clone();
    let page =
        blocking_store(move || store.claims_page(None, None, 0, Some(before_index), true, limit))
            .await?;
    let has_more = page.next_cursor.is_some();
    let items = page
        .claims
        .into_iter()
        .map(client_history_resource)
        .collect::<Vec<_>>();
    let next_cursor = page
        .next_cursor
        .map(|next| {
            encode_client_cursor(&ClientPageCursor {
                snapshot: snapshot.clone(),
                collection: "history".into(),
                offset: offset.saturating_add(items.len()),
                limit,
                history: query.history,
                person: query.person.clone(),
                actor: query.actor.clone(),
                owner_run: query.owner_run.clone(),
                status: query.status.clone(),
                owner: None,
                state: None,
                native_only: query.native_only,
                items_digest: String::new(),
                before_index: Some(next),
                after_key: None,
                expires_at_unix_ms,
            })
        })
        .transpose()?;
    let mut filters = BTreeMap::new();
    if query.history {
        filters.insert("history".into(), "all".into());
    }
    for (name, value) in [
        ("person", query.person.as_ref()),
        ("actor", query.actor.as_ref()),
        ("owner_run", query.owner_run.as_ref()),
        ("status", query.status.as_ref()),
    ] {
        if let Some(value) = value {
            filters.insert(name.into(), value.clone());
        }
    }
    if query.native_only {
        filters.insert("native_only".into(), "true".into());
    }
    Ok(Json(ClientResourcePage {
        kind: "page".into(),
        collection: "history".into(),
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
    }))
}

pub async fn client_history_detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let id = client_detail_id("history", &id);
    let index = id
        .strip_prefix("history/")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|index| *index > 0)
        .ok_or_else(|| ApiError::not_found(format!("history `{id}` does not exist")))?;
    let upper = index
        .checked_add(1)
        .ok_or_else(|| ApiError::not_found(format!("history `{id}` does not exist")))?;
    let store = state.store.clone();
    let page =
        blocking_store(move || store.claims_page(None, None, index - 1, Some(upper), true, 1))
            .await?;
    page.claims
        .into_iter()
        .next()
        .filter(|claim| claim.store_index == index)
        .map(client_history_resource)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("history `{id}` does not exist")))
}

/// [`blocking_store`] for an HTTP roster read: worker queue wait, then the attributed read.
async fn blocking_roster<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let submitted = crate::otel::export_enabled().then(Instant::now);
    blocking_store(move || roster_worker(true, submitted, None, operation)).await
}
