use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use st3::{api::AppState, store::Store};
use std::{path::Path, sync::Arc};
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

fn state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory("creation-test").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "creation-test".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
        private_notes: Default::default(),
    }
}
async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    person: Option<&str>,
    key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(person) = person {
        request = request.header("x-st3-person", person);
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    let response = app
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap(),
    )
}
async fn action(
    app: axum::Router,
    person: &str,
    kind: &str,
    key: &str,
    parameters: Value,
) -> (StatusCode, Value) {
    let (_, caps) = request(
        app.clone(),
        "GET",
        "/v1/client/capabilities",
        Some(person),
        None,
        Value::Null,
    )
    .await;
    request(app, "POST", "/v1/client/actions", Some(person), None, json!({
        "api_version":"st3.client.v0", "id":format!("action/{key}"), "type":kind, "idempotency_key":key,
        "fence":{"snapshot_id":caps["snapshot"]["id"]}, "parameters":parameters
    })).await
}
#[tokio::test]
async fn personal_shell_creation_retry_and_end_use_person_ownership() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = st3::api::router(state.clone());
    let (status, result) = action(
        app.clone(),
        "person/ada",
        "terminal.create",
        "shell-create-00001",
        json!({"name":"A shell", "cwd":"/tmp"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let id = result["value"]["affected_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(id.starts_with("terminal/pty/person/ada/"), "{id}");
    let desired = state.store.desired_subjects().unwrap();
    assert_eq!(desired.len(), 1);
    let member = desired[0].member.as_ref().unwrap();
    assert_eq!(member.driver, None);
    assert_eq!(member.display_name.as_deref(), Some("A shell"));
    assert_eq!(member.restart, st3::model::RestartType::Never);
    assert_eq!(member.cwd, "/tmp");
    // A retry with a refreshed fence returns its durable action receipt.
    let (_, caps) = request(
        app.clone(),
        "GET",
        "/v1/client/capabilities",
        Some("person/ada"),
        None,
        Value::Null,
    )
    .await;
    let retry = json!({"api_version":"st3.client.v0", "id":"action/shell-create-00001", "type":"terminal.create", "idempotency_key":"shell-create-00001", "fence":{"snapshot_id":caps["snapshot"]["id"]}, "parameters":{"name":"A shell", "cwd":"/tmp"}});
    // Refreshing the snapshot does not turn an accepted create into another mutation.
    let (status, replay) = request(
        app.clone(),
        "POST",
        "/v1/client/actions",
        Some("person/ada"),
        None,
        retry.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(
        replay["value"]["affected_ids"],
        result["value"]["affected_ids"]
    );
    assert_eq!(
        replay["value"]["operation_id"],
        result["value"]["operation_id"]
    );
    let mut different = retry;
    different["parameters"]["name"] = json!("A different shell");
    let (status, conflict) = request(
        app.clone(),
        "POST",
        "/v1/client/actions",
        Some("person/ada"),
        None,
        different,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "idempotency-conflict");
    assert_eq!(state.store.desired_subjects().unwrap().len(), 1);
    let (denied, _) = action(
        app.clone(),
        "person/alex",
        "terminal.end",
        "shell-end-other-00001",
        json!({"target_id":id}),
    )
    .await;
    assert_eq!(denied, StatusCode::FORBIDDEN);
    let (ended, body) = action(
        app.clone(),
        "person/ada",
        "terminal.end",
        "shell-end-owner-00001",
        json!({"target_id":id}),
    )
    .await;
    assert_eq!(ended, StatusCode::OK, "{body}");
    assert_eq!(
        state
            .store
            .selected_desired_kind(id.strip_prefix("terminal/").unwrap())
            .unwrap()
            .as_deref(),
        Some("stop")
    );
    let (status, agent_shell) = action(
        app.clone(),
        "agent/worker",
        "terminal.create",
        "shell-create-agent-01",
        json!({"name":"Agent shell", "cwd":"/tmp"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent_shell}");
    let agent_id = agent_shell["value"]["affected_ids"][0].as_str().unwrap();
    assert!(agent_id.starts_with("terminal/pty/agent/worker/"));
    assert_eq!(
        action(
            app.clone(),
            "agent/worker",
            "terminal.end",
            "shell-end-agent-0001",
            json!({"target_id":agent_id})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn agent_creation_has_typed_parameters_and_native_first_message() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = st3::api::router(state.clone());
    let (status, result) = action(
        app.clone(),
        "person/ada",
        "agent.create",
        "agent-create-00001",
        json!({"name":"worker", "harness":"codex", "workspace":"/tmp", "message":"--write a test"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        result["value"]["affected_ids"][0],
        "agent/creation-test.worker"
    );
    let members = state.store.desired_subjects().unwrap();
    let st3::model::LaunchSpec::Argv(argv) = &members[0].member.as_ref().unwrap().launch else {
        panic!()
    };
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--initial-message", "--write a test"])
    );
    assert_eq!(
        action(
            app.clone(),
            "person/ada",
            "agent.create",
            "agent-create-00002",
            json!({"name":"worker", "harness":"codex", "workspace":"/tmp"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        action(
            app.clone(),
            "person/ada",
            "agent.create",
            "agent-create-bad-001",
            json!({"name":"bad", "harness":"invented", "workspace":"/tmp"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        action(
            app.clone(),
            "person/ada",
            "agent.create",
            "agent-create-bad-002",
            json!({"name":"bad", "harness":"codex", "workspace":"relative"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        action(
            app.clone(),
            "person/ada",
            "terminal.create",
            "terminal-create-bad",
            json!({"name":"bad", "command":"rm"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, created) = action(
        app.clone(),
        "agent/example/operator",
        "agent.create",
        "agent-free-mode-0001",
        json!({"name":"second", "harness":"omp", "workspace":"/tmp", "message":"First"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let subject = created["value"]["affected_ids"][0].as_str().unwrap();
    let claim = state
        .store
        .latest_claim(subject, Some("intent.desired"))
        .unwrap()
        .unwrap();
    assert_eq!(claim.actor.as_deref(), Some("agent/example/operator"));
    assert_eq!(members.len(), 1);
}

fn replicate(source: &Store, target: &Store) {
    const FLEET: &str = "creation-test-fleet";
    source.bind_fleet(FLEET).unwrap();
    target.bind_fleet(FLEET).unwrap();
    for _ in 0..100 {
        let inventory = target.replication_inventory().unwrap();
        if inventory.digest == source.replication_inventory().unwrap().digest {
            return;
        }
        let exchange = source
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        target
            .receive_replication_exchange("creation-test", FLEET, &exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        target.project_replication_backlog().unwrap();
    }
    panic!("replica did not converge");
}

#[tokio::test]
async fn first_message_receipt_survives_reopen_and_replication() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("claims.sqlite");
    let mut original = state(root.path());
    original.store = Arc::new(Store::open(&database, "creation-test").unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = st3::client::Client::new(st3::client::Endpoint::Http(format!(
        "http://{}",
        listener.local_addr().unwrap()
    )));
    let server_state = original.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, st3::api::router(server_state))
            .await
            .unwrap();
    });
    assert!(
        st3::creation::claim_initial_message(&client, "agent/test/worker", "launch-1", "i1")
            .await
            .unwrap()
    );
    assert!(
        !st3::creation::claim_initial_message(&client, "agent/test/worker", "launch-1", "i1")
            .await
            .unwrap()
    );
    assert!(
        !st3::creation::claim_initial_message(&client, "agent/test/worker", "launch-1", "i2")
            .await
            .unwrap()
    );
    assert!(
        st3::creation::claim_initial_message(&client, "agent/test/worker", "launch-2", "i2")
            .await
            .unwrap()
    );
    server.abort();
    let _ = server.await;
    drop(original);
    let reopened = Store::open(&database, "creation-test").unwrap();
    let mut second = state(root.path());
    second.node = "replica-test".into();
    second.store = Arc::new(Store::open_memory("replica-test").unwrap());
    replicate(&reopened, &second.store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = st3::client::Client::new(st3::client::Endpoint::Http(format!(
        "http://{}",
        listener.local_addr().unwrap()
    )));
    let server = tokio::spawn(async move {
        axum::serve(listener, st3::api::router(second))
            .await
            .unwrap();
    });
    assert!(
        !st3::creation::claim_initial_message(&client, "agent/test/worker", "launch-1", "i3")
            .await
            .unwrap()
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn terminal_declaration_survives_replication_and_rejects_another_claim_actor() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = st3::api::router(state.clone());
    let (status, result) = action(
        app,
        "person/ada",
        "terminal.create",
        "replicated-shell-001",
        json!({"name":"Remote shell", "host":"other-host", "cwd":"/tmp"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let desired = state.store.desired_subjects().unwrap().pop().unwrap();
    let target = Store::open_memory("replica-test").unwrap();
    replicate(&state.store, &target);
    assert_eq!(target.desired_subjects().unwrap(), vec![desired.clone()]);
    let claim = state
        .store
        .latest_claim(&desired.subject, Some("intent.desired"))
        .unwrap()
        .unwrap();
    let fields = std::collections::BTreeMap::from([
        ("kind".into(), claim.body["kind"].clone()),
        ("revision".into(), json!("forged")),
        ("desired".into(), claim.body["desired"].clone()),
    ]);
    let forged = st3::model::ClaimInput {
        subject: desired.subject,
        kind: "intent.desired".into(),
        actor: Some("person/alex".into()),
        fields,
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    };
    assert_eq!(
        target.append_claim(&forged).unwrap_err().code,
        "terminal-owner-forbidden"
    );
}
