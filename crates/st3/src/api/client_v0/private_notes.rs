use super::*;
use crate::private_notes::{NotesFence, NotesWrite};


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


pub(in crate::api) async fn detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    bound: Option<Extension<BoundAgent>>,
    AxumPath(uri): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    if bound.is_some() {
        return Err(ApiError::bad(crate::model::St3Error::new(
            "unsupported-capability",
            "native private-notes self-read is unavailable without admitted reader-incarnation provenance",
        )));
    }
    principal(&state, &session, None, "notes.read")?;
    let reader = state.clone();
    let notes = super::super::blocking_action(move || {
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

    async fn request(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app.oneshot(Request::builder().method(method).uri(path)
            .header(LOCAL_PERSON_HEADER, "person/operator")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn bearer(app: Router, method: &str, path: &str, body: Value, credential: &str) -> (StatusCode, Value) {
        let response = app.oneshot(Request::builder().method(method).uri(path)
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn notes_routes_require_attested_pairing_and_refuse_native_and_forged_authority() {
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
        let unclassified = crate::api::router(state.clone());
        let app = unclassified.clone().layer(Extension(private_notes_login::Identity::VerifiedNonAgent));
        let path = "/v1/client/private-notes/dev.schickling.agent-private-notes%3A%2F%2Fnode%2Fworker";
        let issuer = app.clone().layer(Extension(VerifiedNotesPairingPrincipal));
        for local in [app.clone(), issuer.clone()] {
            let (status, denied) = request(local, "GET", path, Value::Null).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        }
        let forged = crate::model::ClaimInput {
            subject: "custom/client/forged-notes-pair".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("credential_hash".into(), json!(credential_digest("forged-notes-credential"))),
                ("session_actor".into(), json!("client/forged-notes")),
                ("person_id".into(), json!("person/operator")),
                ("scopes".into(), json!(["notes.read", "notes.write"])),
                ("expires_at_unix_ms".into(), json!(u64::MAX)),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        };
        state.store.append_claim(&forged).unwrap();
        let (status, denied) = bearer(app.clone(), "GET", path, Value::Null, "forged-notes-credential").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        for kind in ["custom.client.pairing-begun", "custom.client.pairing-completed", "custom.client.pairing-revoked"] {
            let mut public = forged.clone();
            public.kind = kind.into();
            let (status, denied) = request(app.clone(), "POST", "/v1/claims", serde_json::to_value(&public).unwrap()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{denied}");
            assert_eq!(denied["code"], "claim-write-forbidden");
        }
        let pairing = json!({"api_version":"st3.client.v0", "person_id":"person/operator", "device_name":"notes editor", "scopes":["notes.read", "notes.write"]});
        let native_issuer = unclassified.clone().layer(Extension(private_notes_login::Identity::Agent));
        for denied_issuer in [app.clone(), native_issuer.clone()] {
            let (status, denied) = request(denied_issuer, "POST", "/v1/client/pairings", pairing.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        }
        let (status, offered) = request(issuer.clone(), "POST", "/v1/client/pairings", pairing).await;
        assert_eq!(status, StatusCode::OK, "{offered}");
        let pairing_id = offered["value"]["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap();
        let complete_path = format!("/v1/client/pairings/{pairing_id}/complete");
        let completion = json!({
            "api_version":"st3.client.v0", "code":offered["value"]["code"],
            "device_public_key":"legacy-notes-device-public-key-fixture",
        });
        for denied_peer in [native_issuer, unclassified.clone(),
            unclassified.clone().layer(Extension(private_notes_login::Identity::Unavailable))] {
            let (status, denied) = request(denied_peer, "POST", &complete_path, completion.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        }
        let (status, completed) = request(app.clone(), "POST", &complete_path, completion).await;
        assert_eq!(status, StatusCode::OK, "{completed}");
        let credential = completed["value"]["credential"].as_str().unwrap();
        for denied_peer in [unclassified.clone(),
            unclassified.clone().layer(Extension(private_notes_login::Identity::Unavailable))] {
            let (status, denied) = bearer(denied_peer, "GET", path, Value::Null, credential).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        }
        let trusted = unclassified.clone().layer(Extension(private_notes_login::Identity::TrustedPairedTransport));
        let (status, denied) = request(trusted.clone(), "GET", path, Value::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, notes) = bearer(trusted, "GET", path, Value::Null, credential).await;
        assert_eq!(status, StatusCode::OK, "{notes}");
        let (status, notes) = bearer(app.clone(), "GET", path, Value::Null, credential).await;
        assert_eq!(status, StatusCode::OK, "{notes}");
        assert_eq!(notes["value"]["data"]["markdown"], "");
        let action = json!({
            "api_version":"st3.client.v0", "id":"action/notes-first", "type":"private-notes.write",
            "idempotency_key":"notes-first-key-0001", "fence":notes["value"]["actions"][0]["fence"],
            "parameters":{"uri":uri,"markdown":"owner private bytes\n"},
        });
        let native = unclassified.layer(Extension(private_notes_login::Identity::Agent));
        let (status, denied) = bearer(native.clone(), "GET", path, Value::Null, credential).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{denied}");
        assert_eq!(denied["code"], "unsupported-capability");
        let (status, denied) = bearer(native, "POST", "/v1/client/actions", action.clone(), credential).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let (status, written) = bearer(app.clone(), "POST", "/v1/client/actions", action.clone(), credential).await;
        assert_eq!(status, StatusCode::OK, "{written}");
        let (status, paired_notes) = bearer(app.clone(), "GET", path, Value::Null, credential).await;
        assert_eq!(status, StatusCode::OK, "{paired_notes}");
        assert_eq!(paired_notes["value"]["data"]["markdown"], "owner private bytes\n");
        assert_eq!(written["value"]["private_notes"], paired_notes["value"]["data"]["fence"]);
        let replica = state.store.export_replication(0).unwrap();
        assert!(!serde_json::to_string(&replica).unwrap().contains("owner private bytes"));
        let peer_root = tempfile::tempdir().unwrap();
        let mut peer = crate::api::tests::state(peer_root.path());
        peer.node = "peer".into();
        peer.store = Arc::new(crate::store::Store::open_memory("peer").unwrap());
        peer.private_notes = Arc::new(crate::private_notes::Authority {
            person: Some("person/operator".into()), catalogs: vec![],
        });
        peer.store.import_replication("node", &replica).unwrap();
        let peer_app = crate::api::router(peer).layer(Extension(private_notes_login::Identity::VerifiedNonAgent));
        let (status, denied) = bearer(peer_app, "GET", path, Value::Null, credential).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        let mut stale = action;
        stale["id"] = "action/notes-concurrent".into();
        stale["idempotency_key"] = "notes-concurrent-key-0001".into();
        stale["parameters"]["markdown"] = "stale overwrite\n".into();
        let (status, refused) = bearer(app, "POST", "/v1/client/actions", stale, credential).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(std::fs::read_to_string(subject.join("resources/private-notes.md")).unwrap(), "owner private bytes\n");
    }
}
