use super::*;
use st3_client::{HarnessInventory, HarnessInventoryItem, HarnessInventoryQuery, PageInfo};

const CURSOR_TTL_MS: u128 = 60_000;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    actor: String,
    agent: String,
    query: HarnessInventoryQuery,
    revision: String,
    after: String,
    expires_at: u128,
}

pub(in crate::api) async fn read(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(agent): AxumPath<String>,
    Query(mut query): Query<HarnessInventoryQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    if !agent.starts_with("agent/") || agent.len() <= 6 || agent.len() > 512 {
        return Err(validation("inventory requires an agent subject"));
    }
    if !matches!(query.collection.as_str(), "files" | "skills" | "skill-commands" | "slash-commands")
        || query.directory.len() > 1024 || query.prefix.len() > 160
        || query.prefix.chars().any(char::is_control)
        || query.cursor.as_ref().is_some_and(|cursor| cursor.len() > 8192)
        || query.limit.is_some_and(|limit| !(1..=200).contains(&limit)) {
        return Err(validation("invalid inventory collection, filter, cursor or page bound"));
    }
    if query.collection != "files" && !query.directory.is_empty() {
        return Err(validation("directory applies only to file inventory"));
    }
    query.limit = Some(query.limit.unwrap_or(50));
    let live = remote_terminal_live_session(&state, &agent, &query.runtime_incarnation)?;
    if live.owner_host_id != query.owner_host_id
        || managed_session_id(&agent, &live.incarnation_id) != query.session_id {
        return Err(stale("the inventory owner or managed session fence is stale"));
    }
    if live.owner_host_id != client_host_id(&state.node) {
        if !acting_party(&session) { return Err(forbidden("remote inventory requires a concrete actor")); }
        let relay = state.client_relay.as_ref().ok_or_else(|| remote_unavailable(&live.owner_host_id))?;
        return relay.read(&live.owner_host_id, &crate::peer::ClientReadRequest {
            authority_actor: session.authority_actor, relay: None,
            request: crate::peer::ClientReadOperation::HarnessInventory { agent_id: agent, query },
        }).await.map(Json).map_err(|error| remote_read_error(&live.owner_host_id, error));
    }
    let reader = state.clone();
    let actor = session.authority_actor;
    tokio::task::spawn_blocking(move || local_read(&reader, &actor, &agent, query))
        .await.map_err(ApiError::internal)?.map(Json)
}

fn local_snapshot(state: &AppState, agent: &str, query: &HarnessInventoryQuery)
    -> Result<Option<st_drivers::harness_inventory::Snapshot>, ApiError> {
    let live = live_session(state, agent, Some(&query.runtime_incarnation))?;
    if live.owner_host_id != query.owner_host_id || managed_session_id(agent, &live.incarnation_id) != query.session_id {
        return Err(stale("the inventory owner or managed session fence is stale"));
    }
    if live.driver.as_deref() != Some("omp") { return Ok(None) }
    let root = state.state_dir.join("drivers").join(&hex::encode(Sha256::digest(agent.as_bytes()))[..24]);
    let identity = agent.trim_start_matches("agent/");
    let observations = root.join("observations");
    if !st_drivers::harness_events::enabled(&observations) { return Ok(None) }
    let bytes = st_drivers::harness_events::read_snapshot(&observations, "harness-inventory")
        .map_err(|_| inventory_unavailable("native inventory evidence is unreadable"))?;
    let Some(bytes) = bytes else { return Ok(None) };
    let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| inventory_unavailable("native inventory evidence is invalid"))?;
    let provider = value["incarnation"].as_str()
        .ok_or_else(|| inventory_unavailable("native inventory has no authenticated provider"))?;
    let owner = st_drivers::harness_events::read_snapshot(&observations, "harness-state")
        .map_err(|_| inventory_unavailable("native provider ownership is unreadable"))?
        .ok_or_else(|| inventory_unavailable("native provider ownership is absent"))?;
    let owner: Value = serde_json::from_slice(&owner)
        .map_err(|_| inventory_unavailable("native provider ownership is invalid"))?;
    if owner["incarnation"].as_str() != Some(provider) {
        return Err(stale("native inventory evidence belongs to a superseded provider"));
    }
    let native = st_drivers::omp_session::bound_native_session(
        &root.join("sessions/omp"), identity, identity, provider,
    ).map_err(|_| inventory_unavailable("native session binding is unreadable"))?;
    if native.as_deref() != Some(query.native_session_id.as_str()) {
        return Err(stale("the inventory native session fence is stale or not ready"));
    }
    if let Some(fields) = value.as_object_mut() { fields.remove("incarnation"); }
    let snapshot: st_drivers::harness_inventory::Snapshot = serde_json::from_value(value)
        .map_err(|_| inventory_unavailable("native inventory evidence is invalid"))?;
    if snapshot.incarnation_id != query.runtime_incarnation || snapshot.session_id != query.native_session_id {
        return Err(stale("native inventory evidence belongs to a previous session"));
    }
    Ok(Some(snapshot))
}

fn inventory_unavailable(message: &str) -> ApiError {
    ApiError { status: StatusCode::SERVICE_UNAVAILABLE, code: "inventory-unavailable".into(),
        message: message.into(), details: Box::default() }
}

fn local_read(state: &AppState, actor: &str, agent: &str, mut query: HarnessInventoryQuery) -> Result<Value, ApiError> {
    let snapshot = local_snapshot(state, agent, &query)?;
    let encoded = query.cursor.take();
    let limit = query.limit.unwrap_or(50);
    let now = client_now_ms();
    let mut expires_at = now.saturating_add(CURSOR_TTL_MS);
    let cursor = encoded.as_ref().map(|encoded| {
        let malformed = || validation("malformed inventory cursor");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(
            encoded.strip_prefix("inventory-page/").ok_or_else(malformed)?).map_err(|_| malformed())?;
        let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
        if cursor.actor != actor || cursor.agent != agent || cursor.query != query
            || cursor.expires_at < now || cursor.expires_at > expires_at {
            return Err(client_page_expired("inventory cursor expired or its binding changed"));
        }
        Ok(cursor)
    }).transpose()?;
    let mut result = HarnessInventory {
        kind: "harness-inventory".into(), agent_id: agent.into(), owner_host_id: query.owner_host_id.clone(),
        session_id: query.session_id.clone(), native_session_id: snapshot.as_ref().map(|snapshot| snapshot.session_id.clone()),
        runtime_incarnation: query.runtime_incarnation.clone(), collection: query.collection.clone(),
        status: "unsupported".into(), coverage: "none".into(), full_inventory: false,
        reason: Some("this harness has no observed native inventory source".into()),
        observed_at: snapshot.as_ref().map(|snapshot| snapshot.observed_at.clone()), items: Vec::new(),
        page: PageInfo { limit, has_more: false, next_cursor: None, cursor_expires_at: None },
    };
    let Some(snapshot) = snapshot else {
        if encoded.is_some() { return Err(client_page_expired("native inventory source changed")); }
        return serde_json::to_value(result).map_err(ApiError::internal);
    };
    let evidence_revision = hex::encode(Sha256::digest(serde_json::to_vec(&snapshot).map_err(ApiError::internal)?));
    let mut revision = Sha256::new();
    revision.update(evidence_revision.as_bytes());
    match query.collection.as_str() {
        "files" => {
            if let Some(root) = snapshot.workspace.as_deref() {
                let listing = crate::file_inventory::list(Path::new(root), &query.directory, &query.prefix)
                    .map_err(|error| match error {
                        crate::file_inventory::Error::InvalidDirectory | crate::file_inventory::Error::InvalidPrefix => validation("file inventory requires a visible root-relative directory and name prefix"),
                        crate::file_inventory::Error::Changed => client_page_expired("directory changed during inventory read"),
                        crate::file_inventory::Error::ScanLimit => inventory_unavailable("directory exceeds the bounded inventory scan"),
                        crate::file_inventory::Error::Io(error) => {
                            let _ = error.kind(); // Do not expose owner filesystem paths in errors.
                            inventory_unavailable("directory metadata cannot be read without following links")
                        }
                    })?;
                revision.update(format!("{}:{}:{}:{}:{}:{}", listing.device, listing.inode, listing.modified_seconds,
                    listing.modified_nanoseconds, listing.changed_seconds, listing.changed_nanoseconds).as_bytes());
                for entry in listing.entries {
                    if entry.name.chars().any(char::is_control) { continue }
                    revision.update(entry.name.as_bytes());
                    revision.update([u8::from(entry.directory)]);
                    result.items.push(HarnessInventoryItem { name: entry.name,
                        kind: if entry.directory { "directory" } else { "file" }.into(), source: None });
                }
                result.status = "supported".into(); result.coverage = "native-workspace".into();
                result.full_inventory = true; result.reason = None;
            } else { result.reason = Some("native session workspace has not been supplied".into()); }
        }
        "skills" => { result.reason = Some("OMP does not expose full loaded skills through the interactive extension API".into()); }
        "skill-commands" | "slash-commands" => {
            result.coverage = if query.collection == "skill-commands" { "enabled-skill-commands" } else { "dynamic-commands" }.into();
            result.status = match snapshot.dynamic_commands {
                st_drivers::harness_inventory::Availability::Supported => "supported",
                st_drivers::harness_inventory::Availability::Unsupported => "unsupported",
                st_drivers::harness_inventory::Availability::Unavailable => "unavailable",
            }.into();
            result.reason = Some("interactive native source excludes builtins and disabled skill commands; this is not full inventory".into());
            for command in snapshot.commands {
                if !command.name.starts_with(&query.prefix) || (query.collection == "skill-commands"
                    && command.source != st_drivers::harness_inventory::Source::Skill) { continue }
                result.items.push(HarnessInventoryItem { name: command.name,
                    kind: if command.source == st_drivers::harness_inventory::Source::Skill { "skill-command" } else { "slash-command" }.into(),
                    source: Some(match command.source { st_drivers::harness_inventory::Source::Extension => "extension",
                        st_drivers::harness_inventory::Source::Prompt => "prompt", st_drivers::harness_inventory::Source::Skill => "skill" }.into()) });
            }
        }
        _ => return Err(validation("unknown inventory collection")),
    }
    let after = local_snapshot(state, agent, &query)?;
    let after_revision = after.map(|snapshot| serde_json::to_vec(&snapshot)).transpose().map_err(ApiError::internal)?;
    if after_revision.as_deref().map(|bytes| hex::encode(Sha256::digest(bytes))).as_deref() != Some(evidence_revision.as_str()) {
        return Err(client_page_expired("native inventory changed during the read"));
    }
    let revision = hex::encode(revision.finalize());
    if let Some(cursor) = cursor {
        if cursor.revision != revision {
            return Err(client_page_expired("inventory directory or native source changed"));
        }
        expires_at = cursor.expires_at;
        result.items.retain(|item| item.name > cursor.after);
    }
    result.page.has_more = result.items.len() > limit;
    result.items.truncate(limit);
    if result.page.has_more {
        let cursor = Cursor { actor: actor.into(), agent: agent.into(), query, revision,
            after: result.items.last().expect("nonempty bounded inventory page").name.clone(), expires_at };
        result.page.next_cursor = Some(format!("inventory-page/{}", base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&cursor).map_err(ApiError::internal)?)));
        result.page.cursor_expires_at = Some(client_timestamp(expires_at));
    }
    serde_json::to_value(result).map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGENT: &str = "agent/example/inventory";
    const RUNTIME: &str = "inventory:one";
    fn fixture(root: &Path, workspace: &Path) -> AppState {
        let state = crate::api::tests::state(root);
        state.store.apply_internal(
            &parse_intent(&format!("version 2\nagent \"example/inventory\" {{ workspace {:?}; harness \"omp\" {{}} }}\n", workspace), "node").unwrap(),
            "inventory-fixture",
        ).unwrap();
        runtime(&state, RUNTIME);
        let owner = root.join("drivers").join(&hex::encode(Sha256::digest(AGENT.as_bytes()))[..24]);
        let observations = owner.join("observations");
        st_drivers::harness_events::enable(&observations, RUNTIME).unwrap();
        let seq = st_drivers::harness_state::claim(&observations, "example/inventory", "omp", "provider-one").unwrap();
        let mut observer = st_drivers::pi_channel::EventObserver::new(
            &observations, "example/inventory", "omp", "provider-one", seq, "example/inventory",
        ).unwrap();
        st_drivers::omp_session::record_channel_binding(&owner.join("sessions/omp"), "example/inventory",
            "example/inventory", "provider-one", "native-one", None, None).unwrap();
        st_drivers::omp_session::confirm_channel_binding(&owner.join("sessions/omp"), &observations,
            "example/inventory", "example/inventory", "provider-one", seq, "native-one", None).unwrap();
        observer.observe(&json!({"type":"ready", "sessionId":"native-one"})).unwrap();
        observer.observe(&json!({"type":"inventory", "session_id":"native-one",
            "workspace":workspace.to_str().unwrap(), "observed_at":"2026-10-04T12:00:00Z",
            "dynamic_commands":"supported", "commands":[
                {"name":"inspect","source":"extension"}, {"name":"skill:review","source":"skill"}
            ]})).unwrap();
        state
    }
    fn runtime(state: &AppState, incarnation: &str) {
        state.store.append_claim(&ClaimInput {
            subject: AGENT.into(), kind: "runtime.observed".into(), actor: Some("person/alex".into()),
            fields: serde_json::from_value(json!({"status":"running", "runtime_id":"example/inventory",
                "incarnation_id":incarnation, "terminal":false})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    fn query(collection: &str) -> HarnessInventoryQuery {
        HarnessInventoryQuery { owner_host_id: "host/node".into(), session_id: managed_session_id(AGENT, RUNTIME),
            native_session_id: "native-one".into(), runtime_incarnation: RUNTIME.into(), collection: collection.into(),
            directory: String::new(), prefix: String::new(), limit: Some(1), cursor: None }
    }
    async fn call(state: &AppState, query: HarnessInventoryQuery) -> Result<Value, ApiError> {
        read(State(state.clone()), Extension(ClientSession::local(Some("person/alex")).unwrap()),
            AxumPath(AGENT.into()), Query(query)).await.map(|Json(value)| value)
    }
    #[tokio::test]
    async fn inventory_pages_filter_confine_and_expire_on_namespace_or_native_binding_change() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta", ".env"] { std::fs::write(workspace.path().join(name), "private contents").unwrap(); }
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("external")).unwrap();
        let state = fixture(root.path(), workspace.path());
        let first = call(&state, query("files")).await.unwrap();
        assert_eq!(first["items"], json!([{"name":"alpha","kind":"file","source":null}]));
        assert_eq!(first["full_inventory"], true);
        let mut next = query("files");
        next.cursor = Some(first["page"]["next_cursor"].as_str().unwrap().into());
        let second = call(&state, next.clone()).await.unwrap();
        assert_eq!(second["items"], json!([{"name":"beta","kind":"file","source":null}]));
        assert_eq!(second["page"]["has_more"], false);
        for directory in ["..", ".env", "/absolute", "external"] {
            let mut request = query("files"); request.directory = directory.into();
            assert!(call(&state, request).await.is_err(), "must refuse {directory}");
        }
        let mut filtered = query("files"); filtered.prefix = "beta".into();
        assert_eq!(call(&state, filtered).await.unwrap()["items"], second["items"]);
        std::fs::write(workspace.path().join("gamma"), "private contents").unwrap();
        assert_eq!(call(&state, next).await.unwrap_err().code, "page-cursor-expired");
        let mut native_stale = query("files"); native_stale.native_session_id = "native-old".into();
        assert_eq!(call(&state, native_stale).await.unwrap_err().code, "stale-fence");
        runtime(&state, "inventory:two");
        assert_eq!(call(&state, query("files")).await.unwrap_err().code, "stale-fence");
    }
    #[tokio::test]
    async fn dynamic_native_inventory_never_claims_full_skills_or_builtins() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = fixture(root.path(), workspace.path());
        let skills = call(&state, query("skills")).await.unwrap();
        assert_eq!(skills["status"], "unsupported");
        assert_eq!(skills["full_inventory"], false);
        assert_eq!(skills["items"], json!([]));
        let mut enabled = query("skill-commands"); enabled.prefix = "skill:r".into();
        let commands = call(&state, enabled).await.unwrap();
        assert_eq!(commands["coverage"], "enabled-skill-commands");
        assert_eq!(commands["full_inventory"], false);
        assert_eq!(commands["items"], json!([{"name":"skill:review","kind":"skill-command","source":"skill"}]));
        let mut slash = query("slash-commands"); slash.prefix = "ins".into();
        let command = call(&state, slash).await.unwrap();
        assert_eq!(command["items"], json!([{"name":"inspect","kind":"slash-command","source":"extension"}]));
        assert_eq!(command["full_inventory"], false);
        assert!(!serde_json::to_string(&command).unwrap().contains(workspace.path().to_str().unwrap()));
    }
    #[tokio::test]
    async fn pending_native_switch_and_provider_replacement_refuse_predecessor_inventory() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = fixture(root.path(), workspace.path());
        let owner = root.path().join("drivers").join(&hex::encode(Sha256::digest(AGENT.as_bytes()))[..24]);
        st_drivers::omp_session::record_channel_binding(&owner.join("sessions/omp"), "example/inventory",
            "example/inventory", "provider-one", "native-two", None, None).unwrap();
        assert_eq!(call(&state, query("slash-commands")).await.unwrap_err().code, "stale-fence");
        st_drivers::harness_state::claim(&owner.join("observations"), "example/inventory", "omp", "provider-two").unwrap();
        assert_eq!(call(&state, query("files")).await.unwrap_err().code, "stale-fence");
    }
}
