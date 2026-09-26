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
        "runtimes.list",
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
        "work.publish-mission",
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
        "machine",
        "message",
        "mission",
        "operation",
        "runtime",
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
    assert_eq!(attention["person_id"], "person/nathan");
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
fn event_and_terminal_streams_are_contiguous_and_resumable() {
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

    let frames = fixture("terminal-frames.json");
    let expected_incarnation = frames["value"]["runtime_incarnation"].as_str().unwrap();
    let frames = frames["value"]["frames"].as_array().unwrap();
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["runtime_incarnation"], expected_incarnation);
        if index > 0 {
            assert_eq!(
                frames[index - 1]["sequence"].as_u64().unwrap() + 1,
                frame["sequence"].as_u64().unwrap()
            );
        }
    }
}

fn test_state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory("client-v0-baseline").unwrap()),
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
                .header("x-st3-person", "person/nathan")
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
            requester: Some("person/nathan".into()),
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
    let (_, default) = client_json_person(app.clone(), "/v1/client/now", "person/nathan").await;
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
        "person/nathan",
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
            "person_id": "person/nathan"
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
            "api_version": "st3.client.v0", "device_name": "Test phone", "person_id": "person/nathan"
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
            actor: Some("person/nathan".into()),
            fields: std::collections::BTreeMap::from([
                (
                    "credential_hash".into(),
                    Value::String(hex::encode(Sha256::digest(b"expired inventory credential"))),
                ),
                (
                    "device_id".into(),
                    Value::String("device/expired-inventory".into()),
                ),
                ("person_id".into(), Value::String("person/nathan".into())),
                (
                    "session_actor".into(),
                    Value::String("person/nathan/session/expired-inventory".into()),
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
        client_json_person(app.clone(), "/v1/client/devices", "person/nathan").await;
    assert_eq!(status, StatusCode::OK, "{devices}");
    let encoded_devices = serde_json::to_string(&devices).unwrap();
    assert_eq!(devices["value"]["items"][0]["id"], device);
    assert_eq!(devices["value"]["items"][0]["state"], "active");
    assert!(!encoded_devices.contains("credential_hash"));
    assert!(!encoded_devices.contains("device_public_key"));
    let (status, device_history) = client_json_person(
        app.clone(),
        "/v1/client/devices?history=true",
        "person/nathan",
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
        client_json_person(app.clone(), "/v1/client/devices", "person/nathan").await;
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
        "person/nathan",
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
    for (subject, reviewer) in [
        ("attention/nathan-paired", "person/nathan"),
        ("attention/alex-paired", "person/alex"),
    ] {
        store
            .request_attention(
                subject,
                &st3::model::AttentionRequest {
                    reviewer: reviewer.into(),
                    title: format!("Attention for {reviewer}"),
                    reason: "Paired authority conformance".into(),
                    severity: "error".into(),
                    targets: vec!["mission/paired-proof".into()],
                    actor: "daemon/runtime".into(),
                    idempotency_key: format!("paired-{subject}"),
                },
            )
            .unwrap();
    }
    let local = st3::api::router(state.clone());
    let fabric = st3::api::fabric_router(state.clone());

    let (status, challenge) = client_post_json_person(
        local.clone(),
        "/v1/client/pairings",
        "person/nathan",
        serde_json::json!({
            "api_version": "st3.client.v0", "device_name": "Nathan's phone", "person_id": "person/nathan"
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
    assert_eq!(paired["value"]["person_id"], "person/nathan");
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
    assert_eq!(capability_state("attention.resolve"), "granted");
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
            .find(|item| item["id"] == id)
            .unwrap()
    };
    let nathan = item("attention/nathan-paired");
    assert_eq!(nathan["attention_kind"], "fault");
    assert_eq!(nathan["source_id"], "attention/nathan-paired");
    assert_eq!(nathan["person_id"], "person/nathan");
    assert_eq!(nathan["actions"], serde_json::json!(["attention.resolve"]));
    let resolve = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-attention-nathan",
        "type": "attention.resolve", "idempotency_key": "paired-attention-nathan-0001",
        "fence": {
            "snapshot_id": attention["snapshot"]["id"],
            "subject_revisions": { "attention/nathan-paired": nathan["revision"] }
        },
        "parameters": { "attention_id": "attention/nathan-paired", "outcome": "resolved" }
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
            .all(|item| item["id"] != "attention/alex-paired")
    );
    let (status, cross_person_read) = client_json_auth(
        fabric.clone(),
        "/v1/client/attention?person=person%2Falex",
        credential,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{cross_person_read}");
    let alex_revision = store
        .claims_for("attention/alex-paired", None)
        .unwrap()
        .last()
        .unwrap()
        .id
        .clone();
    let cross_attention = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-attention-alex",
        "type": "attention.resolve", "idempotency_key": "paired-attention-alex-000001",
        "fence": {
            "snapshot_id": attention["snapshot"]["id"],
            "subject_revisions": { "attention/alex-paired": alex_revision }
        },
        "parameters": { "attention_id": "attention/alex-paired", "outcome": "resolved" }
    });
    let (status, denied) = client_post_json_auth(
        fabric.clone(),
        "/v1/client/actions",
        credential,
        cross_attention,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");

    let (_, current) =
        client_json_auth(fabric.clone(), "/v1/client/capabilities", credential).await;
    let ungranted = serde_json::json!({
        "api_version": "st3.client.v0", "id": "action/paired-message-denied",
        "type": "message.send", "idempotency_key": "paired-message-denied-0001",
        "fence": { "snapshot_id": current["snapshot"]["id"], "subject_revisions": {} },
        "parameters": { "to": "person/nathan", "content": "not delegated" }
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
            "title": "Nathan launch", "request": "Draft a paired mission.",
            "target": { "type": "new-mission", "mission_id": "mission/paired-nathan", "workspace": workspace }
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
        "person/nathan"
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
            actor: Some("person/nathan".into()),
            fields: std::collections::BTreeMap::from([
                (
                    "credential_hash".into(),
                    Value::String(hex::encode(Sha256::digest(expired_credential.as_bytes()))),
                ),
                ("person_id".into(), Value::String("person/nathan".into())),
                (
                    "session_actor".into(),
                    Value::String("person/nathan/session/expired".into()),
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
        "person_id": "person/nathan",
        "full_control": true
    });
    let (status, denied) =
        client_post_json(paired_gateway.clone(), "/v1/client/pairings", begin.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, challenge) =
        client_post_json_person(local.clone(), "/v1/client/pairings", "person/nathan", begin).await;
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
        client_json_person(local.clone(), "/v1/client/capabilities", "person/nathan").await;
    let (status, revoked) = client_post_json_person(
        local,
        "/v1/client/actions",
        "person/nathan",
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
    assert_eq!(session.requester, "person/nathan");

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
    let (_, current_missions) = client_json(app.clone(), "/v1/client/missions").await;
    assert!(
        current_missions["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != "mission/client-action-demo")
    );
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
        "history",
        "sessions",
    ] {
        let (status, envelope) =
            client_json(app.clone(), &format!("/v1/client/{collection}")).await;
        assert_eq!(status, StatusCode::OK, "{collection}: {envelope}");
        assert_eq!(envelope["api_version"], "st3.client.v0");
        assert_eq!(envelope["value"]["kind"], "page");
        assert_eq!(envelope["value"]["collection"], collection);
        assert!(envelope["value"]["items"].is_array());
        assert_eq!(envelope["value"]["page"]["limit"], 50);
    }

    let (_, first) = client_json(app.clone(), "/v1/client/agents?limit=1").await;
    assert_eq!(first["value"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["value"]["page"]["has_more"], true);
    let cursor = first["value"]["page"]["next_cursor"].as_str().unwrap();
    let uri = format!(
        "/v1/client/agents?limit=1&cursor={}",
        urlencoding::encode(cursor)
    );
    let (_, second) = client_json(app.clone(), &uri).await;
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
    assert_eq!(first["snapshot"]["id"], continued["snapshot"]["id"]);
    assert_eq!(second["value"]["items"], continued["value"]["items"]);
}

#[tokio::test]
async fn client_work_projection_exposes_external_blocker_and_reopens_after_resolution() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state(root.path());
    let source = r#"
version 2
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
            requester: Some("person/nathan".into()),
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
    let attention = state
        .store
        .request_attention(
            "attention/client-silber-xcode",
            &st3::model::AttentionRequest {
                reviewer: "person/nathan".into(),
                title: "Silber needs its Xcode simulator components updated".into(),
                reason: "CoreSimulator is unavailable.".into(),
                severity: "error".into(),
                targets: vec!["host/silber".into(), subject.clone()],
                actor: "agent/client-v0-baseline.ios-owner".into(),
                idempotency_key: "client-silber-xcode".into(),
            },
        )
        .unwrap();
    let reason = "Silber requires a privileged Xcode first-launch repair.";
    state
        .store
        .work_action(
            &subject,
            "release",
            &request("client-blocker-release", Some(reason)),
        )
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
    assert_eq!(item["state"], "blocked");
    assert_eq!(item["blocked_reason"], reason);
    assert_eq!(item["blockers"], serde_json::json!([attention.subject]));
    assert_eq!(item["operational"]["actionable"], false);
    assert!(
        item["operational"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&Value::String("external-blocker".into()))
    );

    state
        .store
        .resolve_attention(
            "attention/client-silber-xcode",
            &st3::model::AttentionResolveRequest {
                outcome: "resolved".into(),
                reason: Some("The simulator runtime is available.".into()),
                actor: "person/nathan".into(),
                idempotency_key: "client-silber-resolved".into(),
            },
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
