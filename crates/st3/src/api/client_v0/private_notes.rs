use super::*;
use crate::private_notes::{NotesFence, NotesWrite, operation_key};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteParameters {
    uri: String,
    markdown: String,
}

pub(in crate::api) async fn detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(uri): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let reader = state.clone();
    let notes = super::super::blocking_action(move || {
        reader.private_notes.read(&reader.node, &uri)
    }).await?;
    let mut actions = Vec::new();
    if session.allows("control.work") && acting_party(&session) {
        actions.push(json!({"id":"private-notes.write", "fence":{"snapshot_id":snapshot.id, "subject_revisions":{}, "private_notes":notes.fence}}));
    }
    Ok(Json(json!({"ref":notes.uri,"family":"private-notes","schema":"st3.private-notes@1","data":notes,"actions":actions,"live":null})))
}

fn reuse_observation_transport(state: &AppState, input: &mut ClaimInput) -> Result<(), ApiError> {
    let key = input.idempotency_key.as_deref().expect("notes observations have stable keys");
    let existing = smallclaims::store::operation_tx(
        &state.store.readers.get(), &smallclaims::store::operation_id_for_key(key),
    ).map_err(ApiError::internal)?;
    let Some((_, claim_id, operation_state)) = existing else { return Ok(()) };
    if operation_state != "active" {
        return Err(ApiError::bad(St3Error::new("private-notes-indeterminate", "the notes audit operation has conflicting authority")));
    }
    let claim = state.store.claim_by_id(&claim_id).map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::bad(St3Error::new("private-notes-indeterminate", "the original notes audit metadata is unavailable")))?;
    let fields = claim.body.get("fields").and_then(Value::as_object)
        .ok_or_else(|| ApiError::internal("the persisted notes observation has no fields"))?;
    if claim.subject != input.subject || claim.kind != input.kind || claim.actor != input.actor
        || fields.len() != input.fields.len()
        || input.fields.iter().any(|(key, value)| key != "transport" && fields.get(key) != Some(value)) {
        return Err(ApiError::bad(St3Error::new("idempotency-conflict", "the notes key names a different attributed observation")));
    }
    let transport = fields.get("transport")
        .filter(|value| matches!(value.as_str(), Some("unix" | "fabric-loopback")))
        .ok_or_else(|| ApiError::internal("the persisted notes observation has no normal transport"))?;
    // The same ordinary paired session may retry through either listener.
    // Preserve the original durable intent's transport, not the recovery hop.
    input.fields.insert("transport".into(), transport.clone());
    Ok(())
}

fn publish_observation(state: &AppState, input: &mut ClaimInput) -> Result<(), ApiError> {
    reuse_observation_transport(state, input)?;
    if let Err(error) = state.store.append_claim(input) {
        if error.code != "idempotency-mismatch" { return Err(ApiError::bad(error)); }
        // Another daemon process may have published this key after our lookup.
        reuse_observation_transport(state, input)?;
        state.store.append_claim(input).map_err(ApiError::bad)?;
    }
    signal_changed(state);
    Ok(())
}

pub(super) async fn write(state: &AppState, session: &ClientSession, request: ActionRequest) -> Result<Value, ApiError> {
    require_scope(session, "control.work")?;
    if !acting_party(session) {
        return Err(forbidden("client mutations require a concrete person or a local agent"));
    }
    validate_fence(state, &request.fence)?;
    let parameters: WriteParameters = serde_json::from_value(request.parameters).map_err(|_| validation("notes write requires only a canonical URI and complete Markdown"))?;
    st3_schema::private_notes::PrivateNotesUri::parse(&parameters.uri).map_err(|error| validation(error.message))?;
    let fence: NotesFence = request.fence.private_notes.ok_or_else(|| validation("notes write requires its carrier generation and revision fence"))?;
    let write = NotesWrite { uri: parameters.uri, markdown: parameters.markdown, fence };
    let operation = operation_key(&session.actor, &request.idempotency_key).map_err(ApiError::bad)?;
    // No supported write can change bytes without a durable attributed intent.
    // A graph failure after local completion remains an error; exact-key retry
    // recovers the private receipt and idempotently publishes completion.
    let mut audit = ClaimInput {
        subject: format!("resource/{}", crate::graph::uri_reference_name(&write.uri)),
        kind: "resource.observed".into(),
        actor: Some(session_claim_actor(session)),
        fields: BTreeMap::from([
            ("kind".into(), json!("uri.reference")),
            // Only the URI is a resource fact; attribution is observation metadata.
            ("facts".into(), json!({"uri":write.uri})),
            ("action".into(), json!("private-notes.write")),
            ("operation_id".into(), json!(format!("operation/private-notes-{}", &operation[..24]))),
            ("stage".into(), json!("requested")),
            ("authority_actor".into(), json!(session.authority_actor)),
            ("session_actor".into(), json!(session.actor)),
            ("transport".into(), json!(session.transport)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("private-notes:{operation}:requested")),
    };
    publish_observation(state, &mut audit)?;
    let writer = state.clone();
    let actor = session.actor.clone();
    let key = request.idempotency_key;
    let mut result = super::super::blocking_action(move || {
        writer.private_notes.write(&writer.node, &writer.state_dir, &actor, &key, &write)
    }).await?;
    audit.fields.insert("stage".into(), json!("completed"));
    audit.idempotency_key = Some(format!("private-notes:{operation}:completed"));
    publish_observation(state, &mut audit)?;
    result["action_id"] = request.id.into();
    result["snapshot_id"] = new_client_snapshot(state).id.into();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    async fn request(app: Router, actor: Option<&str>, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if let Some(actor) = actor { builder = builder.header(LOCAL_PERSON_HEADER, actor); }
        let response = app.oneshot(builder.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn bearer(app: Router, credential: &str, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app.oneshot(Request::builder().method(method).uri(path)
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn ordinary_person_and_agent_writes_are_attributed_without_replicating_private_bytes() {
        let root = tempfile::tempdir().unwrap();
        let catalog = root.path().join("catalog");
        let subject = catalog.join("agents/node/worker");
        std::fs::create_dir_all(subject.join("resources")).unwrap();
        let uri = "dev.schickling.agent-private-notes://node/worker";
        std::fs::write(subject.join("agent.kdl"), format!("agent \"worker\" {{\n host \"node\"\n resource \"notes\" uri=\"{uri}\"\n}}\n")).unwrap();
        let mut state = crate::api::tests::state(root.path());
        state.private_notes = Arc::new(crate::private_notes::Authority { catalogs: vec![catalog] });
        let app = crate::api::router(state.clone());
        let path = "/v1/client/private-notes/dev.schickling.agent-private-notes%3A%2F%2Fnode%2Fworker";
        let (status, read_only) = request(app.clone(), None, "GET", path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{read_only}");
        assert_eq!(read_only["value"]["actions"], json!([]));
        for (index, actor) in ["person/operator", "agent/node/worker"].into_iter().enumerate() {
            let app = if actor.starts_with("agent/") {
                app.clone().layer(Extension(BoundAgent(actor.into())))
            } else {
                app.clone()
            };
            let (status, notes) = request(app.clone(), Some(actor), "GET", path, Value::Null).await;
            assert_eq!(status, StatusCode::OK, "{notes}");
            let action = json!({
                "api_version":"st3.client.v0", "id":format!("action/notes-{index}"), "type":"private-notes.write",
                "idempotency_key":format!("notes-key-{index}-00000001"), "fence":notes["value"]["actions"][0]["fence"],
                "parameters":{"uri":uri,"markdown":format!("private bytes from {actor}\n")},
            });
            let (status, denied) = request(app.clone(), None, "POST", "/v1/client/actions", action.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
            let (status, written) = request(app.clone(), Some(actor), "POST", "/v1/client/actions", action.clone()).await;
            assert_eq!(status, StatusCode::OK, "{written}");
            let (status, retried) = request(app.clone(), Some(actor), "POST", "/v1/client/actions", action.clone()).await;
            assert_eq!(status, StatusCode::OK, "{retried}");
            assert_eq!(retried["value"]["operation_id"], written["value"]["operation_id"]);
            let mut stale = action;
            stale["idempotency_key"] = format!("notes-stale-{index}-000001").into();
            stale["parameters"]["markdown"] = "stale private overwrite".into();
            let (status, refused) = request(app.clone(), Some(actor), "POST", "/v1/client/actions", stale).await;
            assert_eq!(status, StatusCode::CONFLICT, "{refused}");
            assert_eq!(std::fs::read_to_string(subject.join("resources/private-notes.md")).unwrap(), format!("private bytes from {actor}\n"));
            let claims = state.store.claims_for(&format!("resource/{}", crate::graph::uri_reference_name(uri)), Some("resource.observed")).unwrap();
            let completed: Vec<_> = claims.iter().filter(|claim| claim.body.pointer("/fields/stage").and_then(Value::as_str) == Some("completed") && claim.body.pointer("/fields/session_actor").and_then(Value::as_str) == Some(actor)).collect();
            assert_eq!(completed.len(), 1, "exact retry must not duplicate completion");
            assert_eq!(completed[0].actor.as_deref(), Some(actor));
            assert_eq!(completed[0].body["fields"]["authority_actor"], actor);
            assert_eq!(completed[0].body["fields"]["transport"], "unix");
        }
        let peer = crate::store::Store::open_memory("peer").unwrap();
        peer.import_replication("node", &state.store.export_replication(0).unwrap()).unwrap();
        let replicated = peer.claims_for(&format!("resource/{}", crate::graph::uri_reference_name(uri)), Some("resource.observed")).unwrap();
        let replica = serde_json::to_string(&replicated).unwrap();
        for private in ["private bytes from", "stale private overwrite", root.path().to_str().unwrap(), "carrier_generation", "\"revision\"", "\"markdown\""] {
            assert!(!replica.contains(private), "replication exposed {private}");
        }
    }

    #[tokio::test]
    async fn audit_failure_blocks_mutation_or_recovers_completion_without_replacing_again() {
        use std::os::unix::fs::MetadataExt as _;
        let root = tempfile::tempdir().unwrap();
        let catalog = root.path().join("catalog");
        let subject = catalog.join("agents/node/worker");
        std::fs::create_dir_all(subject.join("resources")).unwrap();
        let uri = "dev.schickling.agent-private-notes://node/worker";
        std::fs::write(subject.join("agent.kdl"), format!("agent \"worker\" {{\n host \"node\"\n resource \"notes\" uri=\"{uri}\"\n}}\n")).unwrap();
        let graph_path = root.path().join("graph.sqlite");
        let mut state = crate::api::tests::state(root.path());
        state.store = Arc::new(crate::store::Store::open(&graph_path, "node").unwrap());
        state.private_notes = Arc::new(crate::private_notes::Authority { catalogs: vec![catalog] });
        let app = crate::api::router(state.clone());
        let fabric = crate::api::fabric_router(state.clone());
        let (status, offered) = request(app.clone(), Some("person/operator"), "POST", "/v1/client/pairings", json!({
            "api_version":"st3.client.v0", "person_id":"person/operator", "device_name":"audit recovery", "full_control":true,
        })).await;
        assert_eq!(status, StatusCode::OK, "{offered}");
        let pairing_id = offered["value"]["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap();
        let (status, paired) = request(app.clone(), Some("person/operator"), "POST", &format!("/v1/client/pairings/{pairing_id}/complete"), json!({
            "api_version":"st3.client.v0", "code":offered["value"]["code"], "device_public_key":"legacy-audit-recovery-device-key-fixture",
        })).await;
        assert_eq!(status, StatusCode::OK, "{paired}");
        let credential = paired["value"]["credential"].as_str().unwrap();
        let path = "/v1/client/private-notes/dev.schickling.agent-private-notes%3A%2F%2Fnode%2Fworker";
        let (status, notes) = bearer(app.clone(), credential, "GET", path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{notes}");
        let action = json!({
            "api_version":"st3.client.v0", "id":"action/notes-audit-recovery", "type":"private-notes.write",
            "idempotency_key":"notes-audit-recovery-0001", "fence":notes["value"]["actions"][0]["fence"],
            "parameters":{"uri":uri,"markdown":"private audit recovery bytes\n"},
        });
        let graph = rusqlite::Connection::open(&graph_path).unwrap();
        graph.execute_batch("CREATE TRIGGER refuse_notes_audit BEFORE INSERT ON claims
            WHEN NEW.kind='resource.observed' AND json_extract(NEW.body,'$.fields.stage')='requested'
            BEGIN SELECT RAISE(FAIL, 'injected requested audit refusal'); END;").unwrap();
        let (status, refused) = bearer(app.clone(), credential, "POST", "/v1/client/actions", action.clone()).await;
        assert_ne!(status, StatusCode::OK, "{refused}");
        let carrier = subject.join("resources/private-notes.md");
        assert!(!carrier.exists(), "a failed durable intent must prevent byte mutation");
        graph.execute_batch("DROP TRIGGER refuse_notes_audit;
            CREATE TRIGGER refuse_notes_audit BEFORE INSERT ON claims
            WHEN NEW.kind='resource.observed' AND json_extract(NEW.body,'$.fields.stage')='completed'
            BEGIN SELECT RAISE(FAIL, 'injected completed audit refusal'); END;").unwrap();
        let (status, pending) = bearer(app.clone(), credential, "POST", "/v1/client/actions", action.clone()).await;
        assert_ne!(status, StatusCode::OK, "{pending}");
        assert_eq!(std::fs::read_to_string(&carrier).unwrap(), "private audit recovery bytes\n");
        let before = std::fs::metadata(&carrier).unwrap();
        let resource = format!("resource/{}", crate::graph::uri_reference_name(uri));
        let claims = state.store.claims_for(&resource, Some("resource.observed")).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].body["fields"]["stage"], "requested");
        assert_eq!(claims[0].actor.as_deref(), Some("person/operator"));
        graph.execute_batch("DROP TRIGGER refuse_notes_audit;").unwrap();
        let (status, recovered) = bearer(fabric, credential, "POST", "/v1/client/actions", action.clone()).await;
        assert_eq!(status, StatusCode::OK, "{recovered}");
        let (status, retried) = bearer(app, credential, "POST", "/v1/client/actions", action).await;
        assert_eq!(status, StatusCode::OK, "{retried}");
        assert_eq!(recovered["value"]["private_notes"], retried["value"]["private_notes"]);
        let after = std::fs::metadata(&carrier).unwrap();
        assert_eq!((before.ino(), before.mtime(), before.mtime_nsec()), (after.ino(), after.mtime(), after.mtime_nsec()),
            "completion publication recovery must not replace the carrier again");
        let claims = state.store.claims_for(&resource, Some("resource.observed")).unwrap();
        assert_eq!(claims.len(), 2, "recovery must publish exactly one requested and completed observation");
        assert_eq!(claims[1].body["fields"]["stage"], "completed");
        assert_eq!(claims[1].body["fields"]["session_actor"], paired["value"]["session_actor"]);
        assert_eq!(claims[1].body["fields"]["transport"], "unix", "completion retains the original intent attribution across listener recovery");
    }
}
