use super::*;
use crate::private_notes::{NotesFence, NotesWrite};
use st3_schema::private_notes::PrivateNotesUri;


#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteParameters {
    uri: String,
    markdown: String,
}

pub(super) fn principal(state: &AppState, session: &ClientSession, bound: Option<&BoundAgent>, scope: &str) -> Result<(), ApiError> {
    if bound.is_some() || !session.authority_actor.starts_with("person/")
        || state.private_notes.person.as_deref() != Some(session.authority_actor.as_str()) {
        return Err(forbidden("notes principal authority belongs only to the owner node's configured person; native agents cannot impersonate that person"));
    }
    require_scope(session, scope)
}

pub(super) fn entitled(state: &AppState, session: &ClientSession, scope: &str) -> bool {
    state.private_notes.person.as_deref() == Some(session.authority_actor.as_str()) && session.allows(scope)
}

fn native_source(state: &AppState, bound: &BoundAgent) -> Result<(String, String), ApiError> {
    let desired = state.store.desired_subjects_named(std::slice::from_ref(&bound.0)).map_err(ApiError::internal)?;
    let [desired] = desired.as_slice() else { return Err(forbidden("the native notes producer has no admitted seat declaration")); };
    if desired.kind != "agent" || desired.owner_run.is_some() {
        return Err(forbidden("the native notes producer must be a declared catalog seat"));
    }
    let field = |name: &str| desired.desired.get("children").and_then(Value::as_array)
        .and_then(|children| children.iter().find(|child| child["name"].as_str() == Some(name)))
        .and_then(|child| child["arguments"].as_array()).and_then(|arguments| arguments.first()).and_then(Value::as_str);
    let identity = field("identity").or_else(|| desired.desired["arguments"].as_array().and_then(|args| args.first()).and_then(Value::as_str))
        .ok_or_else(|| forbidden("the admitted native seat has no catalog identity"))?;
    let host = desired.member.as_ref().map(|member| member.host.as_str()).or_else(|| field("host")).unwrap_or(&state.node);
    if host != state.node { return Err(forbidden("the native notes producer belongs to another owner node")); }
    let uri = PrivateNotesUri::for_subject(host, identity).map_err(|error| validation(error.message))?;
    let runtime = state.store.latest_claim(&bound.0, Some("runtime.observed")).map_err(ApiError::internal)?
        .filter(|runtime| runtime.body["fields"]["status"] == "running")
        .ok_or_else(|| stale("the native notes source is not running"))?;
    let incarnation = runtime.body["fields"]["incarnation_id"].as_str()
        .ok_or_else(|| stale("the native notes source has no current incarnation"))?;
    Ok((uri.to_string(), incarnation.into()))
}

pub(in crate::api) async fn bind_native(
    state: &AppState,
    agent: &str,
    source_incarnation: &str,
) -> Result<(), ApiError> {
    if state.private_notes.catalogs.is_empty() { return Ok(()); }
    let desired = state.store.desired_subjects_named(&[agent.to_owned()]).map_err(ApiError::internal)?;
    // Runtime-only and run-owned agents have no catalog-owned notes source.
    if desired.is_empty() || desired.iter().any(|subject| subject.kind != "agent" || subject.owner_run.is_some()) {
        return Ok(());
    }
    let (uri, incarnation) = native_source(state, &BoundAgent(agent.into()))?;
    if source_incarnation != incarnation { return Err(stale("the native notes source incarnation is stale")); }
    let producer = state.clone();
    let agent = agent.to_owned();
    super::super::blocking_action(move || {
        match producer.private_notes.bind_source(&producer.node, &producer.state_dir, &uri, &agent, &incarnation) {
            Ok(_) => Ok(()),
            // Seats without a declared notes binding have no notes supply to publish.
            Err(error) if error.code == "not-found" => Ok(()),
            Err(error) => Err(error),
        }
    }).await
}

pub(in crate::api) async fn detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    bound: Option<Extension<BoundAgent>>,
    AxumPath(uri): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let native = if let Some(bound) = &bound {
        let (own_uri, incarnation) = native_source(&state, &bound.0)?;
        if uri != own_uri { return Err(forbidden("a native agent may read only its canonical self notes URI")); }
        Some((bound.0.0.clone(), incarnation))
    } else {
        principal(&state, &session, None, "notes.read")?;
        None
    };
    let reader = state.clone();
    let notes = super::super::blocking_action(move || {
        if let Some((agent, incarnation)) = native {
            reader.private_notes.check_source(&reader.node, &reader.state_dir, &uri, &agent, &incarnation)?;
        }
        reader.private_notes.read(&reader.node, &uri)
    }).await?;
    let mut actions = Vec::new();
    if bound.is_none() && entitled(&state, &session, "notes.write") {
        actions.push(json!({"id":"private-notes.write", "fence":{"snapshot_id":snapshot.id, "subject_revisions":{}, "private_notes":notes.fence}}));
    }
    Ok(Json(json!({"ref":notes.uri,"family":"private-notes","schema":"st3.private-notes@1","data":notes,"actions":actions,"live":null})))
}

pub(super) async fn write(state: &AppState, session: &ClientSession, bound: Option<&BoundAgent>, request: ActionRequest) -> Result<Value, ApiError> {
    principal(state, session, bound, "notes.write")?;
    validate_fence(state, &request.fence)?;
    let parameters: WriteParameters = serde_json::from_value(request.parameters).map_err(|_| validation("notes write requires only a canonical URI and complete Markdown"))?;
    let fence: NotesFence = request.fence.private_notes.ok_or_else(|| validation("notes write requires its carrier generation and revision fence"))?;
    let write = NotesWrite { uri: parameters.uri, markdown: parameters.markdown, fence };
    let writer = state.clone();
    let actor = session.actor.clone();
    let key = request.idempotency_key;
    let mut result = super::super::blocking_action(move || writer.private_notes.write(&writer.node, &writer.state_dir, &actor, &key, &write)).await?;
    result["action_id"] = request.id.into();
    result["snapshot_id"] = new_client_snapshot(state).id.into();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    async fn request(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app.oneshot(Request::builder().method(method).uri(path)
            .header(LOCAL_PERSON_HEADER, "person/operator")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn bearer(app: Router, path: &str, credential: &str) -> (StatusCode, Value) {
        let response = app.oneshot(Request::builder().uri(path)
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn notes_routes_require_transport_proof_and_never_allow_native_principal_impersonation() {
        let root = tempfile::tempdir().unwrap();
        let catalog = root.path().join("catalog");
        let subject = catalog.join("agents/node/worker");
        std::fs::create_dir_all(subject.join("resources")).unwrap();
        let uri = "dev.schickling.agent-private-notes://node/worker";
        std::fs::write(subject.join("agent.kdl"), format!("agent \"worker\" {{\n host \"node\"\n resource \"notes\" uri=\"{uri}\"\n}}\n")).unwrap();
        let mut state = crate::api::tests::state(root.path());
        state.private_notes = Arc::new(crate::private_notes::Authority {
            person: Some("person/operator".into()), catalogs: vec![catalog],
        });
        let app = crate::api::router(state.clone());
        let path = "/v1/client/private-notes/dev.schickling.agent-private-notes%3A%2F%2Fnode%2Fworker";
        let (status, denied) = request(app.clone(), "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let principal = app.clone().layer(Extension(VerifiedNotesPrincipal));
        let (status, notes) = request(principal.clone(), "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{notes}");
        let value = &notes["value"];
        assert_eq!(value["data"]["markdown"], "");
        let action = json!({
            "api_version":"st3.client.v0", "id":"action/notes-first", "type":"private-notes.write",
            "idempotency_key":"notes-first-key-0001", "fence":value["actions"][0]["fence"],
            "parameters":{"uri":uri,"markdown":"owner private bytes\n"},
        });
        let native = principal.clone().layer(Extension(BoundAgent("agent/foreign".into())));
        let (status, denied) = request(native.clone(), "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, denied) = request(native.clone(), "POST", "/v1/client/actions", action.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, denied) = request(app.clone(), "POST", "/v1/client/actions", action.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let pairing = json!({"api_version":"st3.client.v0", "person_id":"person/operator", "device_name":"notes reader", "scopes":["notes.read"]});
        let (status, denied) = request(app, "POST", "/v1/client/pairings", pairing.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, denied) = request(native, "POST", "/v1/client/pairings", pairing.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, offered) = request(principal.clone(), "POST", "/v1/client/pairings", pairing).await;
        assert_eq!(status, StatusCode::OK, "{offered}");
        let (status, written) = request(principal.clone(), "POST", "/v1/client/actions", action.clone()).await;
        assert_eq!(status, StatusCode::OK, "{written}");
        let pairing_id = offered["value"]["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap();
        let complete_path = format!("/v1/client/pairings/{pairing_id}/complete");
        let (status, completed) = request(principal.clone(), "POST", &complete_path, json!({
            "api_version":"st3.client.v0", "code":offered["value"]["code"],
            "device_public_key":"legacy-notes-device-public-key-fixture",
        })).await;
        assert_eq!(status, StatusCode::OK, "{completed}");
        let credential = completed["value"]["credential"].as_str().unwrap();
        let (status, paired_notes) = bearer(principal.clone(), path, credential).await;
        assert_eq!(status, StatusCode::OK, "{paired_notes}");
        assert_eq!(paired_notes["value"]["data"]["markdown"], "owner private bytes\n");
        assert_eq!(paired_notes["value"]["actions"], json!([]));
        let replica = state.store.export_replication(0).unwrap();
        let exported = serde_json::to_string(&replica).unwrap();
        assert!(!exported.contains("owner private bytes"));
        let peer_root = tempfile::tempdir().unwrap();
        let mut peer = crate::api::tests::state(peer_root.path());
        peer.node = "peer".into();
        peer.store = Arc::new(crate::store::Store::open_memory("peer").unwrap());
        peer.private_notes = Arc::new(crate::private_notes::Authority {
            person: Some("person/operator".into()), catalogs: vec![],
        });
        peer.store.import_replication("node", &replica).unwrap();
        let peer_app = crate::api::router(peer);
        let (status, denied) = bearer(peer_app.clone(), path, credential).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, denied) = bearer(peer_app, "/v1/client/capabilities", credential).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let mut stale = action;
        stale["id"] = "action/notes-concurrent".into();
        stale["idempotency_key"] = "notes-concurrent-key-0001".into();
        stale["parameters"]["markdown"] = "stale overwrite\n".into();
        let (status, refused) = request(principal, "POST", "/v1/client/actions", stale).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(std::fs::read_to_string(subject.join("resources/private-notes.md")).unwrap(), "owner private bytes\n");
    }
}
