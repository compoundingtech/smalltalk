//! Native model changes are reserved only by the observed runtime owner.
use super::*;
use st3_schema::harness_control::{Binding, ModelParameters, ModelRequest};

pub(super) async fn mutate(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Value, ApiError> {
    if !session
        .authority_actor
        .strip_prefix("person/")
        .is_some_and(|person| !person.is_empty() && !person.contains('/'))
    {
        return Err(forbidden("harness model control requires a concrete person"));
    }
    let parameters: ModelParameters = serde_json::from_value(request.parameters.clone())
        .map_err(|error| validation(error.to_string()))?;
    if !parameters
        .subject
        .strip_prefix("agent/")
        .is_some_and(|agent| !agent.is_empty())
    {
        return Err(validation("harness target must be an agent subject"));
    }
    if request.fence.runtime_incarnation.as_deref()
        != Some(parameters.binding.incarnation_id.as_str())
        || request.fence.runtime_desired_revision.as_deref()
            != Some(parameters.binding.desired_revision.as_str())
    {
        return Err(validation("native binding must match both runtime fences"));
    }
    let host = harness_control::owner(state, &parameters.subject)?;
    if host != client_host_id(&state.node) {
        let mut result = harness_control::relay(
            state,
            session,
            &host,
            crate::peer::ClientReadOperation::HarnessModelMutation {
                action_id: request.id.clone(),
                idempotency_key: request.idempotency_key.clone(),
                parameters: request.parameters.clone(),
            },
        )
        .await?;
        // The gateway envelopes the unwrapped owner result with its own snapshot.
        result["snapshot_id"] = Value::String(new_client_snapshot(state).id);
        return Ok(result);
    }
    let receipt = state
        .store
        .reserve_harness_model(&ModelRequest {
            actor: session.authority_actor.clone(),
            idempotency_key: request.idempotency_key.clone(),
            parameters,
        })
        .map_err(ApiError::bad)?;
    signal_local_change(state);
    Ok(json!({
        "kind": "action-result",
        "action_id": request.id,
        "operation_id": receipt.operation_id,
        "status": receipt.status,
        "affected_ids": [&receipt.subject],
        "harness_model": receipt,
        "snapshot_id": new_client_snapshot(state).id,
    }))
}

const MODEL_PAGE_BYTES: usize = 512 * 1024;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ModelQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCursor {
    subject: String,
    binding: Binding,
    model_revision: String,
    offset: usize,
}

fn encode_model_cursor(state: &AppState, cursor: &ModelCursor) -> Result<String, ApiError> {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(cursor).map_err(ApiError::internal)?);
    let signature = derive_terminal_capability(
        state, "harness-models-cursor.v1", &payload, &client_host_id(&state.node),
    )?;
    Ok(format!("harness-models/{payload}.{signature}"))
}

fn decode_model_cursor(state: &AppState, encoded: &str) -> Result<ModelCursor, ApiError> {
    if encoded.len() > MODEL_PAGE_BYTES {
        return Err(validation("the model catalogue cursor exceeds its bound"));
    }
    let (payload, signature) = encoded.strip_prefix("harness-models/")
        .and_then(|encoded| encoded.split_once('.'))
        .ok_or_else(|| validation("the model catalogue cursor is malformed"))?;
    let expected = derive_terminal_capability(
        state, "harness-models-cursor.v1", payload, &client_host_id(&state.node),
    )?;
    if expected.len() != signature.len() || expected.bytes().zip(signature.bytes())
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right)) != 0
    {
        return Err(validation("the model catalogue cursor signature is invalid"));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| validation("the model catalogue cursor is malformed"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| validation("the model catalogue cursor is malformed"))
}

#[derive(Default)]
struct JsonByteCount(usize);

impl std::io::Write for JsonByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_bytes(value: &impl Serialize) -> Result<usize, ApiError> {
    let mut counter = JsonByteCount::default();
    serde_json::to_writer(&mut counter, value).map_err(ApiError::internal)?;
    Ok(counter.0)
}

pub(in crate::api) async fn models(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(subject): AxumPath<String>,
    Query(query): Query<ModelQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=100).contains(&limit) {
        return Err(validation("model catalogue limit must be between 1 and 100"));
    }
    let subject = if subject.starts_with("agent/") {
        subject
    } else {
        format!("agent/{subject}")
    };
    let host = harness_control::owner(&state, &subject)?;
    if host != client_host_id(&state.node) {
        return Ok(Json(harness_control::relay(
            &state,
            &session,
            &host,
            crate::peer::ClientReadOperation::HarnessModels {
                subject,
                cursor: query.cursor,
                limit: query.limit,
            },
        ).await?));
    }
    let cursor = query.cursor.as_deref()
        .map(|cursor| decode_model_cursor(&state, cursor)).transpose()?;
    if cursor.as_ref().is_some_and(|cursor| cursor.subject != subject) {
        return Err(client_page_expired("the model catalogue cursor names another subject"));
    }
    let native = state.store.harness_control_state(&subject).map_err(ApiError::bad)?
        .ok_or_else(|| ApiError::bad(St3Error::new(
            "unsupported-harness-control",
            "this runtime has not reported a native model catalogue",
        )))?;
    if cursor.as_ref().is_some_and(|cursor| {
        !native.models.available
            || cursor.binding.desired_revision != native.binding.desired_revision
            || cursor.binding.incarnation_id != native.binding.incarnation_id
            || cursor.binding.session_id != native.binding.session_id
            || cursor.model_revision != native.models.revision
    }) {
        return Err(client_page_expired("the native model catalogue changed; restart pagination"));
    }
    let total = native.models.choices.len();
    let start = cursor.as_ref().map_or(0, |cursor| cursor.offset);
    if start > total {
        return Err(client_page_expired("the model catalogue cursor is outside the catalogue"));
    }
    let mut continuation = ModelCursor {
        subject: subject.clone(),
        binding: native.binding,
        model_revision: native.models.revision.clone(),
        offset: total,
    };
    // The largest possible continuation reserves room before counting choices.
    let mut page = json!({
        "schema": "harness-models.v1",
        "subject": subject,
        "model_revision": native.models.revision,
        "selected": native.models.selected,
        "atomic_model_effort": false,
        "available": native.models.available,
        "complete": native.models.complete,
        "source": native.models.source,
        "total": total,
        "choices": [],
        "cursor": null,
    });
    let mut bytes = json_bytes(&page)?;
    if start < total {
        // Base64url has no padding/escaping; a SHA-256 MAC occupies 43 characters.
        let payload_bytes = json_bytes(&continuation)?;
        bytes += "harness-models/".len() + (payload_bytes * 4).div_ceil(3)
            + 1 + 43 + 2 - 4; // dot, MAC, quotes, replacing JSON null
    }
    if bytes > MODEL_PAGE_BYTES {
        return Err(ApiError::bad(St3Error::new(
            "blob-too-large", "native model catalogue metadata exceeds the page byte bound",
        )));
    }
    let mut choices = Vec::with_capacity(limit.min(total - start));
    for choice in native.models.choices.into_iter().skip(start).take(limit) {
        let size = json_bytes(&choice)? + usize::from(!choices.is_empty());
        if size > MODEL_PAGE_BYTES - bytes {
            if choices.is_empty() {
                return Err(ApiError::bad(St3Error::new(
                    "blob-too-large", "a native model choice exceeds the page byte bound",
                )));
            }
            break;
        }
        bytes += size;
        choices.push(serde_json::to_value(choice).map_err(ApiError::internal)?);
    }
    continuation.offset = start + choices.len();
    page["cursor"] = if continuation.offset < total {
        Value::String(encode_model_cursor(&state, &continuation)?)
    } else {
        Value::Null
    };
    page["choices"] = Value::Array(choices);
    Ok(Json(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(root: &Path, node: &str) -> AppState {
        AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        }
    }

    fn request(state: &AppState) -> Value {
        json!({
            "api_version": CLIENT_API_VERSION,
            "id": "action/model-boundary",
            "type": "harness.model.set",
            "idempotency_key": "model-boundary-idempotency-key",
            "fence": {
                "snapshot_id": new_client_snapshot(state).id,
                "runtime_incarnation": "incarnation-one",
                "runtime_desired_revision": "desired-one",
            },
            "parameters": {
                "subject": "agent/model-worker",
                "binding": {
                    "incarnation_id": "incarnation-one",
                    "desired_revision": "desired-one",
                    "session_id": "native-one",
                    "turn_id": null,
                },
                "model_revision": "models-one",
                "provider": "provider-one",
                "model_id": "model-one",
            },
        })
    }

    async fn submit(app: Router, request: &Value, identity: (&str, &str)) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/client/actions")
                    .header("content-type", "application/json")
                    .header(identity.0, identity.1)
                    .body(Body::from(serde_json::to_vec(request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn router_requires_person_and_exact_action_binding_fences() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path(), "model-boundary");
        let app = crate::api::router(state.clone());
        let original = request(&state);
        let identity = (LOCAL_PERSON_HEADER, "person/ada");
        let (status, _) = submit(app.clone(), &original, identity).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        for actor in ["agent/operator", "person/", "person/ada/session/one"] {
            let (status, response) = submit(app.clone(), &original, (LOCAL_PERSON_HEADER, actor)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{actor}: {response}");
        }
        for key in ["runtime_incarnation", "runtime_desired_revision"] {
            for replacement in [Value::Null, json!("different")] {
                let mut changed = original.clone();
                changed["fence"][key] = replacement;
                let (status, response) = submit(app.clone(), &changed, identity).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
                assert_eq!(response["code"], "validation-failed");
            }
        }
        for subject in ["person/ada", "agent/"] {
            let mut changed = original.clone();
            changed["parameters"]["subject"] = json!(subject);
            assert_eq!(submit(app.clone(), &changed, identity).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        }
        for location in ["parameters", "binding"] {
            let mut changed = original.clone();
            if location == "parameters" {
                changed["parameters"]["unexpected"] = json!(true);
            } else {
                changed["parameters"]["binding"]["unexpected"] = json!(true);
            }
            assert_eq!(submit(app.clone(), &changed, identity).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        }
        let mut selected_actor = original;
        selected_actor["parameters"]["actor"] = json!("person/other");
        assert_eq!(submit(app, &selected_actor, identity).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn router_requires_existing_runtime_scope_for_paired_person() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path(), "model-scope");
        let app = crate::api::fabric_router(state.clone());
        let credential = "paired-model-person";
        for (scopes, expected) in [
            (json!(["control.messages"]), StatusCode::FORBIDDEN),
            (json!(["control.runtimes"]), StatusCode::NOT_FOUND),
        ] {
            state.store.append_claim(&ClaimInput {
                subject: "custom/client/model-person".into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::from([
                    ("credential_hash".into(), json!(credential_digest(credential))),
                    ("session_actor".into(), json!("client/model-person")),
                    ("person_id".into(), json!("person/ada")),
                    ("scopes".into(), scopes),
                    ("expires_at_unix_ms".into(), json!(client_now_ms() as u64 + 60_000)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            }).unwrap();
            for queue in [false, true] {
                let mut action = request(&state);
                if queue {
                    action["type"] = json!("harness.queue.mutate");
                    action["parameters"] = json!({
                        "subject": "agent/model-worker",
                        "binding": action["parameters"]["binding"],
                        "queue_revision": 0,
                        "mutation": {"type":"enqueue", "content":"scope boundary", "lane":"follow_up"},
                    });
                }
                let (status, response) = submit(
                    app.clone(),
                    &action,
                    ("authorization", &format!("Bearer {credential}")),
                ).await;
                assert_eq!(status, expected, "{response}");
            }
        }
    }

    #[tokio::test]
    async fn gateway_without_owner_route_does_not_reserve_locally() {
        let owner_root = tempfile::tempdir().unwrap();
        let gateway_root = tempfile::tempdir().unwrap();
        let owner = state(owner_root.path(), "model-owner");
        let gateway = state(gateway_root.path(), "model-gateway");
        owner.store.append_claim(&ClaimInput {
            subject: "agent/model-worker".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/model-worker".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), json!("model-runtime")),
                ("incarnation_id".into(), json!("incarnation-one")),
                ("status".into(), json!("running")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();
        gateway.store.import_replication("model-owner", &owner.store.export_replication(0).unwrap()).unwrap();
        let index = gateway.store.index().unwrap();
        let (status, response) = submit(
            crate::api::router(gateway.clone()),
            &request(&gateway),
            (LOCAL_PERSON_HEADER, "person/ada"),
        ).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
        assert_eq!(response["code"], "remote-unavailable");
        assert_eq!(response["details"]["owner_host_id"], "host/model-owner");
        assert_eq!(gateway.store.index().unwrap(), index);
    }

    #[tokio::test]
    async fn catalogue_pages_bound_escaped_bytes_and_expire_on_native_revision_change() {
        use st3_schema::harness_control::{Approval, ModelChoice, Models, NativeState};
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path(), "model-catalogue");
        let source = "version 2\nagent \"model-worker\" { workspace \"/tmp\"; command \"true\" }";
        let intent = crate::graph::parse_intent(source, &state.node).unwrap();
        let planned = state.store.mission(&intent, IntentInput {
            kdl: source.into(), source_name: None,
        }).unwrap();
        assert!(planned.blockers.is_empty(), "{:?}", planned.blockers);
        state.store.apply(&intent, &planned.subject_tokens, source).unwrap();
        state.store.append_claim(&ClaimInput {
            subject: "agent/model-catalogue.model-worker".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/model-catalogue.model-worker".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), json!("native-runtime")),
                ("incarnation_id".into(), json!("incarnation-one")),
                ("status".into(), json!("running")),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let fence = state.store.bind_mailbox(&crate::mailbox::Fence::new(
            "agent/model-catalogue.model-worker", "incarnation-one", "delivery",
        )).unwrap();
        let choices = (0..3).map(|index| ModelChoice {
            provider: "native".into(),
            id: format!("{index}{}", "\\".repeat(100_000)),
            reasoning: true,
            supported_efforts: vec!["high".into()],
        }).collect::<Vec<_>>();
        let expected_ids = choices.iter().map(|choice| choice.id.clone()).collect::<Vec<_>>();
        let mut native = NativeState {
            subject: "agent/model-catalogue.model-worker".into(),
            binding: Binding {
                desired_revision: state.store.harness_control_desired_revision(&fence).unwrap(),
                incarnation_id: "incarnation-one".into(),
                session_id: "native-one".into(),
                turn_id: None,
            },
            idle: true,
            input_supported: true,
            steer: Default::default(),
            models: Models {
                choices, selected: None, revision: "models-one".into(),
                atomic_model_effort: false, available: true, complete: true,
                source: "native-extension-model-registry".into(),
            },
            approval: Approval { supported: false, reason: "native-live-approval-api-unavailable".into() },
            reason: None,
        };
        state.store.observe_harness_control(&native, &fence).unwrap();
        let app = crate::api::router(state.clone());
        let read = |uri: String| {
            let app = app.clone();
            async move {
                let response = app.oneshot(Request::builder()
                    .uri(uri).header(LOCAL_PERSON_HEADER, "person/ada")
                    .body(Body::empty()).unwrap()).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let path = "/v1/client/harness-models/agent/model-catalogue.model-worker";
        let (status, first) = read(format!("{path}?limit=100")).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let first = &first["value"];
        assert!(json_bytes(first).unwrap() <= MODEL_PAGE_BYTES);
        assert_eq!(first["total"], 3);
        assert_eq!(first["complete"], true);
        assert_eq!(first["atomic_model_effort"], false);
        assert_eq!(first["source"], "native-extension-model-registry");
        let cursor = first["cursor"].as_str().unwrap();
        let (status, second) = read(format!("{path}?cursor={}", urlencoding::encode(cursor))).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert!(json_bytes(&second["value"]).unwrap() <= MODEL_PAGE_BYTES);
        assert!(second["value"]["cursor"].is_null());
        let actual_ids = first["choices"].as_array().unwrap().iter()
            .chain(second["value"]["choices"].as_array().unwrap())
            .map(|choice| choice["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(actual_ids, expected_ids);
        let (_, signature) = cursor.rsplit_once('.').unwrap();
        let mut forged = decode_model_cursor(&state, cursor).unwrap();
        forged.offset = 0;
        let forged_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&forged).unwrap());
        let forged_cursor = format!("harness-models/{forged_payload}.{signature}");
        assert_eq!(read(format!("{path}?cursor={}", urlencoding::encode(&forged_cursor))).await.0,
            StatusCode::UNPROCESSABLE_ENTITY);
        for limit in [0, 101] {
            assert_eq!(read(format!("{path}?limit={limit}")).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        }
        native.models.revision = "models-two".into();
        state.store.observe_harness_control(&native, &fence).unwrap();
        let (status, expired) = read(format!("{path}?cursor={}", urlencoding::encode(cursor))).await;
        assert_eq!(status, StatusCode::GONE, "{expired}");
        assert_eq!(expired["code"], "page-cursor-expired");
        let (status, current) = read(format!("{path}?limit=1")).await;
        assert_eq!(status, StatusCode::OK, "{current}");
        let current_cursor = current["value"]["cursor"].as_str().unwrap();
        state.store.append_claim(&ClaimInput {
            subject: "agent/model-catalogue.model-worker".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/model-catalogue.model-worker".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), json!("native-runtime")),
                ("incarnation_id".into(), json!("incarnation-one")),
                ("status".into(), json!("stopped")),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let (status, ended) = read(path.into()).await;
        assert_eq!(status, StatusCode::OK, "{ended}");
        assert_eq!(ended["value"]["available"], false);
        let (status, expired) = read(format!("{path}?cursor={}", urlencoding::encode(current_cursor))).await;
        assert_eq!(status, StatusCode::GONE, "{expired}");
        assert_eq!(expired["code"], "page-cursor-expired");
    }
}
