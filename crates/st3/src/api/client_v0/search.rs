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
    sizes: BTreeMap<String, (usize, usize)>,
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
        sizes: BTreeMap::new(),
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

fn refresh(
    state: &AppState,
    session: &ClientSession,
    held: &HeldIndex,
    runtime: &tokio::runtime::Handle,
) -> anyhow::Result<()> {
    let snapshot = new_client_snapshot(state);
    let old = held.lock().unwrap().stamps.clone();
    let old_sizes = held.lock().unwrap().sizes.clone();
    let mut sizes = BTreeMap::new();
    let mut budget = TEXT_BYTES;
    let mut slots = ENTRIES;
    let mut stamps = BTreeMap::new();
    let mut changed = Vec::new();
    let mut incomplete = Vec::new();
    // Messages are immutable text. Their access rule is the same private from/to rule as
    // the messages projection, including archived messages and the person's sent mail.
    let mail_stamp = state
        .store
        .conversation_search_mail_stamp(&session.authority_actor, snapshot.store_index)?;
    stamps.insert("mail".into(), mail_stamp.clone());
    if old.get("mail") != Some(&mail_stamp) {
        let mut entries = Vec::new();
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
            let text = format!("{}\n{}", message.title.unwrap_or_default(), message.content);
            if text.len() > budget || slots == 0 {
                incomplete.push("mail: search index budget reached".into());
                continue;
            }
            budget -= text.len();
            slots -= 1;
            entries.push(SearchEntry {
                conversation_id: message.subject.clone(),
                entry_id: message.subject,
                agent_id: agent,
                timestamp: client_timestamp(claim.accepted_at_unix_ms),
                entry_type: "message".into(),
                text,
            });
        }
        sizes.insert("mail".into(), (TEXT_BYTES - budget, ENTRIES - slots));
        changed.push(("mail".to_owned(), entries));
    } else {
        let size = old_sizes.get("mail").copied().unwrap_or_default();
        budget = budget.saturating_sub(size.0);
        slots = slots.saturating_sub(size.1);
        sizes.insert("mail".into(), size);
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
                incomplete.push(format!("{id}: {}", error.message));
                continue;
            }
        };
        let mut stamp = resource["revision"].to_string();
        if !owner.starts_with("agent/") {
            stamp.push_str(&resource["updated_at"].to_string());
        }
        if owner.starts_with("agent/") && remote.is_none() {
            // Joined Small Talk message entries also change this normalized conversation.
            stamp.push_str(
                &state
                    .store
                    .conversation_search_mail_stamp(owner, snapshot.store_index)?,
            );
            stamp.push_str(&state.store.conversation_search_timeline_stamp(
                owner,
                resource["runtime_incarnation"].as_str().unwrap_or_default(),
                snapshot.store_index,
            )?);
            // Driver claims can be quiet while a native transcript grows.
            if let Some(transcript) = managed_transcript(
                state,
                owner,
                resource["runtime_incarnation"].as_str().unwrap_or_default(),
            )
            .map_err(|error| anyhow::anyhow!(error.message))?
            {
                stamp.push_str(&transcript.driver);
                match transcript.transcript {
                    Ok(transcript) => {
                        stamp.push_str(&transcript.transcript.to_string_lossy());
                        if let Ok(metadata) = fs::metadata(&transcript.transcript) {
                            stamp.push_str(&format!(
                                ":{}:{:?}",
                                metadata.len(),
                                metadata.modified().ok()
                            ));
                        }
                    }
                    Err(missing) => stamp.push_str(&missing.reason),
                }
            }
        }
        // The remote transcript may grow without a replicated observation. Recheck it at
        // each demand refresh rather than pretending its local observation is current.
        if remote.is_some() {
            stamp.push_str(&snapshot.id);
        }
        stamps.insert(id.to_owned(), stamp.clone());
        if budget == 0 || slots == 0 {
            incomplete.push(format!("{id}: search index budget reached"));
            stamps.remove(id);
            continue;
        }
        if remote.is_none() && old.get(id) == Some(&stamp) {
            let size = old_sizes.get(id).copied().unwrap_or_default();
            if size.0 <= budget && size.1 <= slots {
                budget -= size.0;
                slots -= size.1;
                sizes.insert(id.to_owned(), size);
                continue;
            }
        }
        let mut query = ClientListQuery {
            limit: Some(200),
            ..Default::default()
        };
        let mut entries = Vec::new();
        let mut unavailable = false;
        let start_budget = budget;
        let start_slots = slots;
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
            for item in page["items"].as_array().into_iter().flatten().rev() {
                let kind = item["type"].as_str().unwrap_or_default();
                if matches!(kind, "truncation" | "redaction" | "error") {
                    incomplete.push(format!("{id}: {kind}"));
                }
                // A Small Talk message has its canonical mailbox hit; avoid indexing its
                // duplicated title and content again through each seat's transcript.
                if !matches!(kind, "content" | "tool_call" | "tool_result") {
                    continue;
                }
                let mut text = String::new();
                searchable_text(&item["body"], &mut text);
                if text.trim().is_empty() {
                    continue;
                }
                if text.len() > budget || slots == 0 {
                    incomplete.push(format!("{id}: search index budget reached"));
                    continue;
                }
                budget -= text.len();
                slots -= 1;
                entries.push(SearchEntry {
                    conversation_id: id.to_owned(),
                    entry_id: item["id"].as_str().unwrap_or_default().into(),
                    agent_id: owner.starts_with("agent/").then(|| owner.to_owned()),
                    timestamp: item["timestamp"].as_str().unwrap_or_default().into(),
                    entry_type: kind.into(),
                    text,
                });
            }
            query.cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
            if query.cursor.is_some() && (budget == 0 || slots == 0) {
                incomplete.push(format!("{id}: search index budget reached"));
                break;
            }
            if query.cursor.is_none() {
                break;
            }
        }
        // Never leave old searchable text behind after a source becomes unavailable.
        if unavailable {
            entries.clear();
            stamps.remove(id);
            budget = start_budget;
            slots = start_slots;
        }
        sizes.insert(id.to_owned(), (start_budget - budget, start_slots - slots));
        changed.push((id.to_owned(), entries));
    }
    let mut index = held.lock().unwrap();
    for source in old.keys().filter(|source| !stamps.contains_key(*source)) {
        index.index.remove(source)?;
    }
    let updated = !changed.is_empty();
    for (source, entries) in changed {
        index.index.replace(&source, &entries)?;
    }
    incomplete.sort();
    incomplete.dedup();
    // Unchanged truncated sources retain their warning too.
    for warning in &index.incomplete {
        if warning
            .split_once(": ")
            .is_some_and(|(source, _)| old.get(source) == stamps.get(source))
        {
            incomplete.push(warning.clone());
        }
    }
    incomplete.sort();
    incomplete.dedup();
    if updated || index.stamps != stamps {
        index.revision = uuid::Uuid::now_v7().to_string();
    }
    index.stamps = stamps;
    index.sizes = sizes;
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
    // A cold index has explicit readiness. The refresh runs in the background, and the
    // caller can retry without holding an HTTP request for an unbounded inventory build.
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
            code: if index.refreshing || index.error.is_none() {
                "index-building"
            } else {
                "remote-unavailable"
            }
            .into(),
            message: index.error.clone().unwrap_or_else(|| {
                "conversation search index is being built; retry shortly".into()
            }),
            details: Default::default(),
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
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = search(
                State(state.clone()),
                Extension(ClientSession::local(Some(person)).unwrap()),
                Query(query.clone()),
            )
            .await
            .map(|Json(value)| value);
            match result {
                Err(error) if error.code == "index-building" && Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => return result,
            }
        }
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
