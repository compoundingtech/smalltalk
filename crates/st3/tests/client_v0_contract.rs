use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use st3::api::AppState;
use st3::store::Store;
use tokio::sync::{Barrier, Notify, watch};
use tower::ServiceExt as _;

fn asset_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/st3/client-v0")
}

fn json(path: impl AsRef<Path>) -> Value {
    let path = path.as_ref();
    serde_json::from_slice(
        &std::fs::read(path)
            .unwrap_or_else(|error| panic!("read client v0 asset {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse client v0 asset {}: {error}", path.display()))
}

fn fixture(name: &str) -> Value {
    json(asset_root().join("fixtures").join(name))
}

/// Validate the published wire contract, with closed resources for producer drift proof.
/// ResourceHeader and resource-specific fields are composed with allOf, so closing either
/// branch with additionalProperties would reject the other branch's legitimate fields.
/// Draft 2020-12 unevaluatedProperties closes their union without flattening or duplicating it.
fn contract_validator(definition: &str) -> jsonschema::Validator {
    let mut schema = json(asset_root().join("schemas/client-v0.schema.json"));
    // Consumers preserve future cases, but this daemon must emit only declared cases.
    // Tighten every open enum (`anyOf: [{enum: known}, {type: string}]`) in a test-only copy.
    fn strict_known_cases(value: &mut Value) {
        match value {
            Value::Object(object) => {
                let known = object.get("anyOf").and_then(Value::as_array).and_then(|cases| {
                    if cases.len() == 2 && cases[1]["type"] == "string" {
                        cases[0].get("enum").cloned()
                    } else {
                        None
                    }
                });
                if let Some(known) = known {
                    object.remove("anyOf");
                    object.insert("enum".into(), known);
                }
                for child in object.values_mut() {
                    strict_known_cases(child);
                }
            }
            Value::Array(array) => {
                for child in array {
                    strict_known_cases(child);
                }
            }
            _ => {}
        }
    }
    strict_known_cases(&mut schema);
    if let Some(cases) = schema["$defs"]["Resource"]["oneOf"].as_array_mut() {
        cases.retain(|case| case["$ref"] != "#/$defs/UnknownResource");
    }
    for resource in schema["$defs"].as_object_mut().unwrap().values_mut() {
        if resource["allOf"].as_array().is_some_and(|branches| {
            branches
                .iter()
                .any(|branch| branch["$ref"] == "#/$defs/ResourceHeader")
        }) {
            resource["unevaluatedProperties"] = Value::Bool(false);
        }
    }
    compile_contract_validator(schema, definition)
}

fn consumer_validator(definition: &str) -> jsonschema::Validator {
    compile_contract_validator(
        json(asset_root().join("schemas/client-v0.schema.json")),
        definition,
    )
}

fn compile_contract_validator(mut schema: Value, definition: &str) -> jsonschema::Validator {
    schema.as_object_mut().unwrap().remove("oneOf");
    schema["$ref"] = Value::String(format!("#/$defs/{definition}"));
    // Unknown x-st-* annotation keywords are ignored by the standard validator.
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|error| panic!("compile {definition} contract: {error}"))
}

fn assert_conforms(validator: &jsonschema::Validator, context: &str, value: &Value) {
    if !validator.is_valid(value) {
        let errors = validator
            .iter_errors(value)
            .map(|error| {
                format!(
                    "{} (schema {}): {error}",
                    error.instance_path, error.schema_path
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{context} violates client-v0:\n{errors}\n{value}");
    }
}

#[test]
fn consumers_accept_future_enum_cases_but_producers_reject_them() {
    for (definition, known) in [
        ("MissionState", "running"),
        ("MustAct", "you"),
        ("RunOutcomeStatus", "completed"),
        ("AgentState", "running"),
        ("AgentReachability", "reachable"),
        ("TimelineStatus", "running"),
        ("ErrorCode", "attention-migrated"),
    ] {
        let consumer = consumer_validator(definition);
        let producer = contract_validator(definition);
        let known = Value::String(known.into());
        assert_conforms(&consumer, definition, &known);
        assert_conforms(&producer, definition, &known);
        let future = Value::String("future-case".into());
        assert_conforms(&consumer, definition, &future);
        assert!(
            !producer.is_valid(&future),
            "{definition} producer accepted a future case"
        );
        assert!(!consumer.is_valid(&Value::Null), "{definition} accepted null");
        assert!(
            !consumer.is_valid(&Value::from(1)),
            "{definition} accepted a number"
        );
    }
}

#[test]
fn future_resources_remain_readable_without_bypassing_known_resource_validation() {
    let consumer = consumer_validator("Resource");
    let producer = contract_validator("Resource");
    let mut resource = serde_json::json!({
        "id": "future-widget/example",
        "kind": "future-widget",
        "revision": "r1",
        "updated_at": "2026-10-01T10:00:00.000Z",
        "future_payload": {"answer": 42}
    });
    assert_conforms(&consumer, "future resource", &resource);
    assert!(!producer.is_valid(&resource));

    resource["id"] = Value::String("invalid-id".into());
    assert!(
        !consumer.is_valid(&resource),
        "future resources still require valid headers"
    );
    resource["id"] = Value::String("mission/example".into());
    for kind in ["mission", "agent", "glass", "machine"] {
        resource["kind"] = Value::String(kind.into());
        assert!(
            !consumer.is_valid(&resource),
            "malformed known {kind} must not escape through UnknownResource"
        );
    }

    for known in fixture("resources.json").as_array().unwrap() {
        assert_conforms(&consumer, "known resource", known);
        assert_conforms(&producer, "known resource", known);
    }
    assert_conforms(
        &consumer_validator("Envelope"),
        "glass page",
        &fixture("glasses.json"),
    );
    assert_conforms(
        &contract_validator("Envelope"),
        "glass page",
        &fixture("glasses.json"),
    );
}

#[test]
fn future_timeline_types_preserve_payloads_without_weakening_known_bodies() {
    let consumer = consumer_validator("TimelineEntry");
    let producer = contract_validator("TimelineEntry");
    let mut entry = fixture("timeline.json")["value"]["items"][0].clone();
    assert_conforms(&consumer, "known status entry", &entry);
    assert_conforms(&producer, "known status entry", &entry);
    entry["body"]["status"] = Value::String("future-status".into());
    assert_conforms(&consumer, "future status", &entry);
    assert!(!producer.is_valid(&entry));

    entry["type"] = Value::String("future-entry".into());
    entry["body"] = serde_json::json!({"future_payload": [1, true, null]});
    assert_conforms(&consumer, "future timeline type", &entry);
    assert!(!producer.is_valid(&entry));
    entry["type"] = Value::String("content".into());
    assert!(
        !consumer.is_valid(&entry),
        "known content still requires its typed body"
    );
}

#[test]
fn family_references_reject_cross_family_and_malformed_ids() {
    for (definition, family) in [
        ("MissionId", "mission"),
        ("MissionRunId", "mission-run"),
        ("RunGenerationId", "run-generation"),
        ("StepRunId", "step-run"),
        ("AgentId", "agent"),
        ("HostId", "host"),
        ("RuntimeId", "runtime"),
        ("TerminalId", "terminal"),
        ("SnapshotId", "snapshot"),
        ("RequestId", "request"),
        ("TimelineEntryId", "timeline-entry"),
    ] {
        for validator in [consumer_validator(definition), contract_validator(definition)] {
            assert_conforms(
                &validator,
                definition,
                &Value::String(format!("{family}/nested/example")),
            );
            for invalid in [
                "wrong-family/example".to_owned(),
                format!("{family}/"),
                format!("{family}/white space"),
                format!("{family}/line\nbreak"),
                family.to_owned(),
            ] {
                let invalid = Value::String(invalid);
                assert!(!validator.is_valid(&invalid), "{definition}: {invalid}");
            }
        }
    }
    for (definition, allowed, rejected) in [
        (
            "ActorRef",
            ["agent/example", "daemon/example", "person/example"],
            "step/example",
        ),
        (
            "ParticipantRef",
            ["agent/example", "person/example", "step/example"],
            "daemon/example",
        ),
    ] {
        for validator in [consumer_validator(definition), contract_validator(definition)] {
            for valid in &allowed {
                assert_conforms(&validator, definition, &Value::String((*valid).into()));
            }
            assert!(!validator.is_valid(&Value::String(rejected.into())));
        }
    }
}

#[test]
fn resource_fields_and_generation_map_keys_enforce_family_references() {
    let resources = fixture("resources.json");
    for kind in ["mission", "agent"] {
        let original = resources
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["kind"] == kind)
            .unwrap();
        for validator in [consumer_validator("Resource"), contract_validator("Resource")] {
            assert_conforms(&validator, kind, original);
            let mut wrong_id = original.clone();
            wrong_id["id"] = Value::String("wrong-family/example".into());
            assert!(
                !validator.is_valid(&wrong_id),
                "{kind} accepted a cross-family id"
            );
        }
    }
    let mission = resources
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["kind"] == "mission")
        .unwrap();
    for validator in [consumer_validator("Resource"), contract_validator("Resource")] {
        for generations in [
            serde_json::json!({"agent/example": "run-generation/example"}),
            serde_json::json!({"mission-run/": "run-generation/example"}),
            serde_json::json!({"mission-run/white space": "run-generation/example"}),
            serde_json::json!({"mission-run/example": "mission-run/example"}),
        ] {
            let mut invalid = mission.clone();
            invalid["run_generations"] = generations;
            assert!(
                !validator.is_valid(&invalid),
                "invalid run generation map: {invalid}"
            );
        }
        let mut wrong_run = mission.clone();
        wrong_run["runs"] = serde_json::json!(["agent/example"]);
        assert!(!validator.is_valid(&wrong_run));
    }
}

#[tokio::test]
async fn outbound_rust_collection_commands_conform_with_none_options() {
    use axum::extract::ws::WebSocketUpgrade;
    use futures_util::StreamExt as _;
    let validator = contract_validator("CollectionCommand");
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/v1/client/collections/stream",
        axum::routing::get(move |upgrade: WebSocketUpgrade| {
            let sender = sender.clone();
            async move {
                upgrade.protocols(["st3.client.collections.v0"]).on_upgrade(
                    move |mut socket| async move {
                        while let Some(Ok(message)) = socket.next().await {
                            if let Ok(text) = message.to_text() {
                                sender
                                    .send(serde_json::from_str::<Value>(text).unwrap())
                                    .unwrap();
                            }
                        }
                    },
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client =
        st3_client::Client::fabric_loopback(format!("http://{address}"), "synthetic-token");
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe("agents", "agents", 100, None, None)
        .await
        .unwrap();
    stream.subscribe_glasses("glasses").await.unwrap();
    stream
        .subscribe_terminal("term", "terminal/example", None, "synthetic-capability")
        .await
        .unwrap();
    for expected in ["agents", "glasses", "term"] {
        let command = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(command["id"], expected);
        assert_conforms(&validator, "actual Rust None command", &command);
        let mut omitted = command.clone();
        if expected == "term" {
            assert_eq!(command.get("incarnation"), Some(&Value::Null));
            omitted.as_object_mut().unwrap().remove("incarnation");
            let mut missing_capability = omitted.clone();
            missing_capability
                .as_object_mut()
                .unwrap()
                .remove("capability");
            assert!(!validator.is_valid(&missing_capability));
        } else {
            assert_eq!(command.get("actor"), Some(&Value::Null));
            assert_eq!(command.get("status"), Some(&Value::Null));
            omitted.as_object_mut().unwrap().remove("actor");
            omitted.as_object_mut().unwrap().remove("status");
            let mut person = omitted.clone();
            person["person"] = Value::Null;
            assert_conforms(&validator, "nullable person filter", &person);
            person["person"] = serde_json::json!(42);
            assert!(!validator.is_valid(&person));
        }
        assert_conforms(&validator, "omitted optional command values", &omitted);
        let mut extra = command.clone();
        extra["undeclared"] = Value::Bool(true);
        assert!(!validator.is_valid(&extra));
        let mut bad_type = command;
        bad_type[if expected == "term" {
            "incarnation"
        } else {
            "actor"
        }] = serde_json::json!(false);
        assert!(!validator.is_valid(&bad_type));
    }
    stream.close().await;
    server.abort();
}

#[tokio::test]
async fn strict_daemon_contract_rejects_undeclared_fields_and_unknown_known_cases() {
    let root = tempfile::tempdir().unwrap();
    let app = st3::api::router(test_state(root.path()));
    let (status, emitted) = client_json(app, "/v1/client/machines").await;
    assert_eq!(status, StatusCode::OK, "{emitted}");
    let validator = contract_validator("Envelope");
    assert_conforms(&validator, "emitted machine page", &emitted);
    let machine = &emitted["value"]["items"][0];
    assert_eq!(machine["kind"], "machine");
    assert_eq!(machine["state"], "local");

    // Each negative starts with a real emitted payload that the validator accepted.
    let mut extra_field = emitted.clone();
    extra_field["value"]["items"][0]["undeclared_resource_field"] = Value::Bool(true);
    assert!(
        !validator.is_valid(&extra_field),
        "producer-only resource fields must not silently escape the contract"
    );
    let mut unknown_state = emitted.clone();
    unknown_state["value"]["items"][0]["state"] = Value::String("future-state".into());
    assert!(
        !validator.is_valid(&unknown_state),
        "known enum families remain strict in the producer proof"
    );
    let mut wrong_kind = emitted;
    wrong_kind["value"]["items"][0]["kind"] = Value::String("host".into());
    assert!(
        !validator.is_valid(&wrong_kind),
        "Machine must not regress to an undeclared resource kind"
    );
}

#[tokio::test]
async fn daemon_conversation_and_terminal_frames_conform_to_client_v0() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let frame_validator = contract_validator("CollectionFrame");
    let envelope_validator = contract_validator("Envelope");
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let state = test_state(root.path());
    let agent = "agent/example/worker";
    let runtime = "contract-terminal";
    let incarnation = "contract-terminal:i1";
    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: serde_json::from_value(serde_json::json!({
                "runtime_id": runtime, "incarnation_id": incarnation,
                "status": "running", "terminal": true, "reachability": "local"
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let app = st3::api::router(state.clone());
    let (status, agents) = client_json(app.clone(), "/v1/client/agents").await;
    assert_eq!(status, StatusCode::OK, "{agents}");
    assert_conforms(&envelope_validator, "live terminal agent", &agents);
    let session_id = agents["value"]["items"][0]["current_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: "message/contract-first".into(),
            kind: "message.sent".into(),
            actor: Some("person/alex".into()),
            fields: serde_json::from_value(serde_json::json!({
                "from": "person/alex", "to": agent, "session_id": session_id,
                "content": "A synthetic request.", "status": "sent"
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();

    let server_socket = socket.clone();
    let server_app = app.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, server_app)
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let client = st3_client::Client::unix_as(&socket, "person/alex");
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe_conversation("talk", agent)
        .await
        .unwrap();
    let conversation = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(conversation["kind"], "conversation");
    assert_eq!(conversation["replace"], true);
    assert_conforms(&frame_validator, "initial conversation frame", &conversation);
    let message = conversation["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["type"] == "message")
        .unwrap();
    assert_eq!(message["body"]["message_id"], "message/contract-first");
    assert_eq!(message["body"].get("reply_to"), Some(&Value::Null));
    let (status, changes) = client_json(
        app.clone(),
        &format!("/v1/client/conversations/{}/changes", urlencoding::encode(agent)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changes}");
    assert_conforms(&envelope_validator, "conversation changes envelope", &changes);

    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: agent.into(),
            kind: "harness.timeline".into(),
            actor: Some(agent.into()),
            fields: serde_json::from_value(serde_json::json!({
                "operation": "append", "entry_id": "timeline-entry/contract-reply",
                "revision": 1, "role": "assistant", "entry_type": "content", "final": true,
                "body": {"media_type": "text/plain", "text": "A synthetic response."},
                "driver": "codex", "incarnation_id": incarnation, "sequence": 1
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    state.event_notify.send(state.store.index().unwrap()).unwrap();
    let delta = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(delta["kind"], "conversation");
    assert_eq!(delta["replace"], false);
    assert_conforms(&frame_validator, "conversation delta frame", &delta);
    assert!(delta["items"].as_array().unwrap().iter().any(|entry| {
        entry["id"] == "timeline-entry/contract-reply"
            && entry["body"]["text"] == "A synthetic response."
    }));
    stream.unsubscribe("talk").await.unwrap();

    // Only the PTY byte source is synthetic; the daemon's real emulator and serializers
    // generate the TerminalScreen, envelope and multiplexed screen frame.
    std::fs::create_dir_all(&state.pty_root).unwrap();
    let pty_listener =
        tokio::net::UnixListener::bind(state.pty_root.join(format!("{runtime}.sock"))).unwrap();
    let pty = tokio::spawn(async move {
        let (mut connection, _) = pty_listener.accept().await.unwrap();
        let mut bytes = [0_u8; 128];
        let mut reader = pty_core::protocol::PacketReader::new();
        loop {
            let count = connection.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0, "terminal viewer disconnected before PEEK");
            if reader.feed(&bytes[..count]).unwrap().iter().any(|packet| {
                packet.type_ == pty_core::protocol::MessageType::Peek
            }) {
                break;
            }
        }
        connection
            .write_all(&pty_core::protocol::encode_screen(
                b"\x1b[1;31mSynthetic terminal\x1b[0m\r\nSecond line",
            ))
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let terminal = format!("terminal/{agent}");
    let (status, screen) = client_json(
        app.clone(),
        &format!("/v1/client/terminals/{}/screen", urlencoding::encode(&terminal)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{screen}");
    assert_conforms(&envelope_validator, "terminal screen envelope", &screen);
    assert_eq!(screen["value"]["lines"][0]["text"], "Synthetic terminal");
    assert_eq!(screen["value"]["lines"][0]["runs"][0]["bold"], true);
    let (status, capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    assert_eq!(status, StatusCode::OK, "{capabilities}");
    let action = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/contract-attach", "type": "terminal.attach",
        "idempotency_key": "contract-terminal-attach-0001",
        "fence": {
            "snapshot_id": capabilities["snapshot"]["id"], "subject_revisions": {},
            "runtime_incarnation": incarnation, "terminal_sequence": capabilities["snapshot"]["store_index"]
        },
        "parameters": {"target_id": terminal}
    });
    let (status, attached) = client_post_json(app, "/v1/client/actions", action).await;
    assert_eq!(status, StatusCode::OK, "{attached}");
    let capability = attached["value"]["terminal_attachment"]["stream_capability"]
        .as_str()
        .unwrap();
    stream
        .subscribe_terminal("term", &terminal, Some(incarnation), capability)
        .await
        .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame["kind"], "screen");
    assert_conforms(&frame_validator, "multiplexed terminal screen", &frame);
    assert_eq!(frame["value"]["lines"], screen["value"]["lines"]);
    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: serde_json::from_value(serde_json::json!({
                "runtime_id": runtime, "incarnation_id": "contract-terminal:i2",
                "status": "running", "terminal": true, "reachability": "local"
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    state
        .event_notify
        .send_replace(state.store.index().unwrap());
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ended["kind"], "error", "{ended}");
    assert_eq!(ended["collection"], "terminal");
    assert_eq!(ended["retryable"], false);
    assert_conforms(&frame_validator, "terminal stale-fence error", &ended);
    let mut extra = ended.clone();
    extra["undeclared"] = serde_json::json!(true);
    assert!(!frame_validator.is_valid(&extra));
    stream.close().await;
    pty.abort();
    server.abort();
}

#[test]
fn manifest_names_existing_json_fixtures_and_schema_definitions() {
    let root = asset_root();
    let manifest = json(root.join("fixtures/manifest.json"));
    let schema = json(root.join("schemas/client-v0.schema.json"));
    let definitions = schema["$defs"].as_object().expect("schema definitions");
    let mut files = HashSet::new();

    for entry in manifest["fixtures"].as_array().expect("fixture manifest") {
        let file = entry["file"].as_str().expect("fixture file");
        let definition = entry["definition"].as_str().expect("fixture definition");
        assert!(files.insert(file), "duplicate fixture {file}");
        assert!(
            definitions.contains_key(definition),
            "unknown definition {definition}"
        );
        let value = json(root.join("fixtures").join(file));
        assert!(!value.is_null(), "empty fixture {file}");
    }

    for entry in std::fs::read_dir(root.join("fixtures")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name != "manifest.json" {
            assert!(files.contains(name.as_str()), "unlisted fixture {name}");
        }
    }

    fn assert_refs_resolve(value: &Value, definitions: &serde_json::Map<String, Value>) {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                    && let Some(name) = reference.strip_prefix("#/$defs/")
                {
                    assert!(
                        definitions.contains_key(name),
                        "unresolved schema reference {reference}"
                    );
                }
                for child in object.values() {
                    assert_refs_resolve(child, definitions);
                }
            }
            Value::Array(array) => {
                for child in array {
                    assert_refs_resolve(child, definitions);
                }
            }
            _ => {}
        }
    }
    assert_refs_resolve(&schema, definitions);
}

#[test]
fn operation_manifest_is_launch_only_and_covers_v0_resources_and_actions() {
    let operations = json(asset_root().join("schemas/operations.json"));
    let encoded = serde_json::to_string(&operations).unwrap();
    assert!(
        !encoded.contains("planning"),
        "planning leaked into the client surface"
    );
    assert_eq!(operations["transport"]["public_listener"], false);
    assert_eq!(operations["transport"]["remote_mode"], "online-thin-client");

    let read_ids = operations["reads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|read| read["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    for required in [
        "capabilities.get",
        "now.list",
        "machines.list",
        "devices.list",
        "attention.list",
        "messages.list",
        "launches.list",
        "launch-variants.list",
        "launch-decisions.list",
        "launch-approvals.list",
        "missions.list",
        "work.list",
        "agents.list",
        "agent-queue.get",
        "runtimes.list",
        "observers.list",
        "subscriptions.list",
        "lanes.list",
        "lanes.get",
        "operations.list",
        "history.list",
        "sessions.list",
        "timeline.list",
        "events.list",
        "terminal.screen",
    ] {
        assert!(
            read_ids.contains(required),
            "missing read contract {required}"
        );
    }

    let actions = operations["actions"].as_object().unwrap();
    for required in [
        "attention.resolve",
        "review.approve",
        "review.reject",
        "message.send",
        "message.read",
        "message.close",
        "launch.create",
        "launch.revise",
        "launch.preview",
        "launch.approve",
        "launch.cancel",
        "mission.start",
        "mission.revise",
        "mission.approve-revision",
        "mission.cancel-revision",
        "mission.cancel",
        "work.claim",
        "work.renew",
        "work.progress",
        "work.complete",
        "work.fail",
        "work.release",
        "work.retry",
        "work.publish-mission",
        "agent.stop",
        "agent.start",
        "agent.queue-move",
        "lane.join",
        "lane.leave",
        "lane.move",
        "lane.mark",
        "lane.approve",
        "runtime.stop",
        "runtime.restart",
        "runtime.reset",
        "runtime.context-clear",
        "runtime.signal",
        "terminal.input",
        "terminal.resize",
        "terminal.attach",
        "terminal.detach",
        "pairing.revoke",
    ] {
        assert!(
            actions.contains_key(required),
            "missing action contract {required}"
        );
    }
    let schema = json(asset_root().join("schemas/client-v0.schema.json"));
    let schema_actions = schema["$defs"]["ActionCommon"]["properties"]["type"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action.as_str().unwrap())
        .collect::<BTreeSet<_>>();
    let manifest_actions = actions.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(schema_actions, manifest_actions);
}

#[test]
fn resource_fixture_covers_every_resource_kind_with_stable_unique_ids() {
    let resources = fixture("resources.json");
    let mut ids = HashSet::new();
    let kinds = resources
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| {
            let id = resource["id"].as_str().unwrap();
            assert!(id.contains('/'), "ID has no type prefix: {id}");
            assert!(ids.insert(id), "duplicate stable ID {id}");
            resource["kind"].as_str().unwrap()
        })
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        "agent",
        "attention",
        "history",
        "launch",
        "launch-approval",
        "launch-decision",
        "launch-variant",
        "lane",
        "machine",
        "message",
        "mission",
        "operation",
        "observer",
        "runtime",
        "subscription",
        "device",
        "session",
        "work",
    ]);
    assert_eq!(kinds, expected);

    let attention = resources
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["kind"] == "attention")
        .unwrap();
    assert_eq!(attention["person_id"], "person/alex");
    assert_eq!(attention["source_id"], "launch/release");
    assert_eq!(attention["attention_kind"], "launch-approval");
    assert_eq!(
        attention["actions"],
        serde_json::json!(["launch.approve", "launch.cancel"])
    );
}

#[test]
fn launch_preview_token_is_deterministic() {
    let resources = fixture("resources.json");
    let variant = resources
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "launch-variant")
        .unwrap();
    // RFC 8785 canonical JSON for the normative token input in the fixture.
    let canonical = r#"{"api_version":"st3.client.v0","candidate_revision":2,"diagnostics":[],"launch_id":"launch/release","normalized_mission":{"goals":["Ship release"],"id":"mission/release","steps":[{"id":"build","needs":[]},{"id":"deploy","needs":["build"]}]},"target_generation":null,"variant_id":"launch-variant/release/default"}"#;
    let token = format!("lpv0:{:x}", Sha256::digest(canonical.as_bytes()));
    assert_eq!(variant["preview_token"], token);
}

#[test]
fn action_fixture_is_idempotent_fenced_and_cannot_select_identity() {
    let action = fixture("action.json");
    assert_eq!(action["api_version"], "st3.client.v0");
    assert!(action["idempotency_key"].as_str().unwrap().len() >= 16);
    assert!(action["fence"]["snapshot_id"].is_string());
    assert!(action["fence"]["mission_generation"].is_string());
    assert!(action["fence"]["runtime_incarnation"].is_string());
    let encoded = serde_json::to_string(&action).unwrap();
    for forbidden in [
        "\"actor\"",
        "\"credential\"",
        "fleet_secret",
        "shared_secret",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "forbidden action identity field {forbidden}"
        );
    }
}

#[test]
fn timeline_is_ordered_typed_and_links_tool_results_to_prior_calls() {
    let timeline = fixture("timeline.json");
    let entries = timeline["value"]["items"].as_array().unwrap();
    let mut previous = None;
    let mut calls = HashSet::new();
    let mut types = BTreeSet::new();
    for entry in entries {
        let sequence = entry["sequence"].as_u64().unwrap();
        if let Some(previous) = previous {
            assert!(sequence > previous, "timeline is not strictly ordered");
        }
        previous = Some(sequence);
        let kind = entry["type"].as_str().unwrap();
        types.insert(kind);
        if kind == "tool_call" {
            calls.insert(entry["body"]["call_id"].as_str().unwrap());
        } else if kind == "tool_result" {
            assert!(calls.contains(entry["body"]["call_id"].as_str().unwrap()));
        }
        assert!(matches!(
            entry["role"].as_str().unwrap(),
            "system" | "user" | "assistant" | "tool"
        ));
    }
    assert_eq!(
        types,
        BTreeSet::from([
            "content",
            "error",
            "message",
            "redaction",
            "status",
            "tool_call",
            "tool_result",
            "truncation",
            "usage"
        ])
    );
}

#[test]
fn event_feeds_are_contiguous_and_terminal_streams_replace_whole_screens() {
    let events = fixture("events.json");
    let items = events["value"]["items"].as_array().unwrap();
    for pair in items.windows(2) {
        assert_eq!(pair[0]["next_cursor"], pair[1]["previous_cursor"]);
        assert_eq!(
            pair[0]["sequence"].as_u64().unwrap() + 1,
            pair[1]["sequence"].as_u64().unwrap()
        );
    }
    assert_eq!(
        items.last().unwrap()["next_cursor"],
        events["value"]["resume_cursor"]
    );

    // A terminal stream is a sequence of whole screens for one incarnation. Each replaces the
    // one before it, so consecutive screens differ in revision and every run spells its line.
    let screens = [
        fixture("terminal-screen.json"),
        fixture("terminal-screen-changed.json"),
    ];
    assert_eq!(
        screens[0]["value"]["runtime_incarnation"],
        screens[1]["value"]["runtime_incarnation"]
    );
    assert_ne!(
        screens[0]["value"]["revision"],
        screens[1]["value"]["revision"]
    );
    for screen in &screens {
        for line in screen["value"]["lines"].as_array().unwrap() {
            let spelled = line["runs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|run| run["text"].as_str().unwrap())
                .collect::<String>();
            assert_eq!(
                spelled.trim_end_matches(' '),
                line["text"].as_str().unwrap()
            );
        }
    }
    assert_eq!(
        fixture("terminal-stale-fence-error.json")["code"],
        "stale-fence"
    );
}

fn test_state(root: &Path) -> AppState {
    AppState {
        // Server reads and writes overlap; use the daemon's file-backed WAL semantics.
        store: Arc::new(
            Store::open(&root.join("claims.sqlite3"), "client-v0-baseline").unwrap(),
        ),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "client-v0-baseline".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

#[tokio::test]
async fn collection_socket_multiplexes_snapshot_then_changes_and_resubscribes() {
    let validator = contract_validator("CollectionFrame");
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let state = test_state(root.path());
    let server_state = state.clone();
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, st3::api::router(server_state))
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let client = st3_client::Client::unix(&socket);
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe("missions", "missions", 20, None, None)
        .await
        .unwrap();
    stream
        .subscribe("agents", "agents", 20, None, None)
        .await
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first["kind"], "snapshot");
    assert_eq!(second["kind"], "snapshot");
    assert_eq!(first["id"], "missions");
    assert_eq!(second["id"], "agents");
    assert_conforms(&validator, "missions snapshot", &first);
    assert_conforms(&validator, "agents snapshot", &second);

    let source =
        "version 2\nmission \"socket-test\" state=\"ready\" { goal \"Test collection changes\" }\n";
    let intent = st3::graph::parse_intent(source, "client-v0-baseline").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "socket-test-definition")
        .unwrap();
    state
        .event_notify
        .send(state.store.index().unwrap())
        .unwrap();
    let change = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(change["kind"], "changes");
    assert_eq!(change["id"], "missions");
    assert_conforms(&validator, "mission change", &change);
    assert!(
        change["upserts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "mission/socket-test")
    );

    stream.close().await;
    let mut replacement = client.collection_stream().await.unwrap();
    replacement
        .subscribe("missions", "missions", 20, None, None)
        .await
        .unwrap();
    let fresh = tokio::time::timeout(std::time::Duration::from_secs(5), replacement.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fresh["kind"], "snapshot");
    assert_conforms(&validator, "resubscribed mission window", &fresh);
    assert!(
        fresh["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "mission/socket-test")
    );
    server.abort();
}

#[tokio::test]
async fn attention_socket_evicts_completed_sources_and_reconnects_without_cached_cards() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let state = test_state(root.path());
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "socket-person-asker")
        .unwrap();
    let ask = state
        .store
        .ask_person(&st3::model::PersonAskRequest {
            legacy_request: None,
            person: "person/avery".into(),
            title: "Choose date".into(),
            reason: "Release needs a date".into(),
            actor: format!("agent/{}.asker", state.store.origin()),
            step: None,
            new_run: Some("choose-date".into()),
            incarnation: None,
            idempotency_key: "socket-ask".into(),
            request: None,
        })
        .unwrap();
    let server_state = state.clone();
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, st3::api::router(server_state))
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let client = st3_client::Client::unix(&socket);
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe("attention", "attention", 20, None, None)
        .await
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let card = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["source_id"] == ask.subject)
        .unwrap();
    let card_id = card["id"].clone();
    let revision = card["revision"].clone();
    let app = st3::api::router(state.clone());
    let action = serde_json::json!({"api_version":"st3.client.v0", "id":"action/socket-done", "type":"work.done",
        "idempotency_key":"socket-done-00000001", "fence":{"snapshot_id":first["snapshot"]["id"], "subject_revisions":{(card_id.as_str().unwrap()):revision}},
        "parameters":{"target_id":ask.subject, "episode":card["episode"], "summary":"Friday", "evidence":[]}});
    let (status, response) = client_post_json_person(
        app.clone(),
        "/v1/client/actions",
        "person/avery",
        action.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let change = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(change["kind"], "changes");
    assert!(
        change["removes"].as_array().unwrap().contains(&card_id),
        "{change}"
    );
    let mut stale = action;
    stale["id"] = "action/stale-done".into();
    stale["idempotency_key"] = "stale-done-00000001".into();
    let (status, response) =
        client_post_json_person(app, "/v1/client/actions", "person/avery", stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert_eq!(response["code"], "stale-fence");
    stream.close().await;
    let mut replacement = client.collection_stream().await.unwrap();
    replacement
        .subscribe("attention", "attention", 20, None, None)
        .await
        .unwrap();
    let fresh = tokio::time::timeout(std::time::Duration::from_secs(5), replacement.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fresh["kind"], "snapshot");
    assert!(fresh["items"].as_array().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn missions_first_page_stays_under_100ms_with_thousands_of_definitions() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("large.sqlite");
    let store = Arc::new(Store::open(&db, "client-v0-baseline").unwrap());
    let source = "version 2\nmission \"base\" state=\"ready\" { goal \"Page quickly\" }\n";
    let intent = st3::graph::parse_intent(source, "client-v0-baseline").unwrap();
    let planned = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "large-page-base")
        .unwrap();
    let base = store.mission_definitions().unwrap().remove(0).mission;
    let claim_id: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT claim_id FROM mission_definitions WHERE mission_id='base'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut connection = rusqlite::Connection::open(&db).unwrap();
    st3::store::configure_projection_writer(&connection).unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..3000 {
        let id = format!("large-{index:04}");
        let mut mission = base.clone();
        mission.id = id.clone();
        mission.subject = format!("mission/{id}");
        transaction.execute(
            "INSERT INTO mission_revisions(mission_id,revision,state,body,claim_id,created_index) VALUES(?1,?2,'ready',?3,?4,1)",
            rusqlite::params![id, mission.revision, serde_json::to_string(&mission).unwrap(), claim_id],
        ).unwrap();
        transaction.execute(
            "INSERT INTO mission_definitions(mission_id,revision,state,claim_id) VALUES(?1,?2,'ready',?3)",
            rusqlite::params![id, mission.revision, claim_id],
        ).unwrap();
    }
    transaction.commit().unwrap();
    let mut state = test_state(root.path());
    state.store = store;
    let app = st3::api::router(state);
    let started = std::time::Instant::now();
    let (status, page) = client_json(app, "/v1/client/missions?limit=50").await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["value"]["items"].as_array().unwrap().len(), 50);
    assert!(
        elapsed < std::time::Duration::from_millis(100),
        "mission page over 3000 rows took {elapsed:?}"
    );
}

#[tokio::test]
async fn client_v0_read_routes_conform_to_the_manifest() {
    let root = tempfile::tempdir().unwrap();
    let app = st3::api::router(test_state(root.path()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/client/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(envelope["api_version"], "st3.client.v0");
    assert_eq!(
        envelope["snapshot"]["projection_version"],
        "client-projection.v0"
    );
    assert_eq!(envelope["value"]["kind"], "capabilities");
    assert_eq!(envelope["value"]["transport"], "unix");
    assert_eq!(envelope["value"]["limits"]["max_page_items"], 200);
}

#[tokio::test]
async fn daemon_build_identity_is_shared_by_doctor_and_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let app = st3::api::router(test_state(root.path()));
    let (status, envelope) = client_json(app.clone(), "/v1/client/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    let version = st_drivers::version::machine_version();
    assert_eq!(envelope["value"]["machine_version"], version);
    contract_validator("Capabilities")
        .validate(&envelope["value"])
        .unwrap();
    let capabilities: st3_client::Capabilities =
        serde_json::from_value(envelope["value"].clone()).unwrap();
    assert_eq!(
        capabilities.machine_version.as_deref(),
        Some(version.as_str())
    );
    let (status, doctor_envelope) = client_json(app, "/v1/doctor").await;
    assert_eq!(status, StatusCode::OK);
    let mut doctor = doctor_envelope["value"].clone();
    assert_eq!(doctor["machine_version"], version);

    // New clients can still read old servers that have no version field.
    let mut old_capabilities = envelope["value"].clone();
    old_capabilities
        .as_object_mut()
        .unwrap()
        .remove("machine_version");
    contract_validator("Capabilities")
        .validate(&old_capabilities)
        .unwrap();
    assert!(
        serde_json::from_value::<st3_client::Capabilities>(old_capabilities)
            .unwrap()
            .machine_version
            .is_none()
    );
    doctor.as_object_mut().unwrap().remove("machine_version");
    assert!(
        serde_json::from_value::<st3::model::DoctorReport>(doctor)
            .unwrap()
            .machine_version
            .is_none()
    );
}

async fn client_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

async fn client_post_json(app: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("x-st3-person", "person/alex")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

async fn client_json_auth(app: axum::Router, uri: &str, credential: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("authorization", format!("Bearer {credential}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

async fn client_json_person(app: axum::Router, uri: &str, person: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("x-st3-person", person)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

async fn client_post_json_auth(
    app: axum::Router,
    uri: &str,
    credential: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {credential}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

async fn client_post_json_person(
    app: axum::Router,
    uri: &str,
    person: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("x-st3-person", person)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

#[tokio::test]
async fn completed_read_surface_is_versioned_and_cursor_gaps_require_resync() {
    let root = tempfile::tempdir().unwrap();
    let app = st3::api::router(test_state(root.path()));
    for collection in ["now", "machines", "missions", "runtimes", "operations"] {
        let (status, envelope) =
            client_json(app.clone(), &format!("/v1/client/{collection}")).await;
        assert_eq!(status, StatusCode::OK, "{collection}: {envelope}");
        assert_eq!(envelope["value"]["collection"], collection);
    }
    let (status, error) =
        client_json(app, "/v1/client/events?after=event-cursor/another-host/9").await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(error["code"], "cursor-gap");
    assert_eq!(error["details"]["full_resync"], true);
}

#[tokio::test]
async fn default_now_is_human_attention_and_explicit_run_filter_can_show_work() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let source = r#"version 2
mission "now-work" state="ready" {
  goal "Keep mission work in Control by default."
  step "implement" { assigned-to "agent/worker" }
}
"#;
    let intent = st3::graph::parse_intent(source, "client-v0-baseline").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "now-work-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&st3::model::MissionRunRequest {
            mission: "now-work".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: std::collections::BTreeMap::new(),
            idempotency_key: "now-work-run".into(),
        })
        .unwrap();
    state
        .store
        .set_step_state(&run.steps[0].subject, "ready", None)
        .unwrap();
    let app = st3::api::router(state);
    let (_, default) = client_json_person(app.clone(), "/v1/client/now", "person/alex").await;
    assert!(
        default["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] != "work")
    );
    let (_, filtered) = client_json_person(
        app,
        &format!("/v1/client/now?owner_run={}", run.subject),
        "person/alex",
    )
    .await;
    assert!(
        filtered["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == run.steps[0].subject)
    );
}

#[tokio::test]
async fn unchanged_store_index_names_one_stable_snapshot_and_payload() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let app = st3::api::router(state.clone());

    let (_, empty_first) = client_json(app.clone(), "/v1/client/capabilities").await;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let (_, empty_second) = client_json(app.clone(), "/v1/client/capabilities").await;
    assert_eq!(empty_first["snapshot"], empty_second["snapshot"]);
    assert_eq!(empty_first["value"], empty_second["value"]);
    assert_eq!(empty_first["snapshot"]["store_index"], 0);
    assert_eq!(
        empty_first["snapshot"]["created_at"],
        "1970-01-01T00:00:00.000Z"
    );

    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: "agent/stable-snapshot".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/stable-snapshot".into()),
            fields: std::collections::BTreeMap::from([
                ("runtime_id".into(), Value::String("stable-runtime".into())),
                (
                    "incarnation_id".into(),
                    Value::String("stable-runtime:i1".into()),
                ),
                ("status".into(), Value::String("running".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    for path in ["/v1/client/runtimes", "/v1/client/operations"] {
        let (_, first) = client_json(app.clone(), path).await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let (_, second) = client_json(app.clone(), path).await;
        assert_eq!(first["snapshot"], second["snapshot"], "{path}");
        assert_eq!(first["value"], second["value"], "{path}");
    }

    let (_, before) = client_json(app.clone(), "/v1/client/agents?limit=1").await;
    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: "agent/snapshot-changed".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/snapshot-changed".into()),
            fields: std::collections::BTreeMap::from([
                ("runtime_id".into(), Value::String("changed-runtime".into())),
                ("status".into(), Value::String("running".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let (_, after) = client_json(app, "/v1/client/capabilities").await;
    assert_ne!(before["snapshot"]["id"], after["snapshot"]["id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pairing_completion_mints_exactly_one_credential() {
    const CONTENDERS: usize = 16;

    let root = tempfile::tempdir().unwrap();
    let mut state = test_state(root.path());
    // Exercise the file-backed WAL store used by daemons. The shared-cache in-memory fixture
    // has SQLite table-lock semantics that are deliberately different from production.
    state.store =
        Arc::new(Store::open(&root.path().join("pairing.sqlite"), "client-v0-baseline").unwrap());
    let local = st3::api::router(state.clone());
    let fabric = st3::api::fabric_router(state.clone());
    let (status, challenge) = client_post_json(
        local,
        "/v1/client/pairings",
        serde_json::json!({
            "api_version": "st3.client.v0",
            "device_name": "Concurrent phone",
            "person_id": "person/alex"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    let pairing = challenge["value"]["pairing_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("pairing/")
        .to_owned();
    let uri = format!("/v1/client/pairings/{pairing}/complete");
    let code = challenge["value"]["code"].as_str().unwrap().to_owned();
    // Stretch the work between the read and append enough for every runtime worker to contend on
    // the same subject head. The CAS, rather than scheduling, determines the sole winner.
    let public_key = format!("concurrent-public-key-{}", "x".repeat(1024 * 1024));
    let barrier = Arc::new(Barrier::new(CONTENDERS + 1));
    let mut requests = Vec::new();
    for _ in 0..CONTENDERS {
        let app = fabric.clone();
        let barrier = barrier.clone();
        let uri = uri.clone();
        let body = serde_json::json!({
            "api_version": "st3.client.v0",
            "code": code,
            "device_public_key": public_key
        });
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            client_post_json(app, &uri, body).await
        }));
    }
    barrier.wait().await;

    let mut successes = Vec::new();
    for request in requests {
        let (status, envelope) = request.await.unwrap();
        match status {
            StatusCode::OK => successes.push(
                envelope["value"]["credential"]
                    .as_str()
                    .expect("successful completion credential")
                    .to_owned(),
            ),
            StatusCode::FORBIDDEN => assert_eq!(envelope["code"], "forbidden"),
            _ => panic!("unexpected concurrent pairing result {status}: {envelope}"),
        }
    }
    assert_eq!(
        successes.len(),
        1,
        "exactly one request may mint a credential"
    );

    let claims = state
        .store
        .claims_for(&format!("custom/client/pairing-{pairing}"), None)
        .unwrap();
    let begun = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.pairing-begun")
        .unwrap();
    let completed = claims
        .iter()
        .filter(|claim| claim.kind == "custom.client.pairing-completed")
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].predecessors.as_slice(),
        std::slice::from_ref(&begun.id)
    );
    assert_eq!(completed[0].body["evidence"], serde_json::json!([begun.id]));
}

#[tokio::test]
async fn pairing_is_single_use_and_fenced_actions_are_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let app = st3::api::router(state.clone());
    let fabric = st3::api::fabric_router(state.clone());
    let (_, capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    let snapshot = capabilities["snapshot"]["id"].as_str().unwrap();
    let action = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/message-test", "type": "message.send",
        "idempotency_key": "message-send-test-000001",
        "fence": { "snapshot_id": snapshot, "subject_revisions": {} },
        "parameters": { "to": "person/test", "content": "hello" }
    });
    let (status, first) = client_post_json(app.clone(), "/v1/client/actions", action.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["value"]["status"], "completed");
    let (status, repeated) = client_post_json(app.clone(), "/v1/client/actions", action).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(
        first["value"]["operation_id"],
        repeated["value"]["operation_id"]
    );

    let (status, challenge) = client_post_json(
        app.clone(),
        "/v1/client/pairings",
        serde_json::json!({
            "api_version": "st3.client.v0", "device_name": "Test phone", "person_id": "person/alex"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    let pairing = challenge["value"]["pairing_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("pairing/");
    let code = challenge["value"]["code"].as_str().unwrap();
    let complete = serde_json::json!({ "api_version": "st3.client.v0", "code": code, "device_public_key": "test-public-key-0000000000000000000000000000" });
    let (status, paired) = client_post_json(
        app.clone(),
        &format!("/v1/client/pairings/{pairing}/complete"),
        complete.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let credential = paired["value"]["credential"].as_str().unwrap();
    let device = paired["value"]["device_id"].as_str().unwrap();
    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: "custom/client/pairing-expired-inventory".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: Some("person/alex".into()),
            fields: std::collections::BTreeMap::from([
                (
                    "credential_hash".into(),
                    Value::String(hex::encode(Sha256::digest(b"expired inventory credential"))),
                ),
                (
                    "device_id".into(),
                    Value::String("device/expired-inventory".into()),
                ),
                ("person_id".into(), Value::String("person/alex".into())),
                (
                    "session_actor".into(),
                    Value::String("person/alex/session/expired-inventory".into()),
                ),
                ("scopes".into(), serde_json::json!(["read.projections"])),
                ("expires_at_unix_ms".into(), serde_json::json!(1)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let (status, devices) =
        client_json_person(app.clone(), "/v1/client/devices", "person/alex").await;
    assert_eq!(status, StatusCode::OK, "{devices}");
    let encoded_devices = serde_json::to_string(&devices).unwrap();
    assert_eq!(devices["value"]["items"][0]["id"], device);
    assert_eq!(devices["value"]["items"][0]["state"], "active");
    assert!(!encoded_devices.contains("credential_hash"));
    assert!(!encoded_devices.contains("device_public_key"));
    let (status, device_history) = client_json_person(
        app.clone(),
        "/v1/client/devices?history=true",
        "person/alex",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{device_history}");
    assert_eq!(
        device_history["value"]["items"].as_array().unwrap().len(),
        2
    );
    assert!(
        device_history["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["state"] == "expired")
    );
    assert!(credential.len() >= 32);
    let (status, remote_capabilities) =
        client_json_auth(fabric.clone(), "/v1/client/capabilities", credential).await;
    assert_eq!(status, StatusCode::OK, "{remote_capabilities}");
    assert_eq!(remote_capabilities["value"]["transport"], "fabric-loopback");
    let (status, reused) = client_post_json(
        app.clone(),
        &format!("/v1/client/pairings/{pairing}/complete"),
        complete,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reused}");

    let (_, local_capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    let revoke = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/revoke-test", "type": "pairing.revoke",
        "idempotency_key": "pairing-revoke-test-0001",
        "fence": { "snapshot_id": local_capabilities["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "target_id": device }
    });
    let (status, revoked) = client_post_json(app.clone(), "/v1/client/actions", revoke).await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    let (status, current_devices) =
        client_json_person(app.clone(), "/v1/client/devices", "person/alex").await;
    assert_eq!(status, StatusCode::OK, "{current_devices}");
    assert!(
        current_devices["value"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (status, devices) = client_json_person(
        app.clone(),
        "/v1/client/devices?history=true",
        "person/alex",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{devices}");
    let revoked_device = devices["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == device)
        .unwrap();
    assert_eq!(revoked_device["state"], "revoked");
    let encoded_devices = serde_json::to_string(&devices).unwrap();
    assert!(!encoded_devices.contains(credential));
    assert!(!encoded_devices.contains("credential_hash"));
    assert!(!encoded_devices.contains("device_public_key"));
    let (status, denied) = client_json_auth(fabric, "/v1/client/capabilities", credential).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
}

#[tokio::test]
async fn paired_credential_exercises_only_its_exact_person_delegation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        store.origin(),
    )
    .unwrap();
    store
        .apply_internal(&intent, "paired-person-asker")
        .unwrap();
    let ask = |person: &str| {
        store
            .ask_person(&st3::model::PersonAskRequest {
                legacy_request: None,
                person: person.into(),
                title: format!("Decision for {person}"),
                reason: "Reply with the release date.".into(),
                actor: format!("agent/{}.asker", store.origin()),
                step: None,
                new_run: Some(person.into()),
                incarnation: None,
                idempotency_key: format!("paired-{person}"),
                request: None,
            })
            .unwrap()
    };
    let ada_step = ask("person/ada");
    let alex_step = ask("person/alex");
    let local = st3::api::router(state.clone());
    let fabric = st3::api::fabric_router(state.clone());

    let (status, challenge) = client_post_json_person(
        local.clone(),
        "/v1/client/pairings",
        "person/ada",
        serde_json::json!({
            "api_version": "st3.client.v0", "device_name": "Ada's phone", "person_id": "person/ada"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    let pairing = challenge["value"]["pairing_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("pairing/");
    let (status, paired) = client_post_json(
        local.clone(),
        &format!("/v1/client/pairings/{pairing}/complete"),
        serde_json::json!({
            "api_version": "st3.client.v0",
            "code": challenge["value"]["code"],
            "device_public_key": "paired-authority-public-key-000000000000000000"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    assert_eq!(paired["value"]["person_id"], "person/ada");
    let credential = paired["value"]["credential"].as_str().unwrap();

    let (status, capabilities) =
        client_json_auth(fabric.clone(), "/v1/client/capabilities", credential).await;
    assert_eq!(status, StatusCode::OK, "{capabilities}");
    let capability_state = |id: &str| {
        capabilities["value"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|capability| capability["id"] == id)
            .unwrap()["state"]
            .as_str()
            .unwrap()
    };
    assert_eq!(capability_state("attention.resolve"), "unavailable");
    assert_eq!(capability_state("work.done"), "granted");
    assert_eq!(capability_state("review.approve"), "granted");
    assert_eq!(capability_state("mission.cancel-revision"), "ungranted");
    assert_eq!(capability_state("launch.create"), "granted");
    assert_eq!(capability_state("message.send"), "ungranted");

    let (status, attention) =
        client_json_auth(fabric.clone(), "/v1/client/attention", credential).await;
    assert_eq!(status, StatusCode::OK, "{attention}");
    let item = |id: &str| {
        attention["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["source_id"] == id)
            .unwrap()
    };
    let ada = item(&ada_step.subject);
    assert_eq!(ada["attention_kind"], "person-step");
    assert_eq!(ada["source_id"], ada_step.subject);
    assert_eq!(ada["person_id"], "person/ada");
    assert_eq!(ada["actions"], serde_json::json!(["work.done"]));
    let resolve = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-attention-ada",
        "type": "work.done", "idempotency_key": "paired-attention-ada-0001",
        "fence": {
            "snapshot_id": attention["snapshot"]["id"],
            "subject_revisions": { (ada["id"].as_str().unwrap()): ada["revision"] }
        },
        "parameters": { "target_id": ada_step.subject, "summary": "Friday", "episode": ada["episode"] }
    });
    let (status, resolved) =
        client_post_json_auth(fabric.clone(), "/v1/client/actions", credential, resolve).await;
    assert_eq!(status, StatusCode::OK, "{resolved}");

    let (_, attention) = client_json_auth(fabric.clone(), "/v1/client/attention", credential).await;
    assert!(
        attention["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["source_id"] != alex_step.subject)
    );
    let (status, cross_person_read) = client_json_auth(
        fabric.clone(),
        "/v1/client/attention?person=person%2Falex",
        credential,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{cross_person_read}");
    let alex_revision = store
        .claims_for(&alex_step.subject, None)
        .unwrap()
        .last()
        .unwrap()
        .id
        .clone();
    let cross_attention = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-attention-alex",
        "type": "work.done", "idempotency_key": "paired-attention-alex-000001",
        "fence": {
            "snapshot_id": attention["snapshot"]["id"],
            "subject_revisions": { (alex_step.subject.clone()): alex_revision }
        },
        "parameters": { "target_id": alex_step.subject, "summary": "Friday", "episode": alex_revision }
    });
    let (status, denied) = client_post_json_auth(
        fabric.clone(),
        "/v1/client/actions",
        credential,
        cross_attention,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(
        store.step_run(&alex_step.subject).unwrap().unwrap().status,
        "ready"
    );

    let (_, current) =
        client_json_auth(fabric.clone(), "/v1/client/capabilities", credential).await;
    let ungranted = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-message-denied",
        "type": "message.send", "idempotency_key": "paired-message-denied-0001",
        "fence": { "snapshot_id": current["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "to": "person/ada", "content": "not delegated" }
    });
    let (status, _) =
        client_post_json_auth(fabric.clone(), "/v1/client/actions", credential, ungranted).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let spoofed = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-spoof-denied",
        "type": "attention.resolve", "idempotency_key": "paired-spoof-denied-0001",
        "fence": { "snapshot_id": current["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "attention_id": "attention/alex-paired", "outcome": "resolved", "actor": "person/alex" }
    });
    let (status, _) =
        client_post_json_auth(fabric.clone(), "/v1/client/actions", credential, spoofed).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, current) =
        client_json_auth(fabric.clone(), "/v1/client/capabilities", credential).await;
    let create = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-launch-create",
        "type": "launch.create", "idempotency_key": "paired-launch-create-0001",
        "fence": { "snapshot_id": current["snapshot"]["id"], "subject_revisions": {} },
        "parameters": {
            "title": "Ada launch", "request": "Draft a paired mission.",
            "target": { "type": "new-mission", "mission_id": "mission/paired-ada", "workspace": workspace }
        }
    });
    let (status, created) =
        client_post_json_auth(fabric.clone(), "/v1/client/actions", credential, create).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let launch_id = created["value"]["affected_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        store
            .planning_session(launch_id.trim_start_matches("launch/"))
            .unwrap()
            .unwrap()
            .requester,
        "person/ada"
    );

    let (status, alex_launch) = client_post_json_person(
        local.clone(),
        "/v1/launches",
        "person/alex",
        serde_json::to_value(st3::model::PlanningSessionStartRequest {
            mission: "paired-alex".into(),
            run: None,
            request: b"Draft Alex's mission.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("person/alex".into()),
            provider: None,
            model: None,
            effort: None,
            idempotency_key: "paired-alex-launch-0001".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alex_launch}");
    let alex_session_id = alex_launch["value"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("unexpected launch response: {alex_launch}"));
    let alex_id = format!("launch/{alex_session_id}");
    let (_, alex_detail) = client_json_auth(
        fabric.clone(),
        &format!("/v1/client/launches/{}", alex_session_id),
        credential,
    )
    .await;
    let cross_launch = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-launch-alex-cancel",
        "type": "launch.cancel", "idempotency_key": "paired-launch-alex-cancel-01",
        "fence": {
            "snapshot_id": alex_detail["snapshot"]["id"],
            "subject_revisions": { (alex_id.clone()): alex_detail["value"]["revision"] }
        },
        "parameters": { "target_id": alex_id }
    });
    let (status, denied) = client_post_json_auth(
        fabric.clone(),
        "/v1/client/actions",
        credential,
        cross_launch,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");

    let expired_credential = "expired-paired-credential-000000000000000000";
    store
        .append_claim(&st3::model::ClaimInput {
            subject: "custom/client/pairing-expired-proof".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: Some("person/ada".into()),
            fields: std::collections::BTreeMap::from([
                (
                    "credential_hash".into(),
                    Value::String(hex::encode(Sha256::digest(expired_credential.as_bytes()))),
                ),
                ("person_id".into(), Value::String("person/ada".into())),
                (
                    "session_actor".into(),
                    Value::String("person/ada/session/expired".into()),
                ),
                ("scopes".into(), serde_json::json!(["read.projections"])),
                ("expires_at_unix_ms".into(), serde_json::json!(1)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let (status, _) = client_json_auth(
        fabric.clone(),
        "/v1/client/capabilities",
        expired_credential,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, local_capabilities) = client_json(local.clone(), "/v1/client/capabilities").await;
    let revoke = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-revoke-proof",
        "type": "pairing.revoke", "idempotency_key": "paired-revoke-proof-00001",
        "fence": { "snapshot_id": local_capabilities["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "target_id": paired["value"]["device_id"] }
    });
    let (status, revoked) = client_post_json(local, "/v1/client/actions", revoke).await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    let (status, _) = client_json_auth(fabric, "/v1/client/capabilities", credential).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn full_control_pairing_requires_explicit_local_person_opt_in_and_can_be_revoked() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let local = st3::api::router(state.clone());
    let paired_gateway = st3::api::fabric_router(state);

    let begin = serde_json::json!({
        "api_version": "st3.client.v0",
        "device_name": "Full control test phone",
        "person_id": "person/alex",
        "full_control": true
    });
    let (status, denied) =
        client_post_json(paired_gateway.clone(), "/v1/client/pairings", begin.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, challenge) =
        client_post_json_person(local.clone(), "/v1/client/pairings", "person/alex", begin).await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    let pairing = challenge["value"]["pairing_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("pairing/");
    let (status, paired) = client_post_json(
        paired_gateway.clone(),
        &format!("/v1/client/pairings/{pairing}/complete"),
        serde_json::json!({
            "api_version": "st3.client.v0",
            "code": challenge["value"]["code"],
            "device_public_key": "full-control-test-phone-key-0000000000000000"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let scopes = paired["value"]["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|scope| scope == "terminal.control"));
    assert!(scopes.iter().any(|scope| scope == "control.messages"));
    assert!(scopes.iter().any(|scope| scope == "control.missions"));
    assert!(scopes.iter().any(|scope| scope == "control.work"));
    let credential = paired["value"]["credential"].as_str().unwrap();
    let (status, capabilities) = client_json_auth(
        paired_gateway.clone(),
        "/v1/client/capabilities",
        credential,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{capabilities}");
    for action in [
        "message.send",
        "mission.start",
        "work.complete",
        "terminal.input",
    ] {
        assert!(
            capabilities["value"]["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|capability| capability["id"] == action && capability["state"] == "granted"),
            "{action} was not granted"
        );
    }

    let (_, local_capabilities) =
        client_json_person(local.clone(), "/v1/client/capabilities", "person/alex").await;
    let (status, revoked) = client_post_json_person(
        local,
        "/v1/client/actions",
        "person/alex",
        serde_json::json!({
            "api_version": "st3.client.v0",
            "id": "action/revoke-full-control-test",
            "type": "pairing.revoke",
            "idempotency_key": "revoke-full-control-test-0001",
            "fence": {
                "snapshot_id": local_capabilities["snapshot"]["id"],
                "subject_revisions": {}
            },
            "parameters": { "target_id": paired["value"]["device_id"] }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    let (status, _) = client_json_auth(paired_gateway, "/v1/client/capabilities", credential).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn core_launch_and_mission_actions_use_session_identity_and_exact_fences() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let app = st3::api::router(state);

    let (_, capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    let create = serde_json::json!({
        "api_version": "st3.client.v0",
        "id": "action/launch-create-test",
        "type": "launch.create",
        "idempotency_key": "launch-create-test-0001",
        "fence": { "snapshot_id": capabilities["snapshot"]["id"], "subject_revisions": {} },
        "parameters": {
            "title": "Client launch",
            "request": "Prepare a one-step mission without changing the workspace.",
            "target": {
                "type": "new-mission",
                "mission_id": "mission/client-action-demo",
                "workspace": workspace.display().to_string()
            }
        }
    });
    let (status, created) = client_post_json(app.clone(), "/v1/client/actions", create).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let launch_id = created["value"]["affected_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let session_id = launch_id.trim_start_matches("launch/");
    let session = store.planning_session(session_id).unwrap().unwrap();
    assert_eq!(session.requester, "person/alex");

    let (status, current) =
        client_json(app.clone(), &format!("/v1/client/launches/{session_id}")).await;
    assert_eq!(status, StatusCode::OK, "{current}");
    let stale_revision = serde_json::json!({
        "api_version": "st3.client.v0",
        "id": "action/launch-revise-stale",
        "type": "launch.revise",
        "idempotency_key": "launch-revise-stale-001",
        "fence": { "snapshot_id": current["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "launch_id": launch_id, "feedback": "Add evidence." }
    });
    let (status, stale) = client_post_json(app.clone(), "/v1/client/actions", stale_revision).await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["code"], "stale-fence");

    let candidate = br#"
version 2
mission "client-action-demo" state="ready" {
  goal "Prove client action flow."
  step "prove" { goal "Record proof." }
}
"#;
    let (status, submitted) = client_post_json(
        app.clone(),
        &format!("/v1/launches/{session_id}/variants/default/submit"),
        serde_json::to_value(st3::model::PlanningCandidateSubmitRequest {
            actor: session.planner,
            markdown: b"# Client action demo\n\nRecord proof.\n".to_vec(),
            kdl: candidate.to_vec(),
            idempotency_key: "launch-candidate-test-001".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{submitted}");

    let (_, launch) = client_json(app.clone(), &format!("/v1/client/launches/{session_id}")).await;
    let launch_revision = launch["value"]["revision"].clone();
    let preview = serde_json::json!({
        "api_version": "st3.client.v0",
        "id": "action/launch-preview-test",
        "type": "launch.preview",
        "idempotency_key": "launch-preview-test-001",
        "fence": {
            "snapshot_id": launch["snapshot"]["id"],
            "subject_revisions": { (launch_id.clone()): launch_revision }
        },
        "parameters": {
            "launch_id": launch_id,
            "variant_id": format!("launch-variant/{session_id}/default")
        }
    });
    let (status, previewed) = client_post_json(app.clone(), "/v1/client/actions", preview).await;
    assert_eq!(status, StatusCode::OK, "{previewed}");

    let (_, launch) = client_json(app.clone(), &format!("/v1/client/launches/{session_id}")).await;
    let (_, variant) = client_json(
        app.clone(),
        &format!("/v1/client/launches/{session_id}/variants/default"),
    )
    .await;
    let approve = serde_json::json!({
        "api_version": "st3.client.v0",
        "id": "action/launch-approve-test",
        "type": "launch.approve",
        "idempotency_key": "launch-approve-test-001",
        "fence": {
            "snapshot_id": variant["snapshot"]["id"],
            "subject_revisions": { (launch_id.clone()): launch["value"]["revision"] },
            "preview_token": variant["value"]["preview_token"]
        },
        "parameters": {
            "launch_id": launch_id,
            "variant_id": format!("launch-variant/{session_id}/default")
        }
    });
    let (status, approved) = client_post_json(app.clone(), "/v1/client/actions", approve).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert!(
        store
            .active_mission_runs()
            .unwrap()
            .iter()
            .all(|run| { run.mission != "mission/client-action-demo" }),
        "launch approval must publish without starting the mission"
    );

    let (_, capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    let start = serde_json::json!({
        "api_version": "st3.client.v0",
        "id": "action/mission-start-test",
        "type": "mission.start",
        "idempotency_key": "mission-start-test-0001",
        "fence": { "snapshot_id": capabilities["snapshot"]["id"], "subject_revisions": {} },
        "parameters": {
            "mission_id": "mission/client-action-demo",
            "workspace": workspace.display().to_string(),
            "inputs": {}
        }
    });
    let (status, started) = client_post_json(app.clone(), "/v1/client/actions", start).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let run_id = started["value"]["affected_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();

    let (_, mission) = client_json(app.clone(), "/v1/client/missions/client-action-demo").await;
    let generation = mission["value"]["run_generations"][&run_id]
        .as_str()
        .unwrap();
    let cancel = serde_json::json!({
        "api_version": "st3.client.v0",
        "id": "action/mission-cancel-test",
        "type": "mission.cancel",
        "idempotency_key": "mission-cancel-test-001",
        "fence": {
            "snapshot_id": mission["snapshot"]["id"],
            "subject_revisions": {},
            "mission_generation": generation
        },
        "parameters": { "target_id": run_id, "reason": "test complete" }
    });
    let (status, cancelled) = client_post_json(app.clone(), "/v1/client/actions", cancel).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    let cancelling = store.mission_run(&run_id).unwrap().unwrap();
    assert_eq!(cancelling.status, "running");
    assert_eq!(cancelling.phase, "cleanup-cancelled");
    assert!(
        cancelling
            .steps
            .iter()
            .all(|step| step.status == "cancelled")
    );
    assert!(
        store
            .set_mission_run_state(
                &run_id,
                "cancelled",
                "terminal",
                Some("runtime cleanup completed"),
            )
            .unwrap()
    );
    // A cancelled run stays in the current view for a while, with its outcome.
    let (_, current_missions) = client_json(app.clone(), "/v1/client/missions").await;
    let current = current_missions["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "mission/client-action-demo")
        .expect("a just-cancelled mission stays in the current view");
    assert_eq!(current["state"], "cancelled");
    let (_, mission_history) = client_json(app, "/v1/client/missions?history=true").await;
    let cancelled = mission_history["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "mission/client-action-demo")
        .unwrap();
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["operational"]["layer"], "history");
    assert_eq!(cancelled["operational"]["actionable"], false);
}

/// Free mode: a local agent session holds every client-v0 action a person's does, in any
/// namespace and without a declaration, still behind each action's own fences, and every
/// receipt records the agent itself as the actor.
#[tokio::test]
async fn an_agent_session_runs_every_action_as_itself_behind_its_fences() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let source = r#"version 2
agent "example/operator" { workspace "."; command "true"; mission-authority { cancel "example/jobs/*" } }
mission "example/jobs/one" state="ready" { concurrent-runs; goal "Wait for cancellation."; step "wait" { agentless } }
mission "example/other/one" state="ready" { concurrent-runs; goal "Wait outside any grant."; step "wait" { agentless } }
"#;
    let intent = st3::graph::parse_intent(source, store.origin()).unwrap();
    let preview = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "free-mode-source",
            Some("person/operator"),
        )
        .unwrap();
    let start = |mission: &str, key: &str| {
        store
            .create_mission_run(&st3::model::MissionRunRequest {
                mission: mission.into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/operator".into()),
                mode: Some("run".into()),
                inputs: Default::default(),
                idempotency_key: key.into(),
            })
            .unwrap()
    };
    let app = st3::api::router(state.clone());
    for agent in ["agent/example/operator", "agent/example/undeclared"] {
        let (_, capabilities) =
            client_json_person(app.clone(), "/v1/client/capabilities", agent).await;
        for capability in capabilities["value"]["capabilities"].as_array().unwrap() {
            let id = capability["id"].as_str().unwrap();
            if capability["state"] == "unavailable" || id.contains("glass") {
                continue;
            }
            assert_eq!(capability["state"], "granted", "{agent}: {capability}");
        }
    }
    let cancel = |run: &st3::model::MissionRunView,
                  generation: &str,
                  snapshot: &Value,
                  key: &str| {
        serde_json::json!({
            "api_version": "st3.client.v0", "id": format!("action/{key}"), "type": "mission.cancel", "idempotency_key": format!("free-mode-cancel-{key}"),
            "fence": { "snapshot_id": snapshot["snapshot"]["id"], "mission_generation": generation },
            "parameters": { "target_id": run.subject, "reason": "The work was superseded." }
        })
    };
    // The generation fence still holds.
    let stale = start("example/jobs/one", "free-mode-stale-run");
    let (_, snapshot) = client_json(app.clone(), "/v1/client/capabilities").await;
    let (status, body) = client_post_json_person(
        app.clone(),
        "/v1/client/actions",
        "agent/example/operator",
        cancel(&stale, "run-generation/stale", &snapshot, "stale"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // Any agent cancels a run in any namespace, declared or not.
    for (agent, mission, key) in [
        ("agent/example/operator", "example/other/one", "outside"),
        ("agent/example/undeclared", "example/jobs/one", "undeclared"),
    ] {
        let run = start(mission, &format!("free-mode-{key}-run"));
        let (_, snapshot) = client_json(app.clone(), "/v1/client/capabilities").await;
        let request = cancel(&run, &run.generation, &snapshot, key);
        let (status, body) =
            client_post_json_person(app.clone(), "/v1/client/actions", agent, request.clone())
                .await;
        assert_eq!(status, StatusCode::OK, "{agent}: {body}");
        assert_eq!(
            store.mission_run(&run.subject).unwrap().unwrap().phase,
            "cleanup-cancelled"
        );
        let (status, repeated) =
            client_post_json_person(app.clone(), "/v1/client/actions", agent, request).await;
        assert_eq!(status, StatusCode::OK, "{repeated}");
        assert_eq!(
            body["value"]["affected_ids"],
            repeated["value"]["affected_ids"]
        );
        assert_eq!(body["value"]["affected_ids"][0], run.subject.as_str());
    }
    // An agent starts a mission through the client, and the run names it as its requester.
    let (_, snapshot) = client_json(app.clone(), "/v1/client/capabilities").await;
    let (status, body) = client_post_json_person(app.clone(), "/v1/client/actions", "agent/example/operator", serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/free-mode-start", "type": "mission.start", "idempotency_key": "free-mode-agent-start",
        "fence": { "snapshot_id": snapshot["snapshot"]["id"] },
        "parameters": { "mission_id": "mission/example/other/one", "workspace": root.path().display().to_string(), "inputs": {} }
    })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let run = body["value"]["affected_ids"][0].as_str().unwrap();
    assert_eq!(
        store.mission_run(run).unwrap().unwrap().requester,
        "agent/example/operator"
    );
    let receipts = store
        .claims_for_kind_at("custom.client.action-result", None, true, 100)
        .unwrap()
        .claims;
    assert!(!receipts.is_empty());
    for receipt in receipts {
        let actor = receipt.actor.as_deref().unwrap();
        assert!(actor.starts_with("agent/example/"), "{actor}");
        assert_eq!(receipt.body["fields"]["authority_actor"], actor);
    }
}

#[tokio::test]
async fn launch_revise_and_cancel_are_revision_fenced() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let app = st3::api::router(state);
    let (_, capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    let create = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/launch-create-cancel",
        "type": "launch.create", "idempotency_key": "launch-create-cancel-01",
        "fence": { "snapshot_id": capabilities["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "title": "Cancel me", "request": "Draft and revise.",
            "target": { "type": "new-mission", "mission_id": "mission/client-cancel-demo", "workspace": root.path().display().to_string() } }
    });
    let (_, created) = client_post_json(app.clone(), "/v1/client/actions", create).await;
    let launch_id = created["value"]["affected_ids"][0].as_str().unwrap();
    let session_id = launch_id.trim_start_matches("launch/");
    let planner = store.planning_session(session_id).unwrap().unwrap().planner;
    let candidate = br#"
version 2
mission "client-cancel-demo" state="ready" {
  goal "Exercise revision."
  step "draft" { goal "Draft evidence." }
}
"#;
    let (status, submitted) = client_post_json(
        app.clone(),
        &format!("/v1/launches/{session_id}/variants/default/submit"),
        serde_json::to_value(st3::model::PlanningCandidateSubmitRequest {
            actor: planner,
            markdown: b"# Revision demo\n".to_vec(),
            kdl: candidate.to_vec(),
            idempotency_key: "launch-revise-candidate-01".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    let (_, launch) = client_json(app.clone(), &format!("/v1/client/launches/{session_id}")).await;
    let revise = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/launch-revise-test",
        "type": "launch.revise", "idempotency_key": "launch-revise-test-0001",
        "fence": { "snapshot_id": launch["snapshot"]["id"], "subject_revisions": { (launch_id): launch["value"]["revision"] } },
        "parameters": { "launch_id": launch_id, "feedback": "Clarify the evidence." }
    });
    let (status, revised) = client_post_json(app.clone(), "/v1/client/actions", revise).await;
    assert_eq!(status, StatusCode::OK, "{revised}");
    let (_, launch) = client_json(app.clone(), &format!("/v1/client/launches/{session_id}")).await;
    let cancel = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/launch-cancel-test",
        "type": "launch.cancel", "idempotency_key": "launch-cancel-test-0001",
        "fence": { "snapshot_id": launch["snapshot"]["id"], "subject_revisions": { (launch_id): launch["value"]["revision"] } },
        "parameters": { "target_id": launch_id, "reason": "test complete" }
    });
    let (status, cancelled) = client_post_json(app, "/v1/client/actions", cancel).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
}

#[tokio::test]
async fn operational_lists_share_one_versioned_paginated_shape() {
    let validator = contract_validator("Envelope");
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    for (subject, runtime, incarnation) in [
        ("agent/alpha", "alpha-runtime", "alpha-runtime:i1"),
        ("agent/beta", "beta-runtime", "beta-runtime:i1"),
    ] {
        store
            .append_claim(&st3::model::ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: std::collections::BTreeMap::from([
                    ("runtime_id".into(), Value::String(runtime.into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("status".into(), Value::String("running".into())),
                    ("reachability".into(), Value::String("local".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let app = st3::api::router(state);
    for collection in [
        "attention",
        "messages",
        "launches",
        "work",
        "agents",
        "machines",
        "runtimes",
        "history",
        "sessions",
    ] {
        let (status, envelope) =
            client_json(app.clone(), &format!("/v1/client/{collection}")).await;
        assert_eq!(status, StatusCode::OK, "{collection}: {envelope}");
        assert_conforms(&validator, collection, &envelope);
        assert_eq!(envelope["api_version"], "st3.client.v0");
        assert_eq!(envelope["value"]["kind"], "page");
        assert_eq!(envelope["value"]["collection"], collection);
        assert!(envelope["value"]["items"].is_array());
        assert_eq!(envelope["value"]["page"]["limit"], 50);
    }

    let (_, first) = client_json(app.clone(), "/v1/client/agents?limit=1").await;
    assert_conforms(&validator, "first agents page", &first);
    assert_eq!(first["value"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["value"]["page"]["has_more"], true);
    let cursor = first["value"]["page"]["next_cursor"].as_str().unwrap();
    let uri = format!(
        "/v1/client/agents?limit=1&cursor={}",
        urlencoding::encode(cursor)
    );
    let (_, second) = client_json(app.clone(), &uri).await;
    assert_conforms(&validator, "second agents page", &second);
    assert_eq!(first["snapshot"]["id"], second["snapshot"]["id"]);
    assert_ne!(
        first["value"]["items"][0]["id"],
        second["value"]["items"][0]["id"]
    );

    store
        .append_claim(&st3::model::ClaimInput {
            subject: "agent/gamma".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/gamma".into()),
            fields: std::collections::BTreeMap::from([
                ("runtime_id".into(), Value::String("gamma-runtime".into())),
                (
                    "incarnation_id".into(),
                    Value::String("gamma-runtime:i1".into()),
                ),
                ("status".into(), Value::String("running".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let (status, continued) = client_json(app, &uri).await;
    assert_eq!(status, StatusCode::OK, "{continued}");
    assert_conforms(&validator, "retained agents page", &continued);
    assert_eq!(first["snapshot"]["id"], continued["snapshot"]["id"]);
    assert_eq!(second["value"]["items"], continued["value"]["items"]);
}

#[tokio::test]
async fn step_states_in_every_client_projection_belong_to_the_contract() {
    let validator = contract_validator("Envelope");
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let states = [
        ("pending", "waiting"),
        ("ready", "ready"),
        ("working", "claimed"),
        ("blocked", "blocked"),
        ("verifying", "verifying"),
        ("completed", "completed"),
        ("failed", "failed"),
        ("cancelled", "cancelled"),
    ];
    let steps = states
        .iter()
        .map(|(internal, _)| format!("step {internal:?} {{ assigned-to \"agent/builder\" }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!(
        "version 2\nagent \"builder\" {{ workspace \"/tmp\"; command \"true\" }}\nmission \"state-contract\" state=\"ready\" {{ goal \"Show every step state\"; {steps} }}"
    );
    let intent = st3::graph::parse_intent(&source, "client-v0-baseline").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "state-contract-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&st3::model::MissionRunRequest {
            mission: "state-contract".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/avery".into()),
            mode: Some("run".into()),
            inputs: Default::default(),
            idempotency_key: "state-contract-run".into(),
        })
        .unwrap();
    for (internal, _) in states {
        let step = run.steps.iter().find(|step| step.step == internal).unwrap();
        if internal == "working" {
            state
                .store
                .set_step_state(&step.subject, "ready", None)
                .unwrap();
            state
                .store
                .work_action(
                    &step.subject,
                    "claim",
                    &st3::model::WorkRequest {
                        actor: step.assigned_to.clone(),
                        incarnation: Some("builder-one".into()),
                        summary: None,
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: "state-contract-claim".into(),
                    },
                )
                .unwrap();
            state
                .store
                .work_action(
                    &step.subject,
                    "progress",
                    &st3::model::WorkRequest {
                        actor: step.assigned_to.clone(),
                        incarnation: Some("builder-one".into()),
                        summary: Some("Building the release".into()),
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: "state-contract-progress".into(),
                    },
                )
                .unwrap();
            assert_eq!(
                state
                    .store
                    .mission_run_steps(&run.subject, true)
                    .unwrap()
                    .unwrap()
                    .steps
                    .into_iter()
                    .find(|item| item.subject == step.subject)
                    .unwrap()
                    .status,
                "working"
            );
        } else {
            state
                .store
                .set_step_state(&step.subject, internal, None)
                .unwrap();
        }
    }
    let app = st3::api::router(state);
    for path in ["/v1/client/missions", "/v1/client/missions/state-contract"] {
        let (status, response) = client_json(app.clone(), path).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_conforms(&validator, path, &response);
        let mission = if path.ends_with("state-contract") {
            &response["value"]
        } else {
            &response["value"]["items"][0]
        };
        let run = &mission["run_details"][0];
        for step in run["steps"].as_array().unwrap() {
            let expected = states
                .iter()
                .find(|(internal, _)| step["path"] == *internal)
                .unwrap()
                .1;
            assert_eq!(step["state"], expected);
        }
        assert!(
            run["current_steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["state"] == "claimed")
        );
    }
    let (status, response) = client_json(app.clone(), "/v1/client/work?history=true").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_conforms(&validator, "work history", &response);
    assert_eq!(
        response["value"]["items"].as_array().unwrap().len(),
        states.len()
    );
    for step in response["value"]["items"].as_array().unwrap() {
        let (status, detail) = client_json(
            app.clone(),
            &format!("/v1/client/work/{}", step["id"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_conforms(&validator, "work detail", &detail);
    }
    let (status, response) = client_json(app, "/v1/client/agents").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_conforms(&validator, "agent work queue", &response);
    let agent = &response["value"]["items"][0];
    assert_eq!(
        agent["current_work"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["path"] == "working")
            .unwrap()["state"],
        "claimed"
    );
}

#[tokio::test]
async fn client_work_projection_waits_for_person_and_resumes_after_response() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let source = r#"
version 2
agent "ios-owner" { workspace "/tmp"; command "true" }
mission "ios-proof" state="ready" {
  goal "Run the physical simulator proof."
  step "automated-proof" { assigned-to "agent/ios-owner" }
}
"#;
    let intent = st3::graph::parse_intent(source, "client-v0-baseline").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "client-blocker-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&st3::model::MissionRunRequest {
            mission: "ios-proof".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: std::collections::BTreeMap::new(),
            idempotency_key: "client-blocker-run".into(),
        })
        .unwrap();
    let subject = run.steps[0].subject.clone();
    state.store.set_step_state(&subject, "ready", None).unwrap();
    let request = |key: &str, reason: Option<&str>| st3::model::WorkRequest {
        actor: Some("agent/client-v0-baseline.ios-owner".into()),
        incarnation: Some("ios-owner-one".into()),
        summary: None,
        reason: reason.map(str::to_owned),
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    state
        .store
        .work_action(&subject, "claim", &request("client-blocker-claim", None))
        .unwrap();
    let ask = state
        .store
        .ask_person(&st3::model::PersonAskRequest {
            legacy_request: None,
            person: "person/alex".into(),
            title: "Repair simulator components".into(),
            reason: "Repair before automated proof.".into(),
            actor: "agent/client-v0-baseline.ios-owner".into(),
            step: Some(subject.clone()),
            new_run: None,
            incarnation: Some("ios-owner-one".into()),
            idempotency_key: "client-simulator-question".into(),
            request: None,
        })
        .unwrap();
    let app = st3::api::router(state.clone());
    let (status, response) = client_json(app.clone(), "/v1/client/work").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let item = response["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == subject)
        .unwrap();
    assert_eq!(item["state"], "waiting-person");
    assert_eq!(item["blocked_reason"], ask.subject);
    assert_eq!(item["blockers"], serde_json::json!([ask.subject]));
    assert_eq!(item["operational"]["actionable"], false);
    state
        .store
        .finish_person_step(
            &st3::model::PersonStepResponse {
                subject: ask.subject,
                actor: "person/alex".into(),
                summary: "Simulator repaired".into(),
                evidence: vec![],
                episode: None,
                idempotency_key: "client-simulator-response".into(),
                answer: None,
            },
            false,
        )
        .unwrap();
    let (_, response) = client_json(app, "/v1/client/work").await;
    let item = response["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == subject)
        .unwrap();
    assert_eq!(item["state"], "ready");
    assert_eq!(item["blocked_reason"], Value::Null);
    assert_eq!(item["blockers"], serde_json::json!([]));
}

#[tokio::test]
async fn stopped_agents_are_annotated_history_not_default_membership() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    state
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: "agent/retired".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/retired".into()),
            fields: std::collections::BTreeMap::from([
                ("runtime_id".into(), Value::String("retired-runtime".into())),
                (
                    "incarnation_id".into(),
                    Value::String("retired-runtime:i1".into()),
                ),
                ("status".into(), Value::String("stopped".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let app = st3::api::router(state);
    let (_, current) = client_json(app.clone(), "/v1/client/agents").await;
    assert!(current["value"]["items"].as_array().unwrap().is_empty());
    let (_, history) = client_json(app, "/v1/client/agents?history=true").await;
    let retired = &history["value"]["items"][0];
    assert_eq!(retired["id"], "agent/retired");
    assert_eq!(retired["operational"]["layer"], "history");
    assert_eq!(retired["operational"]["actionable"], false);
    assert!(
        retired["operational"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&Value::String("stopped".into()))
    );
}

#[tokio::test]
async fn client_v0_action_route_accepts_the_golden_fenced_action() {
    let root = tempfile::tempdir().unwrap();
    let action = std::fs::read(asset_root().join("fixtures/action.json")).unwrap();
    let response = st3::api::router(test_state(root.path()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/client/actions")
                .header("content-type", "application/json")
                .body(Body::from(action))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_queue_read_and_person_move_share_one_seat_order() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let source = r#"
version 2
agent "queue-seat" { workspace "/tmp"; command "true" }
mission "queued-work" state="ready" {
  concurrent-runs
  goal "Give the durable seat one step in each run."
  step "work" { assigned-to "agent/queue-seat" }
}
"#;
    let intent = st3::graph::parse_intent(source, "client-v0-baseline").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "agent-queue-missions")
        .unwrap();
    let mut runs = Vec::new();
    for index in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(2));
        let run = state
            .store
            .create_mission_run(&st3::model::MissionRunRequest {
                mission: "queued-work".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: std::collections::BTreeMap::new(),
                idempotency_key: format!("agent-queue-run-{index}"),
            })
            .unwrap();
        state
            .store
            .set_step_state(&run.steps[0].subject, "ready", None)
            .unwrap();
        runs.push(run);
    }
    let seat = runs[0].steps[0].assigned_to.clone().unwrap();
    let app = st3::api::router(state.clone());
    let queue_path = format!("/v1/client/agent-queues/{seat}");
    let order = |queue: &Value| {
        queue["value"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["mission_run_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };

    let (status, queue) = client_json(app.clone(), &queue_path).await;
    assert_eq!(status, StatusCode::OK, "{queue}");
    assert_eq!(queue["value"]["kind"], "agent-queue");
    assert_eq!(queue["value"]["agent_id"], seat.as_str());
    assert_eq!(
        order(&queue),
        [
            runs[0].subject.clone(),
            runs[1].subject.clone(),
            runs[2].subject.clone()
        ]
    );
    assert_eq!(
        queue["value"]["next_work_id"],
        runs[0].steps[0].subject.as_str()
    );
    assert_eq!(queue["value"]["runs"][0]["state"], "ready");
    assert_eq!(queue["value"]["move_count"], 0);

    let (_, capabilities) =
        client_json_person(app.clone(), "/v1/client/capabilities", "person/operator").await;
    assert!(
        capabilities["value"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability["id"] == "agent.queue-move"
                && capability["state"] == "granted")
    );
    let snapshot = capabilities["snapshot"]["id"].as_str().unwrap();
    let move_to_top = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/agent-queue-top", "type": "agent.queue-move",
        "idempotency_key": "agent-queue-move-top-0001",
        "fence": { "snapshot_id": snapshot, "subject_revisions": {} },
        "parameters": {
            "agent_id": seat, "mission_run_id": runs[2].subject, "placement": "top",
            "reason": "the release needs it first"
        }
    });
    let (status, result) = client_post_json_person(
        app.clone(),
        "/v1/client/actions",
        "person/operator",
        move_to_top,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["value"]["affected_ids"], serde_json::json!([seat]));

    let (_, queue) = client_json(app.clone(), &queue_path).await;
    assert_eq!(
        order(&queue),
        [
            runs[2].subject.clone(),
            runs[0].subject.clone(),
            runs[1].subject.clone()
        ]
    );
    assert_eq!(
        queue["value"]["next_work_id"],
        runs[2].steps[0].subject.as_str()
    );
    let (status, listed) = client_json(app.clone(), &format!("/v1/client/work?actor={seat}")).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let listed = listed["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            runs[2].steps[0].subject.clone(),
            runs[0].steps[0].subject.clone(),
            runs[1].steps[0].subject.clone()
        ],
        "the seat's own work list follows its queue"
    );
    let moved = &queue["value"]["moves"][0];
    assert_eq!(queue["value"]["move_count"], 1);
    assert_eq!(moved["actor_id"], "person/operator");
    assert_eq!(moved["mission_run_id"], runs[2].subject.as_str());
    assert_eq!(moved["placement"], "top");
    assert_eq!(moved["anchor_run_id"], Value::Null);
    assert_eq!(moved["reason"], "the release needs it first");

    let (_, capabilities) = client_json(app.clone(), "/v1/client/capabilities").await;
    let snapshot = capabilities["snapshot"]["id"].as_str().unwrap();
    let absent = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/agent-queue-absent", "type": "agent.queue-move",
        "idempotency_key": "agent-queue-move-absent-01",
        "fence": { "snapshot_id": snapshot, "subject_revisions": {} },
        "parameters": {
            "agent_id": seat, "mission_run_id": "mission-run/absent", "placement": "after",
            "anchor_run_id": runs[0].subject
        }
    });
    let (status, error) =
        client_post_json_person(app.clone(), "/v1/client/actions", "person/operator", absent).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["code"], "validation-failed", "{error}");
    assert_eq!(error["details"]["run"], "mission-run/absent");

    let (status, missing) = client_json(app, "/v1/client/agent-queues/agent/absent-seat").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
}

/// A runtime that starts nothing: a reconcile pass here only writes a run's own declarations.
struct NoRuntime;

impl st3::reconcile::RuntimeControl for NoRuntime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<st3::reconcile::RuntimeObservation>> {
        Ok(Vec::new())
    }
    fn observe_exec(&self, _: &str) -> anyhow::Result<Option<st3::reconcile::RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &st3::model::MemberSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove(&self, _: &str, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// Run one reconcile pass as the store's own node, which writes each of its running missions'
/// lane declarations.
fn materialize_run_declarations(store: &Arc<Store>) {
    st3::reconcile::Reconciler::new(
        store.clone(),
        Arc::new(NoRuntime),
        "client-v0-baseline".into(),
        Arc::new(Notify::new()),
    )
    .reconcile_once()
    .unwrap();
}

#[tokio::test]
async fn lanes_read_as_resources_and_people_change_them_through_actions() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let source = r#"
version 2
agent "lane-driver" { workspace "/tmp"; command "true" }
mission "example/merge-train" state="ready" {
  goal "Merge ready changes into main one at a time."
  lane "app" {
    entries "resource/github/acme/app/ci/pull-request/"
    approver "person/ada"
  }
  step "drive" { assigned-to "agent/lane-driver" }
}
"#;
    let intent = st3::graph::parse_intent(source, "client-v0-baseline").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "lane-missions")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&st3::model::MissionRunRequest {
            mission: "example/merge-train".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: std::collections::BTreeMap::new(),
            idempotency_key: "lane-run".into(),
        })
        .unwrap();
    materialize_run_declarations(&state.store);
    let lane = format!(
        "lane/{}/app",
        run.subject.strip_prefix("mission-run/").unwrap()
    );
    let entry = |number: &str| format!("resource/github/acme/app/ci/pull-request/{number}");
    let app = st3::api::router(state.clone());

    let (status, listed) = client_json(app.clone(), "/v1/client/lanes").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["value"]["collection"], "lanes");
    let item = &listed["value"]["items"][0];
    assert_eq!(item["id"], lane.as_str());
    assert_eq!(item["kind"], "lane");
    assert_eq!(item["name"], "app");
    assert_eq!(item["mission_run_id"], run.subject.as_str());
    assert_eq!(item["mission_id"], "mission/example/merge-train");
    assert_eq!(item["approver_id"], "person/ada");
    assert_eq!(item["state"], "open");
    assert_eq!(item["revision"], "empty");
    assert_eq!(item["entries"], serde_json::json!([]));

    let (_, capabilities) =
        client_json_person(app.clone(), "/v1/client/capabilities", "person/operator").await;
    for action in [
        "lane.join",
        "lane.leave",
        "lane.move",
        "lane.mark",
        "lane.approve",
    ] {
        assert!(
            capabilities["value"]["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|capability| capability["id"] == action && capability["state"] == "granted"),
            "{action} is not granted"
        );
    }
    // Each action fences on a fresh snapshot, as a client does after it reads.
    let act = |person: &'static str, id: &'static str, kind: &'static str, parameters: Value| {
        let app = app.clone();
        async move {
            let (_, capabilities) =
                client_json_person(app.clone(), "/v1/client/capabilities", person).await;
            let snapshot = capabilities["snapshot"]["id"].as_str().unwrap().to_owned();
            let request = serde_json::json!({
                "api_version": "st3.client.v0", "id": format!("action/{id}"), "type": kind,
                "idempotency_key": format!("lane-action-{id}-0000000"),
                "fence": { "snapshot_id": snapshot, "subject_revisions": {} },
                "parameters": parameters
            });
            client_post_json_person(app, "/v1/client/actions", person, request).await
        }
    };
    for (id, number) in [("join-42", "42"), ("join-43", "43")] {
        let (status, result) = act(
            "person/operator",
            id,
            "lane.join",
            serde_json::json!({ "lane_id": lane, "entry_id": entry(number), "reason": "green" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["value"]["affected_ids"], serde_json::json!([lane]));
    }
    let (status, result) = act(
        "person/operator",
        "move-43",
        "lane.move",
        serde_json::json!({ "lane_id": lane, "entry_id": entry("43"), "placement": "before", "anchor_id": entry("42") }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, denied) = act(
        "person/operator",
        "approve-denied",
        "lane.approve",
        serde_json::json!({ "lane_id": lane, "entry_id": entry("42") }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(denied["code"], "forbidden", "{denied}");
    let (status, result) = act(
        "person/ada",
        "approve-42",
        "lane.approve",
        serde_json::json!({ "lane_id": lane, "entry_id": entry("42") }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, missing) = act(
        "person/operator",
        "mark-missing",
        "lane.mark",
        serde_json::json!({ "lane_id": lane, "entry_id": entry("99"), "state": "ready" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{missing}");
    assert_eq!(missing["code"], "validation-failed", "{missing}");

    let (status, detail) = client_json(app.clone(), &format!("/v1/client/lanes/{lane}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let detail = &detail["value"];
    let labels = detail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["label"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["43", "42"]);
    assert_eq!(detail["entries"][1]["approved_by_id"], "person/ada");
    assert_eq!(detail["entries"][0]["joined_by_id"], "person/operator");
    assert_eq!(detail["entries"][0]["state"], "waiting");
    assert_eq!(detail["recent"][0]["change"], "approved");
    assert_eq!(detail["recent"][1]["change"], "moved");
    assert_eq!(detail["recent"][1]["anchor_id"], entry("42"));
    assert_ne!(detail["revision"], "empty");
    let typed: st3_client::Resource = serde_json::from_value(detail.clone()).unwrap();
    assert!(matches!(typed, st3_client::Resource::Lane(_)), "{detail}");

    let (status, absent) = client_json(app, "/v1/client/lanes/lane/absent/app").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{absent}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_structured_decision_carries_named_answers_both_ways() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "structured-person-asker")
        .unwrap();
    let actor = format!("agent/{}.asker", state.store.origin());
    let app = st3::api::router(state.clone());
    let request = serde_json::json!({
        "version": 1,
        "type": "decision",
        "question": "Land the three parser PRs in order?",
        "why_person": "The owner approves merges to the public repository.",
        "recommendation": {"answer": "land", "reason": "Checks are green on every head."},
        "subjects": [
            {"kind": "pull_request", "label": "#11", "url": "https://example.com/pull/11", "revision": "aaa111"},
            {"kind": "pull_request", "label": "#12", "url": "https://example.com/pull/12", "revision": "bbb222"}
        ],
        "answers": [
            {"id": "land", "label": "Land #11 then #12", "outcome": "accept", "consequence": "The asker queues #11, then #12."},
            {"id": "keep-open", "label": "Keep both open", "outcome": "decline", "consequence": "Nothing merges."}
        ]
    });
    let (_, snapshot) = client_json(app.clone(), "/v1/client/attention").await;
    let mut ask = serde_json::json!({"api_version":"st3.client.v0", "id":"action/structured-ask", "type":"work.ask",
        "idempotency_key":"structured-ask-0001", "fence":{"snapshot_id":snapshot["snapshot"]["id"]}, "parameters":{"person_id":"person/avery",
        "title":"Land the parser PRs?", "reason":"Land the three parser PRs in order?", "new_run":"parser-prs", "request": request}});
    ask["parameters"]["request"]["answers"][1]["outcome"] = serde_json::json!("accept");
    let (status, rejected) =
        client_post_json_person(app.clone(), "/v1/client/actions", &actor, ask.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    ask["parameters"]["request"]["answers"][1]["outcome"] = serde_json::json!("decline");
    let (status, response) =
        client_post_json_person(app.clone(), "/v1/client/actions", &actor, ask).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let (status, attention) = client_json(app.clone(), "/v1/client/attention").await;
    assert_eq!(status, StatusCode::OK, "{attention}");
    let card = &attention["value"]["items"][0];
    assert_eq!(card["attention_kind"], "person-step");
    assert_eq!(card["request"], request);
    let subject = card["source_id"].as_str().unwrap().to_owned();
    let episode = card["episode"].as_str().unwrap().to_owned();
    let done = |id: &str, parameters: Value| {
        serde_json::json!({"api_version":"st3.client.v0", "id": format!("action/{id}"), "type":"work.done",
            "idempotency_key": format!("{id}-0001"), "fence":{"snapshot_id":attention["snapshot"]["id"]},
            "parameters": parameters})
    };
    // A summary alone cannot choose between land and keep-open.
    let (status, rejected) = client_post_json_person(
        app.clone(),
        "/v1/client/actions",
        "person/avery",
        done(
            "structured-words",
            serde_json::json!({"target_id": subject, "episode": episode, "summary": "yes"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    assert_eq!(rejected["code"], "validation-failed");
    assert!(
        rejected["message"]
            .as_str()
            .unwrap()
            .contains("land, keep-open"),
        "{rejected}"
    );
    let (status, response) = client_post_json_person(
        app.clone(),
        "/v1/client/actions",
        "person/avery",
        done(
            "structured-answer",
            serde_json::json!({"target_id": subject, "episode": episode, "summary": "Keep both open",
                "answer": {"id": "keep-open", "text": "Wait for the schema change."}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let (status, work) = client_json(app, &format!("/v1/client/work/{subject}")).await;
    assert_eq!(status, StatusCode::OK, "{work}");
    let answer = &work["value"]["person_answers"][0];
    assert_eq!(answer["respondent"], "person/avery");
    assert_eq!(
        answer["answer"],
        serde_json::json!({"type": "decision", "outcome": "decline", "id": "keep-open",
            "label": "Keep both open", "text": "Wait for the schema change."})
    );
}

#[tokio::test]
async fn native_agent_person_asks_use_source_authority_and_reject_malformed_evidence() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "native-person-asker")
        .unwrap();
    let actor = format!("agent/{}.asker", state.store.origin());
    let app = st3::api::router(state.clone());
    let (_, snapshot) = client_json(app.clone(), "/v1/client/attention").await;
    let ask = serde_json::json!({"api_version":"st3.client.v0", "id":"action/native-ask", "type":"work.ask",
        "idempotency_key":"native-ask-00000001", "fence":{"snapshot_id":snapshot["snapshot"]["id"]}, "parameters":{"person_id":"person/avery",
        "title":"Choose date", "reason":"Release needs a date", "new_run":"release-date"}});
    let (status, response) =
        client_post_json_person(app.clone(), "/v1/client/actions", &actor, ask.clone()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let (status, repeated) =
        client_post_json_person(app.clone(), "/v1/client/actions", &actor, ask).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(response["affected_ids"], repeated["affected_ids"]);
    let card = state
        .store
        .attention_items(Some("person/avery"))
        .unwrap()
        .pop()
        .unwrap();
    let (_, snapshot) = client_json(app.clone(), "/v1/client/attention").await;
    let done = serde_json::json!({"api_version":"st3.client.v0", "id":"action/native-done", "type":"work.done",
        "idempotency_key":"native-done-00000001", "fence":{"snapshot_id":snapshot["snapshot"]["id"]}, "parameters":{"target_id":card.subject,
        "episode":card.episode, "summary":"Friday", "evidence":[17]}});
    let (status, rejected) =
        client_post_json_person(app.clone(), "/v1/client/actions", "person/avery", done).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    assert_eq!(
        state.store.step_run(&card.subject).unwrap().unwrap().status,
        "ready"
    );
    let cancel = serde_json::json!({"api_version":"st3.client.v0", "id":"action/native-cancel", "type":"work.cancel-ask",
        "idempotency_key":"native-cancel-00000001", "fence":{"snapshot_id":snapshot["snapshot"]["id"]}, "parameters":{"target_id":card.subject,
        "episode":card.episode, "summary":"The release was withdrawn", "evidence":[]}});
    let (status, rejected) = client_post_json_person(
        app.clone(),
        "/v1/client/actions",
        "person/robin",
        cancel.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{rejected}");
    let (status, response) =
        client_post_json_person(app, "/v1/client/actions", &actor, cancel).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(
        state
            .store
            .attention_items(Some("person/avery"))
            .unwrap()
            .is_empty()
    );
}
