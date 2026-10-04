//! Person-owned arrangements use the shared register projection, never glass privacy or history folds.
use super::*;

pub(super) fn person(
    session: &ClientSession,
    requested: Option<&str>,
    write: bool,
) -> Result<String, ApiError> {
    require_scope(session, if write { "control.arrangements" } else { "read.arrangements" })?;
    let person = requested.ok_or_else(|| validation("arrangements require an explicit person/NAME owner"))?;
    if !person.starts_with("person/") || person.matches('/').count() != 1 || person.len() == 7 {
        return Err(validation("arrangement owner must be person/NAME"));
    }
    st3_schema::registry().validate_subject(person).map_err(|error| validation(error.message))?;
    if !acting_party(session) {
        return Err(forbidden("arrangements require a concrete person or trusted local fleet agent"));
    }
    if session.authority_actor.starts_with("person/") && session.authority_actor != person {
        return Err(forbidden("cross-person arrangements require delegated authority"));
    }
    Ok(person.to_owned())
}

/// Keep whole resources; byte bounds may shorten a count-bounded authoritative window.
pub(crate) fn window_end(items: &[Value], offset: usize, end: usize) -> Result<usize, ApiError> {
    let budget = CLIENT_MAX_RESPONSE_BYTES - 128_000;
    let mut used = 0;
    let mut kept = offset;
    for item in items.get(offset..end).unwrap_or_default() {
        let bytes = serde_json::to_vec(item).map_err(ApiError::internal)?.len() + 1;
        if used + bytes > budget {
            if kept == offset {
                return Err(validation("the arrangement exceeds the bounded client response budget"));
            }
            break;
        }
        used += bytes;
        kept += 1;
    }
    Ok(kept)
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Extension(request_snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<ClientResourcePage>), ApiError> {
    let owner = person(&session, query.person.as_deref(), false)?;
    if query.actor.is_some() || query.history {
        return Err(validation("arrangements select a person and current state only"));
    }
    if query.cursor.is_some() {
        let page = client_page(&state, &request_snapshot, "arrangements", vec![], &query)?;
        return Ok((Extension(request_snapshot), Json(page)));
    }
    let read_state = state.clone();
    let (snapshot, items) = blocking_store(move || {
        read_state.store.read_snapshot(|index| {
            Ok((client_snapshot_at(&read_state, index), read_state.store.arrangements(&owner, index)?))
        })
    }).await?;
    let page = client_page(&state, &snapshot, "arrangements", items, &query)?;
    Ok((Extension(snapshot), Json(page)))
}

pub(crate) async fn get(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath((name, uuid)): AxumPath<(String, String)>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    let owner = person(&session, Some(&format!("person/{name}")), false)?;
    let subject = format!("arrangement/{owner}/{uuid}");
    st3_schema::arrangements::owner(&subject).map_err(|error| validation(error.message))?;
    let (snapshot, resource) = blocking_store(move || {
        state.store.read_snapshot(|index| {
            Ok((client_snapshot_at(&state, index), state.store.arrangement(&subject, index)?))
        })
    }).await?;
    let resource = resource.ok_or_else(|| ApiError::not_found("the arrangement is absent or retired"))?;
    if serde_json::to_vec(&resource).map_err(ApiError::internal)?.len() > CLIENT_MAX_RESPONSE_BYTES - 128_000 {
        return Err(validation("the arrangement exceeds the bounded client response budget"));
    }
    Ok((Extension(snapshot), Json(resource)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    subject: String,
    owner: String,
    operations: Vec<st3_schema::arrangements::Operation>,
}

pub(super) async fn edit(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<ClaimRecord, ApiError> {
    let parameters: Edit = serde_json::from_value(request.parameters.clone())
        .map_err(|error| validation(error.to_string()))?;
    let owner = person(session, Some(&parameters.owner), true)?;
    if st3_schema::arrangements::owner(&parameters.subject)
        .map_err(|error| validation(error.message))? != owner
    {
        return Err(validation("arrangement owner must match its immutable subject owner"));
    }
    let input = ClaimInput {
        subject: parameters.subject,
        kind: "arrangement.edited".into(),
        actor: Some(session_claim_actor(session)),
        fields: BTreeMap::from([
            ("owner".into(), json!(owner)),
            ("operations".into(), serde_json::to_value(parameters.operations).map_err(ApiError::internal)?),
            ("action_id".into(), json!(request.id)),
            ("action_digest".into(), json!(action_request_digest(request)?)),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some(format!("arrangement-edit:{}", creation_key(session, request))),
    };
    let mut expected = request.fence.subject_revisions.clone();
    expected.retain(|subject, _| {
        !subject.starts_with("attention/") && !subject.starts_with("session/external-")
    });
    let store = state.store.clone();
    blocking_action(move || store.edit_arrangement(&input, &expected)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::test_state_named;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    const SUBJECT: &str = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";

    fn request(state: &AppState, key: &str, operations: Value) -> ActionRequest {
        serde_json::from_value(json!({
            "api_version":CLIENT_API_VERSION, "id":format!("action/{key}"),
            "type":"arrangement.edit", "idempotency_key":format!("arrangement-api-test-{key}"),
            "fence":{"snapshot_id":new_client_snapshot(state).id},
            "parameters":{"subject":SUBJECT,"owner":"person/ada","operations":operations}
        })).unwrap()
    }

    #[tokio::test]
    async fn arrangement_actions_merge_layout_fences_replay_receipts_and_preserve_owner() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "arrangement-actions");
        let agent = ClientSession::local(Some("agent/sidebar-editor")).unwrap();
        let create = request(&state, "create", json!([{"op":"create","name":"Main"}]));
        let first = action(State(state.clone()), Extension(new_client_snapshot(&state)),
            Extension(agent.clone()), Json(create.clone())).await.unwrap().0;
        let mut rename = request(&state, "rename", json!([{"op":"rename","name":"Shared"}]));
        rename.fence.subject_revisions.insert(SUBJECT.into(), first["arrangement_revision"].as_str().unwrap().into());
        let second = action(State(state.clone()), Extension(new_client_snapshot(&state)),
            Extension(agent.clone()), Json(rename)).await.unwrap().0;
        assert_ne!(first["arrangement_revision"], second["arrangement_revision"]);
        let mut placement = request(&state, "stale-layout", json!([{
            "op":"subject.place","subject":"agent/returned-seat","folder":null,"key":"a0"
        }]));
        placement.fence.subject_revisions.insert(SUBJECT.into(), first["arrangement_revision"].as_str().unwrap().into());
        let _ = action(State(state.clone()), Extension(new_client_snapshot(&state)),
            Extension(agent.clone()), Json(placement)).await.unwrap();
        let retry = action(State(state.clone()), Extension(new_client_snapshot(&state)),
            Extension(agent.clone()), Json(create.clone())).await.unwrap().0;
        assert_eq!(retry["arrangement_revision"], first["arrangement_revision"]);
        assert_eq!(retry["operation_id"], first["operation_id"]);
        let mut different = create;
        different.parameters["operations"] = json!([{"op":"create","name":"Different"}]);
        let error = action(State(state.clone()), Extension(new_client_snapshot(&state)),
            Extension(agent.clone()), Json(different)).await.unwrap_err();
        assert_eq!(error.code, "idempotency-conflict");
        let projected = state.store.arrangement(SUBJECT, u64::MAX).unwrap().unwrap();
        assert_eq!(projected["owner"], "person/ada");
        assert_eq!(projected["body"]["name"]["value"], "Shared");
        assert_eq!(projected["body"]["placements"]["agent/returned-seat"]["value"], json!({"folder":null,"key":"a0"}));
        let accepted = state.store.claims_for(SUBJECT, Some("arrangement.edited")).unwrap();
        assert!(accepted.iter().all(|claim| claim.actor.as_deref() == Some("agent/sidebar-editor")));

        let mut fenced = request(&state, "stale-authority", json!([{"op":"rename","name":"Refused"}]));
        fenced.fence.subject_revisions.insert("person/ada".into(), "claim/nonexistent".into());
        assert!(action(State(state.clone()), Extension(new_client_snapshot(&state)),
            Extension(agent), Json(fenced)).await.is_err());
        assert_eq!(state.store.arrangement(SUBJECT, u64::MAX).unwrap().unwrap()["body"]["name"]["value"], "Shared");
    }

    #[tokio::test]
    async fn arrangement_routes_and_collection_require_owner_and_remove_retired_ids() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "arrangement-reads");
        let agent = ClientSession::local(Some("agent/sidebar-reader")).unwrap();
        let _ = action(State(state.clone()), Extension(new_client_snapshot(&state)), Extension(agent.clone()),
            Json(request(&state, "create", json!([{"op":"create","name":"Main"}])))).await.unwrap();
        let app = crate::api::router(state.clone());
        for uri in ["/v1/client/arrangements?person=person%2Fada",
            "/v1/client/arrangements/ada/019a0000-0000-7000-8000-000000000001"] {
            let response = app.clone().oneshot(Request::builder().uri(uri)
                .header(LOCAL_PERSON_HEADER, "agent/sidebar-reader").body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES).await.unwrap()).unwrap();
            let resource = if uri.contains('?') { &body["value"]["items"][0] } else { &body["value"] };
            assert_eq!(resource["owner"], "person/ada", "{body}");
            assert_eq!(resource["body"]["name"]["value"], "Main", "{body}");
        }
        let subscription: CollectionSubscribe = serde_json::from_value(json!({
            "kind":"subscribe","id":"sidebar","collection":"arrangements","person":"person/ada","limit":10
        })).unwrap();
        let (_, before, _) = collection_items(&state, &agent, &subscription).await.unwrap();
        assert_eq!(before[0]["id"], SUBJECT);
        let mut omitted = subscription.clone();
        omitted.person = None;
        assert!(collection_items(&state, &agent, &omitted).await.is_err());
        let other = ClientSession::local(Some("person/grace")).unwrap();
        assert_eq!(collection_items(&state, &other, &subscription).await.unwrap_err().code, "forbidden");
        assert_eq!(person(&ClientSession::local(None).unwrap(), Some("person/ada"), false).unwrap_err().code, "forbidden");
        let _ = action(State(state.clone()), Extension(new_client_snapshot(&state)), Extension(agent.clone()),
            Json(request(&state, "retire", json!([{"op":"retire"}])))).await.unwrap();
        let (_, after, _) = collection_items(&state, &agent, &subscription).await.unwrap();
        assert!(after.is_empty());
        let response = app.oneshot(Request::builder()
            .uri("/v1/client/arrangements/ada/019a0000-0000-7000-8000-000000000001")
            .header(LOCAL_PERSON_HEADER, "agent/sidebar-reader").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn arrangement_collection_rechecks_revoked_paired_grants() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "arrangement-grants");
        let append = |kind: &str, fields: Value| state.store.append_claim(&ClaimInput {
            subject:"custom/client/pairing-arrangement-test".into(), kind:kind.into(),
            actor:Some("person/ada".into()),
            fields:serde_json::from_value(fields).unwrap(), evidence:vec![],
            expected_subject:None, idempotency_key:None,
        }).unwrap();
        let paired = append("custom.client.pairing-completed", json!({
            "session_actor":"client/device-arrangement-test", "person_id":"person/ada",
            "credential_hash":"test-not-a-secret", "expires_at_unix_ms":u64::MAX,
            "scopes":["read.projections","read.arrangements"]
        }));
        let session = paired_client_session(&state, &paired, "fabric-loopback").unwrap();
        let subscription: CollectionSubscribe = serde_json::from_value(json!({
            "kind":"subscribe","id":"sidebar","collection":"arrangements","person":"person/ada"
        })).unwrap();
        collection_items(&state, &session, &subscription).await.unwrap();
        append("custom.client.pairing-revoked", json!({"reason":"person revoked"}));
        assert_eq!(collection_items(&state, &session, &subscription).await.unwrap_err().code, "forbidden");
    }

    #[tokio::test]
    async fn arrangement_pages_keep_their_read_snapshot_across_later_edits() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "arrangement-pages");
        for id in 1..=3 {
            state.store.append_claim(&ClaimInput {
                subject:format!("arrangement/person/ada/019a0000-0000-7000-8000-{id:012x}"),
                kind:"arrangement.edited".into(), actor:Some("person/ada".into()),
                fields:serde_json::from_value(json!({"owner":"person/ada","operations":[{"op":"create","name":format!("Sidebar {id}")}]})).unwrap(),
                evidence:vec![], expected_subject:None, idempotency_key:None,
            }).unwrap();
        }
        let app = crate::api::router(state.clone());
        let get_page = |uri: String| {
            let app = app.clone();
            async move {
                let response = app.oneshot(Request::builder().uri(uri).header(LOCAL_PERSON_HEADER,"person/ada")
                    .body(Body::empty()).unwrap()).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                serde_json::from_slice::<Value>(&to_bytes(response.into_body(),CLIENT_MAX_RESPONSE_BYTES).await.unwrap()).unwrap()
            }
        };
        let first = get_page("/v1/client/arrangements?person=person%2Fada&limit=1".into()).await;
        let agent = ClientSession::local(Some("agent/editor")).unwrap();
        let _ = action(State(state.clone()),Extension(new_client_snapshot(&state)),Extension(agent),
            Json(request(&state,"rename-during-pages",json!([{"op":"rename","name":"Changed after page"}])))).await.unwrap();
        let mut page = first.clone();
        let mut names = vec![];
        loop {
            assert_eq!(page["snapshot"]["id"], first["snapshot"]["id"]);
            names.push(page["value"]["items"][0]["body"]["name"]["value"].as_str().unwrap().to_owned());
            let Some(cursor) = page["value"]["page"]["next_cursor"].as_str() else { break; };
            page = get_page(format!("/v1/client/arrangements?person=person%2Fada&limit=1&cursor={cursor}")).await;
        }
        assert_eq!(names, ["Sidebar 1","Sidebar 2","Sidebar 3"]);
    }

    #[tokio::test]
    async fn arrangement_edit_recovers_receipt_gap_without_accepting_a_changed_action_id() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "arrangement-receipt-gap");
        let session = ClientSession::local(Some("person/ada")).unwrap();
        let original = request(&state,"receipt-gap",json!([{"op":"create","name":"Recovered"}]));
        let accepted = edit(&state,&session,&original).await.unwrap();
        // The durable edit committed but dispatch died before saving the action receipt.
        let mut changed = original.clone();
        changed.id = "action/changed-identity".into();
        assert!(action(State(state.clone()),Extension(new_client_snapshot(&state)),
            Extension(session.clone()),Json(changed)).await.is_err());
        let recovered = action(State(state.clone()),Extension(new_client_snapshot(&state)),
            Extension(session),Json(original)).await.unwrap().0;
        assert_eq!(recovered["action_id"], "action/receipt-gap");
        assert_eq!(recovered["arrangement_revision"], accepted.id);
        assert_eq!(state.store.claims_for(SUBJECT,Some("arrangement.edited")).unwrap().len(),1);
    }

    #[tokio::test]
    async fn arrangement_actions_accept_current_projected_launch_fences_and_refuse_stale_ones() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "arrangement-launch-fences");
        let session = ClientSession::local(Some("person/ada")).unwrap();
        let launch = state.store.create_planning_session(
            "sidebar-launch","example","request","workspace","person/ada","agent/planner",
            &crate::model::PlannerSpec::default(),None,None,
        ).unwrap();
        let mut create = request(&state,"launch-fenced-create",json!([{"op":"create","name":"Main"}]));
        create.fence.subject_revisions.insert(format!("launch/{}",launch.id),format!("launch/{}",launch.updated_at_unix_ms));
        let _ = action(State(state.clone()),Extension(new_client_snapshot(&state)),Extension(session.clone()),
            Json(create)).await.unwrap();
        let mut stale_launch = request(&state,"launch-fenced-stale",json!([{"op":"rename","name":"Refused"}]));
        stale_launch.fence.subject_revisions.insert(format!("launch/{}",launch.id),format!("launch/{}",launch.updated_at_unix_ms.saturating_sub(1)));
        assert!(action(State(state.clone()),Extension(new_client_snapshot(&state)),Extension(session),
            Json(stale_launch)).await.is_err());
        assert_eq!(state.store.arrangement(SUBJECT,u64::MAX).unwrap().unwrap()["body"]["name"]["value"],"Main");
    }
}
