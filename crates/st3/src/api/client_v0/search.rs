use super::*;
use crate::conversation_search::{SearchEntry, SearchIndex};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const REFRESH: Duration = Duration::from_secs(30);
const OWNERS: usize = 4;
const TEXT_BYTES: usize = 16 * 1024 * 1024;
const ENTRIES: usize = 50_000;
static REFRESHERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Clone, Default, Deserialize)]
pub(in crate::api) struct SearchQuery {
    text: String,
    agent: Option<String>,
    since: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

struct IndexState {
    store: std::sync::Weak<Store>,
    index: SearchIndex,
    stamps: BTreeMap<String, String>,
    incomplete: Vec<String>,
    indexed_at: Option<String>,
    revision: String,
    checked: Instant,
    refreshing: bool,
    error: Option<String>,
}

type HeldIndex = Arc<Mutex<IndexState>>;
type IndexKey = (usize, String);
static INDEXES: OnceLock<Mutex<BTreeMap<IndexKey, HeldIndex>>> = OnceLock::new();

#[derive(Deserialize, Serialize)]
struct Cursor {
    revision: String,
    binding: String,
    timestamp: String,
    conversation: String,
    entry: String,
}

fn index_for(state: &AppState, session: &ClientSession) -> Result<HeldIndex, ApiError> {
    let key = (
        Arc::as_ptr(&state.store) as usize,
        session.authority_actor.clone(),
    );
    let mut indexes = INDEXES.get_or_init(Default::default).lock().unwrap();
    indexes.retain(|_, index| index.lock().unwrap().store.strong_count() > 0);
    if let Some(index) = indexes.get(&key) {
        return Ok(index.clone());
    }
    // A bounded per-daemon cache. Eviction also invalidates its cursors.
    if indexes.len() >= OWNERS {
        let oldest = indexes
            .iter()
            .min_by_key(|(_, index)| index.lock().unwrap().checked)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            indexes.remove(&oldest);
        }
    }
    let index = Arc::new(Mutex::new(IndexState {
        store: Arc::downgrade(&state.store),
        index: SearchIndex::new().map_err(ApiError::internal)?,
        stamps: BTreeMap::new(),
        incomplete: Vec::new(),
        indexed_at: None,
        revision: uuid::Uuid::now_v7().to_string(),
        checked: Instant::now(),
        refreshing: false,
        error: None,
    }));
    indexes.insert(key, index.clone());
    Ok(index)
}

fn searchable_text(value: &Value, output: &mut String) {
    match value {
        Value::String(text) => {
            output.push_str(text);
            output.push('\n');
        }
        Value::Array(values) => {
            for value in values {
                searchable_text(value, output);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                searchable_text(value, output);
            }
        }
        _ => {}
    }
}

// Retain newest entries globally, rather than spending the budget on whole sources.
// Keys normalize timestamp offsets and use stable identities to break ties.
struct RecentEntries {
    entries: BTreeMap<(String, String, String), (String, SearchEntry)>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
    omitted: BTreeSet<String>,
    cutoff: Option<(String, String, String)>,
}

impl RecentEntries {
    fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            bytes: 0,
            max_bytes,
            max_entries,
            omitted: BTreeSet::new(),
            cutoff: None,
        }
    }

    fn advance_cutoff(&mut self, cutoff: (String, String, String)) {
        if self.cutoff.as_ref().is_some_and(|old| old >= &cutoff) {
            return;
        }
        while self
            .entries
            .first_key_value()
            .is_some_and(|(key, _)| key <= &cutoff)
        {
            let (_, (source, entry)) = self.entries.pop_first().unwrap();
            self.bytes -= entry.text.len();
            self.omitted.insert(source);
        }
        self.cutoff = Some(cutoff);
    }

    fn insert(&mut self, source: &str, mut entry: SearchEntry) -> anyhow::Result<()> {
        entry.timestamp = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)?
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        if entry.text.len() > self.max_bytes || self.max_entries == 0 {
            self.omitted.insert(source.to_owned());
            return Ok(());
        }
        let key = (
            entry.timestamp.clone(),
            entry.conversation_id.clone(),
            entry.entry_id.clone(),
        );
        if self.cutoff.as_ref().is_some_and(|cutoff| &key <= cutoff) {
            self.omitted.insert(source.to_owned());
            return Ok(());
        }
        if let Some((_, previous)) = self.entries.remove(&key) {
            self.bytes -= previous.text.len();
        }
        self.bytes += entry.text.len();
        self.entries.insert(key, (source.to_owned(), entry));
        while self.bytes > self.max_bytes || self.entries.len() > self.max_entries {
            let (key, (source, entry)) = self.entries.pop_first().expect("over budget");
            self.bytes -= entry.text.len();
            self.cutoff = Some(key);
            self.omitted.insert(source);
        }
        Ok(())
    }
}

fn source_notice(id: &str, item: &Value) -> String {
    let kind = item["type"].as_str().unwrap_or("error");
    let body = &item["body"];
    let code = body["code"].as_str().unwrap_or(kind);
    let message = body["message"].as_str().or_else(|| body["reason"].as_str());
    // Do not include transcript content, tool arguments, paths or raw malformed records.
    let message: String = message
        .unwrap_or("source did not provide a reason")
        .chars()
        .take(256)
        .collect();
    format!("{id}: {kind}: {code}: {message}")
}

fn refresh(
    state: &AppState,
    session: &ClientSession,
    held: &HeldIndex,
    runtime: &tokio::runtime::Handle,
) -> anyhow::Result<()> {
    refresh_with_budget(state, session, held, runtime, TEXT_BYTES, ENTRIES)
}

fn refresh_with_budget(
    state: &AppState,
    session: &ClientSession,
    held: &HeldIndex,
    runtime: &tokio::runtime::Handle,
    max_bytes: usize,
    max_entries: usize,
) -> anyhow::Result<()> {
    let snapshot = new_client_snapshot(state);
    let old = held.lock().unwrap().stamps.clone();
    let mut recent = RecentEntries::new(max_bytes, max_entries);
    let mut incomplete = Vec::new();
    // Messages use the same private from/to rule as the messages projection.
    for message in state
        .store
        .conversation_search_messages(&session.authority_actor, snapshot.store_index)?
    {
        if message.from != session.authority_actor && message.to != session.authority_actor {
            continue;
        }
        let claim = state
            .store
            .claims_for(&message.subject, Some("message.sent"))?
            .into_iter()
            .min_by_key(|claim| claim.store_index);
        let Some(claim) = claim else {
            continue;
        };
        let agent = [&message.from, &message.to]
            .into_iter()
            .find(|party| party.starts_with("agent/"))
            .cloned();
        recent.insert(
            "mail",
            SearchEntry {
                conversation_id: message.subject.clone(),
                entry_id: message.subject,
                agent_id: agent,
                timestamp: client_timestamp(claim.accepted_at_unix_ms),
                entry_type: "message".into(),
                text: format!("{}\n{}", message.title.unwrap_or_default(), message.content),
            },
        )?;
    }
    if crate::external_sessions::discover(state.native_session_home.as_deref(), true)?
        .sessions
        .len()
        >= crate::external_sessions::MAX_EXPOSED_HISTORY
    {
        incomplete.push("native inventory: exposed history limit reached".into());
    }
    for resource in super::super::client_session_resources(
        &state.store,
        true,
        &snapshot.created_at,
        snapshot.store_index,
        state.native_session_home.as_deref(),
        false,
    )? {
        let Some(id) = resource["id"].as_str() else {
            continue;
        };
        let owner = resource["owner_id"].as_str().unwrap_or_default();
        let remote = match conversation_owner_host(state, session, id) {
            Ok(remote) => remote,
            Err(error) => {
                incomplete.push(format!("{id}: {}: {}", error.code, error.message));
                continue;
            }
        };
        let mut query = ClientListQuery {
            limit: Some(200),
            ..Default::default()
        };
        // A failed source must not evict usable entries from other sources.
        let mut source = RecentEntries::new(max_bytes, max_entries);
        let mut unavailable = false;
        loop {
            let read = if let Some(remote) = &remote {
                runtime
                    .block_on(state.client_relay.as_ref().expect("checked relay").read(
                        remote,
                        &crate::peer::ClientReadRequest {
                            authority_actor: session.authority_actor.clone(),
                            relay: None,
                            request: crate::peer::ClientReadOperation::Timeline {
                                session_id: id.to_owned(),
                                limit: 200,
                                cursor: query.cursor.clone(),
                            },
                        },
                    ))
                    .map_err(|error| remote_read_error(remote, error))
            } else {
                timeline_value(state, &snapshot, session, id, &query).map(|Json(page)| page)
            };
            let page = match read {
                Ok(page) => page,
                Err(error) => {
                    incomplete.push(format!("{id}: {}: {}", error.code, error.message));
                    unavailable = true;
                    break;
                }
            };
            for item in page["items"].as_array().into_iter().flatten() {
                let kind = item["type"].as_str().unwrap_or_default();
                if matches!(kind, "truncation" | "redaction" | "error") {
                    incomplete.push(source_notice(id, item));
                }
                // Small Talk messages already have their canonical mailbox hit.
                if !matches!(kind, "content" | "tool_call" | "tool_result") {
                    continue;
                }
                let mut text = String::new();
                searchable_text(&item["body"], &mut text);
                if text.trim().is_empty() {
                    continue;
                }
                let entry = SearchEntry {
                    conversation_id: id.to_owned(),
                    entry_id: item["id"].as_str().unwrap_or_default().into(),
                    agent_id: owner.starts_with("agent/").then(|| owner.to_owned()),
                    timestamp: item["timestamp"].as_str().unwrap_or_default().into(),
                    entry_type: kind.into(),
                    text,
                };
                if let Err(error) = source.insert(id, entry) {
                    incomplete.push(format!("{id}: invalid entry timestamp: {error}"));
                }
            }
            query.cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
            if query.cursor.is_none() {
                break;
            }
        }
        if !unavailable {
            recent.omitted.extend(source.omitted);
            if let Some(cutoff) = source.cutoff {
                recent.advance_cutoff(cutoff);
            }
            for (_, (_, entry)) in source.entries {
                recent.insert(id, entry)?;
            }
        }
    }
    incomplete.extend(recent.omitted.into_iter().map(|id| {
        format!("{id}: search index budget reached (newest entries retained across sources)")
    }));
    incomplete.sort();
    incomplete.dedup();
    let mut selected: BTreeMap<String, Vec<SearchEntry>> = BTreeMap::new();
    for (_, (source, entry)) in recent.entries {
        selected.entry(source).or_default().push(entry);
    }
    // Fingerprint selected text so unchanged refreshes preserve pagination. Every source
    // participates in selection again: a formerly excluded conversation may now be newest.
    let stamps: BTreeMap<String, String> = selected
        .iter()
        .map(|(source, entries)| {
            Ok((
                source.clone(),
                hex::encode(Sha256::digest(serde_json::to_vec(entries)?)),
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    let mut index = held.lock().unwrap();
    for source in old.keys().filter(|source| !stamps.contains_key(*source)) {
        index.index.remove(source)?;
    }
    for (source, entries) in selected {
        if old.get(&source) != stamps.get(&source) {
            index.index.replace(&source, &entries)?;
        }
    }
    if index.stamps != stamps {
        index.revision = uuid::Uuid::now_v7().to_string();
    }
    index.stamps = stamps;
    index.incomplete = incomplete;
    index.indexed_at = Some(client_timestamp(client_now_ms()));
    Ok(())
}

pub(in crate::api) async fn search(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(mut query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    if !session.authority_actor.starts_with("person/") {
        return Err(forbidden(
            "conversation search requires an authenticated person",
        ));
    }
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit)
        || query.text.trim().is_empty()
        || query.text.len() > 512
        || !query.text.chars().any(char::is_alphanumeric)
    {
        return Err(validation(
            "search needs a word, at most 512 bytes, and a limit of 1 through 200",
        ));
    }
    if let Some(since) = &query.since {
        query.since = Some(
            chrono::DateTime::parse_from_rfc3339(since)
                .map_err(|_| validation("since must be an RFC3339 timestamp"))?
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        );
    }
    if query
        .agent
        .as_deref()
        .is_some_and(|agent| !agent.starts_with("agent/"))
    {
        return Err(validation("agent must be an agent subject"));
    }
    let binding = hex::encode(Sha256::digest(
        serde_json::to_vec(&(
            &session.authority_actor,
            &query.text,
            &query.agent,
            &query.since,
            limit,
        ))
        .map_err(ApiError::internal)?,
    ));
    let cursor: Option<Cursor> = query
        .cursor
        .as_ref()
        .map(|cursor| {
            if cursor.len() > 2048 {
                return Err(validation("search cursor is too long"));
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| validation("invalid search cursor"))?;
            serde_json::from_slice(&bytes).map_err(|_| validation("invalid search cursor"))
        })
        .transpose()?;
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.binding != binding)
    {
        return Err(validation(
            "search cursor does not belong to this reader and query",
        ));
    }
    if cursor
        .as_ref()
        .is_some_and(|cursor| chrono::DateTime::parse_from_rfc3339(&cursor.timestamp).is_err())
    {
        return Err(validation("invalid search cursor timestamp"));
    }
    let held = index_for(&state, &session)?;
    {
        let mut index = held.lock().unwrap();
        if cursor.is_none()
            && !index.refreshing
            && (index.indexed_at.is_none() || index.checked.elapsed() >= REFRESH)
            && let Ok(permit) = REFRESHERS.try_acquire()
        {
            index.refreshing = true;
            let state = state.clone();
            let session = session.clone();
            let held = held.clone();
            let runtime = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                let _permit = permit;
                let result = refresh(&state, &session, &held, &runtime);
                let mut index = held.lock().unwrap();
                index.refreshing = false;
                index.checked = Instant::now();
                index.error = result.err().map(|error| error.to_string());
            });
        }
    }
    // The first lookup gives a small inventory time to become ready. Large inventories
    // continue in the background; a retry uses that work instead of starting another scan.
    let start = Instant::now();
    while held.lock().unwrap().indexed_at.is_none() && start.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::task::spawn_blocking(move || search_page(&state, &query, cursor, held, binding, limit))
        .await
        .map_err(ApiError::internal)?
}

fn search_page(
    state: &AppState,
    query: &SearchQuery,
    cursor: Option<Cursor>,
    held: HeldIndex,
    binding: String,
    limit: usize,
) -> Result<Json<Value>, ApiError> {
    let index = held.lock().unwrap();
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.revision != index.revision)
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "cursor-gap".into(),
            message: "conversation search index changed; start the search again".into(),
            details: Default::default(),
        });
    }
    let Some(indexed_at) = &index.indexed_at else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: if index.error.is_some() {
                "search-index-failed"
            } else {
                "search-index-building"
            }
            .into(),
            message: index.error.clone().unwrap_or_else(|| {
                "conversation search index is being built; retry shortly".into()
            }),
            details: Box::new(serde_json::Map::from_iter([(
                "retry_after_ms".into(),
                json!(1000),
            )])),
        });
    };
    let mut hits = index
        .index
        .search(
            &query.text,
            query.agent.as_deref(),
            query.since.as_deref(),
            cursor.as_ref().map(|cursor| {
                (
                    cursor.timestamp.as_str(),
                    cursor.conversation.as_str(),
                    cursor.entry.as_str(),
                )
            }),
            limit + 1,
        )
        .map_err(ApiError::internal)?;
    let has_more = hits.len() > limit;
    hits.truncate(limit);
    let next = if has_more {
        hits.last().map(|last| {
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Cursor {
                    revision: index.revision.clone(),
                    binding,
                    timestamp: last.timestamp.clone(),
                    conversation: last.conversation_id.clone(),
                    entry: last.entry_id.clone(),
                })
                .expect("serializable cursor"),
            )
        })
    } else {
        None
    };
    let mut incomplete = index.incomplete.clone();
    if let Some(error) = &index.error {
        incomplete.push(format!("refresh failed: {error}"));
    }
    if incomplete.len() > 128 {
        let omitted = incomplete.len() - 127;
        incomplete.truncate(127);
        incomplete.push(format!("{omitted} additional incomplete sources"));
    }
    Ok(Json(json!({ "kind": "conversation-search", "items": hits,
        "indexed_at": indexed_at, "host_id": client_host_id(&state.node),
        "incomplete_sources": incomplete, "refreshing": index.refreshing,
        "page": { "limit": limit, "has_more": has_more, "next_cursor": next, "cursor_expires_at": null } })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(source: &str, second: u32, text: &str) -> SearchEntry {
        SearchEntry {
            conversation_id: source.into(),
            entry_id: format!("entry/{second}"),
            agent_id: None,
            timestamp: format!("2026-10-03T00:00:{second:02}Z"),
            entry_type: "content".into(),
            text: text.into(),
        }
    }

    #[test]
    fn budget_retains_global_newest_independently_of_source_order() {
        for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [0, 2, 1]] {
            let entries = [
                candidate("a", 1, "older"),
                candidate("b", 2, "middle!!"),
                candidate("c", 3, "newer"),
            ];
            let mut recent = RecentEntries::new(10, 10);
            for i in order {
                recent
                    .insert(&entries[i].conversation_id, entries[i].clone())
                    .unwrap();
            }
            assert_eq!(recent.entries.len(), 1);
            assert_eq!(recent.entries.values().next().unwrap().1.text, "newer");
            assert!(recent.bytes <= 10);
            assert!(recent.omitted.contains("a") && recent.omitted.contains("b"));
        }
        let mut recent = RecentEntries::new(100, 2);
        recent
            .insert("first", candidate("first", 1, "old"))
            .unwrap();
        recent
            .insert("first", candidate("first", 2, "middle"))
            .unwrap();
        recent
            .insert("last", candidate("last", 3, "latest"))
            .unwrap();
        assert_eq!(recent.entries.len(), 2);
        assert!(recent.entries.values().any(|(source, _)| source == "last"));
        assert!(recent.omitted.contains("first"));
    }

    #[test]
    fn source_errors_explain_the_reason_without_indexing_it() {
        let item = json!({"type":"error", "body":{"code":"native-line-unreadable",
            "message":"st skipped a codex transcript line that is not valid JSON",
            "details":{"raw":"private raw line"}}});
        let notice = source_notice("session/test", &item);
        assert!(notice.contains("native-line-unreadable"));
        assert!(notice.contains("not valid JSON"));
        assert!(!notice.contains("private raw line"));
    }

    #[test]
    fn cold_search_distinguishes_building_from_failed_index() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        let session = ClientSession::local(Some("person/cold")).unwrap();
        let held = index_for(&state, &session).unwrap();
        let building = search_page(
            &state,
            &query("orchid", 50),
            None,
            held.clone(),
            "binding".into(),
            50,
        )
        .unwrap_err();
        assert_eq!(building.code, "search-index-building");
        let envelope = crate::api::client_error_envelope(
            building.status,
            &json!({"code":building.code, "message":building.message}),
            "search-test",
        );
        assert_eq!(envelope["code"], "search-index-building");
        assert_eq!(envelope["retryable"], true);
        assert_eq!(building.details["retry_after_ms"], 1000);
        held.lock().unwrap().error = Some("inventory failed".into());
        let failed = search_page(
            &state,
            &query("orchid", 50),
            None,
            held,
            "binding".into(),
            50,
        )
        .unwrap_err();
        assert_eq!(failed.code, "search-index-failed");
        assert!(failed.message.contains("inventory failed"));
    }

    fn claim(state: &AppState, subject: &str, kind: &str, mut fields: Value) {
        if kind == "message.sent" {
            fields["status"] = json!("sent");
        }
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some("person/alex".into()),
                fields: fields.as_object().unwrap().clone().into_iter().collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    fn query(text: &str, limit: usize) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            limit: Some(limit),
            ..Default::default()
        }
    }
    async fn read(state: &AppState, person: &str, query: SearchQuery) -> Result<Value, ApiError> {
        search(
            State(state.clone()),
            Extension(ClientSession::local(Some(person)).unwrap()),
            Query(query),
        )
        .await
        .map(|Json(value)| value)
    }
    async fn rebuild(state: &AppState, person: &str) {
        let session = ClientSession::local(Some(person)).unwrap();
        let held = index_for(state, &session).unwrap();
        let runtime = tokio::runtime::Handle::current();
        let state = state.clone();
        tokio::task::spawn_blocking(move || refresh(&state, &session, &held, &runtime))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_budget_includes_later_source_and_preserves_unchanged_cursor_revision() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        for agent in ["agent/a", "agent/z"] {
            claim(
                &state,
                agent,
                "runtime.observed",
                json!({"status":"running",
                "runtime_id":agent, "incarnation_id":"i1", "terminal":false}),
            );
            claim(
                &state,
                agent,
                "harness.timeline",
                json!({"operation":"append",
                "entry_id":format!("timeline-entry/{agent}"), "revision":1, "role":"assistant",
                "entry_type":"content", "final":true, "body":{"text":"orchid note"},
                "incarnation_id":"i1", "sequence":1, "driver":"test"}),
            );
        }
        let session = ClientSession::local(Some("person/alex")).unwrap();
        let held = index_for(&state, &session).unwrap();
        let runtime = tokio::runtime::Handle::current();
        let build_state = state.clone();
        let build_held = held.clone();
        tokio::task::spawn_blocking(move || {
            refresh_with_budget(&build_state, &session, &build_held, &runtime, 100, 1).unwrap();
            let revision = build_held.lock().unwrap().revision.clone();
            refresh_with_budget(&build_state, &session, &build_held, &runtime, 100, 1).unwrap();
            assert_eq!(revision, build_held.lock().unwrap().revision);
        })
        .await
        .unwrap();
        let page = search_page(
            &state,
            &query("orchid", 50),
            None,
            held,
            "binding".into(),
            50,
        )
        .unwrap()
        .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["agent_id"], "agent/z");
        assert!(
            page["incomplete_sources"]
                .to_string()
                .contains("search index budget reached")
        );
    }

    #[tokio::test]
    async fn search_privacy_scopes_cursor_binding_and_normalized_replacements() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        claim(
            &state,
            "message/incoming",
            "message.sent",
            json!({"from":"agent/sender", "to":"person/alex", "content":"orchid incoming"}),
        );
        claim(
            &state,
            "message/outgoing",
            "message.sent",
            json!({"from":"person/alex", "to":"agent/sender", "content":"orchid outgoing"}),
        );
        claim(
            &state,
            "message/private",
            "message.sent",
            json!({"from":"person/blair", "to":"person/robin", "content":"orchid private"}),
        );
        claim(
            &state,
            "agent/scribe",
            "runtime.observed",
            json!({"status":"running", "runtime_id":"scribe", "incarnation_id":"scribe:i1", "terminal":false}),
        );
        let timeline = |revision: u64, operation: &str, text: &str| {
            claim(
                &state,
                "agent/scribe",
                "harness.timeline",
                json!({"operation":operation,
                "entry_id":"timeline-entry/note", "revision":revision, "role":"assistant",
                "entry_type":"content", "final":revision>1, "body":{"media_type":"text/plain", "text":text},
                "incarnation_id":"scribe:i1", "sequence":revision, "driver":"test"}),
            );
        };
        timeline(1, "append", "orchid draft");
        let first = read(&state, "person/alex", query("orchid", 1))
            .await
            .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 1);
        let cursor = first["page"]["next_cursor"].as_str().unwrap().to_owned();
        let mut next = query("orchid", 1);
        next.cursor = Some(cursor.clone());
        let second = read(&state, "person/alex", next.clone()).await.unwrap();
        assert_ne!(
            first["items"][0]["entry_id"],
            second["items"][0]["entry_id"]
        );
        assert_eq!(
            read(&state, "person/blair", next.clone())
                .await
                .unwrap_err()
                .code,
            "validation-failed"
        );
        next.text = "draft".into();
        assert_eq!(
            read(&state, "person/alex", next).await.unwrap_err().code,
            "validation-failed"
        );
        let all = read(&state, "person/alex", query("orchid", 200))
            .await
            .unwrap();
        assert_eq!(all["items"].as_array().unwrap().len(), 3);
        assert!(!all["items"].to_string().contains("private"));
        let mut filtered = query("orchid", 200);
        filtered.agent = Some("agent/scribe".into());
        let transcript = read(&state, "person/alex", filtered).await.unwrap();
        assert_eq!(transcript["items"][0]["entry_id"], "timeline-entry/note");
        assert_eq!(transcript["items"].as_array().unwrap().len(), 1);
        timeline(2, "finalize", "lily final");
        rebuild(&state, "person/alex").await;
        assert!(
            read(&state, "person/alex", query("draft", 200))
                .await
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            read(&state, "person/alex", query("lily", 200))
                .await
                .unwrap()["items"][0]["entry_id"],
            "timeline-entry/note"
        );
        let mut stale = query("orchid", 1);
        stale.cursor = Some(cursor);
        assert_eq!(
            read(&state, "person/alex", stale).await.unwrap_err().code,
            "cursor-gap"
        );
        let mut future = query("orchid", 200);
        future.since = Some("2999-01-01T00:00:00+01:00".into());
        assert!(
            read(&state, "person/alex", future).await.unwrap()["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        claim(
            &state,
            "agent/scribe",
            "runtime.observed",
            json!({"status":"running", "runtime_id":"scribe", "incarnation_id":"scribe:i2", "terminal":false}),
        );
        claim(
            &state,
            "agent/scribe",
            "harness.timeline",
            json!({"operation":"finalize",
            "entry_id":"timeline-entry/broken", "revision":2, "role":"assistant", "entry_type":"content",
            "final":true, "body":{"text":"untrusted result"}, "incarnation_id":"scribe:i2", "sequence":2, "driver":"test"}),
        );
        rebuild(&state, "person/alex").await;
        let unavailable = read(&state, "person/alex", query("lily", 200))
            .await
            .unwrap();
        assert!(unavailable["items"].as_array().unwrap().is_empty());
        assert!(
            unavailable["incomplete_sources"]
                .to_string()
                .contains("timeline-history-incomplete")
        );
        let page = read(&state, "person/alex", query("orchid", 1))
            .await
            .unwrap();
        let mut evicted = query("orchid", 1);
        evicted.cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
        assert!(evicted.cursor.is_some());
        INDEXES
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(&state.store) as usize, "person/alex".to_owned()));
        assert_eq!(
            read(&state, "person/alex", evicted).await.unwrap_err().code,
            "cursor-gap"
        );
        let mut session = ClientSession::local(Some("person/alex")).unwrap();
        session.scopes.remove("read.projections");
        assert_eq!(
            search(
                State(state),
                Extension(session),
                Query(query("orchid", 200))
            )
            .await
            .unwrap_err()
            .code,
            "forbidden"
        );
    }

    #[tokio::test]
    async fn search_indexes_native_transcript_text_with_the_normalized_entry_identity() {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::api::tests::state(root.path());
        let home = root.path().join("home");
        let transcript = home.join(".codex/sessions/2026/10/02/search.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, format!("{}\n{}\n",
            json!({"type":"session_meta", "timestamp":"2026-10-02T10:00:00Z", "payload":{"id":"native-search", "cwd":"/tmp"}}),
            json!({"type":"response_item", "timestamp":"2026-10-02T10:01:00Z", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"native orchid note"}]}}))).unwrap();
        state.native_session_home = Some(home.clone());
        let native = crate::external_sessions::discover_fresh(Some(&home), true)
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.native_id == "native-search")
            .unwrap();
        let expected = crate::external_sessions::normalized_timeline(&native)
            .unwrap()
            .into_iter()
            .find(|entry| entry["type"] == "content")
            .unwrap();
        let hits = read(&state, "person/alex", query("native orchid", 200))
            .await
            .unwrap();
        assert_eq!(hits["items"].as_array().unwrap().len(), 1, "{hits}");
        assert_eq!(hits["items"][0]["entry_id"], expected["id"]);
        assert_eq!(hits["items"][0]["conversation_id"], native.id);
    }
}
