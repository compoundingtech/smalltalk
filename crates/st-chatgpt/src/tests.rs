use super::*;
use axum::{body::{Body, to_bytes}, http::Request};
use std::os::unix::fs::{PermissionsExt, symlink};
use tower::ServiceExt;

fn disconnected_adapter() -> Adapter {
    Adapter {
        client: reqwest::Client::builder().unix_socket("/nonexistent/st-chatgpt-tests/st3-client.sock").build().unwrap(),
        bearer: "test-only-not-a-daemon-credential".into(),
    }
}

#[test]
fn discovery_has_exactly_seven_closed_tools_and_correct_hints() {
    let expected = [
        ("st_alerts_list", true, false, false),
        ("st_alert_get", true, false, false),
        ("st_alert_answer", false, true, false),
        ("st_missions_list", true, false, false),
        ("st_agents_list", true, false, false),
        ("st_conversation_read", true, false, false),
        ("st_message_send", false, false, true),
    ];
    let tools = tool_definitions();
    assert_eq!(tools.len(), expected.len());
    for (tool, (name, read, destructive, open_world)) in tools.iter().zip(expected) {
        let published = serde_json::to_value(tool).unwrap();
        assert_eq!(published["name"], name);
        assert_eq!(published["annotations"], json!({
            "readOnlyHint": read, "destructiveHint": destructive,
            "idempotentHint": true, "openWorldHint": open_world,
        }));
        assert_eq!(published["inputSchema"]["type"], "object");
        assert_eq!(published["inputSchema"]["additionalProperties"], false);
    }
}

#[test]
fn writes_publish_typed_closed_fences() {
    for name in ["st_alert_answer", "st_message_send"] {
        let contract = schema(name);
        let fence = &contract["properties"]["fence"];
        assert_eq!(fence["type"], "object");
        assert_eq!(fence["required"], json!(["snapshot_id"]));
        assert_eq!(fence["additionalProperties"], false);
        assert_eq!(fence["properties"]["snapshot_id"]["type"], "string");
        assert_eq!(fence["properties"]["subject_revisions"]["type"], "object");
        assert_eq!(fence["properties"]["subject_revisions"]["additionalProperties"], json!({"type":"string"}));
        assert!(fence["description"].as_str().unwrap().contains("snapshot"));
    }
}

fn send_arguments() -> Value {
    json!({"action_id":"action/test", "idempotency_key":"test-key",
        "fence":{"snapshot_id":"snapshot/test", "subject_revisions":{"agent/recipient":"claim/test"}},
        "to":"agent/recipient", "title":"Hello", "content":"Exact content"})
}

#[test]
fn validation_accepts_every_published_tool_contract() {
    for name in ["st_alerts_list", "st_missions_list", "st_agents_list"] {
        assert!(validate_arguments(name, &json!({})).is_ok());
        assert!(validate_arguments(name, &json!({"limit":1,"cursor":"opaque","max_response_bytes":16384})).is_ok());
    }
    assert!(validate_arguments("st_alert_get", &json!({"alert_id":"attention/test"})).is_ok());
    assert!(validate_arguments("st_conversation_read", &json!({"agent_id":"agent/a","session_id":"session/a",
        "reference":"opaque","revision":"claim/a","start":0,"end":1,"max_response_bytes":1})).is_ok());
    assert!(validate_arguments("st_message_send", &send_arguments()).is_ok());
    assert!(validate_arguments("st_alert_answer", &json!({"action_id":"action/test","idempotency_key":"test-key",
        "fence":{"snapshot_id":"snapshot/test"},"alert_id":"attention/test","episode":"episode/test",
        "option":"done","summary":"Done"})).is_ok());
}

#[test]
fn validation_rejects_unknown_tools_nonobjects_extra_missing_and_wrong_types() {
    assert_eq!(validate_arguments("shell", &json!({})).unwrap_err()["code"], "unknown-tool");
    let negatives = [
        ("st_alerts_list", json!(null)),
        ("st_alerts_list", json!([])),
        ("st_alerts_list", json!({"command":"ignored?"})),
        ("st_alerts_list", json!({"limit":"1"})),
        ("st_alerts_list", json!({"limit":0})),
        ("st_alerts_list", json!({"limit":-1})),
        ("st_alerts_list", json!({"limit":1.5})),
        ("st_alerts_list", json!({"cursor":true})),
        ("st_alerts_list", json!({"max_response_bytes":0})),
        ("st_alerts_list", json!({"max_response_bytes":16385})),
        ("st_alert_get", json!({})),
        ("st_alert_get", json!({"alert_id":null})),
        ("st_alert_get", json!({"alert_id":3})),
        ("st_alert_answer", json!({"action_id":"a"})),
        ("st_conversation_read", json!({"agent_id":"a","session_id":"s","reference":"r","revision":"v","start":-1,"end":3})),
        ("st_conversation_read", json!({"agent_id":"a","session_id":"s","reference":"r","revision":"v","start":0,"end":"3"})),
    ];
    for (name, args) in negatives {
        assert_eq!(validate_arguments(name, &args).unwrap_err()["code"], "invalid-arguments", "{name}: {args}");
    }
}

#[test]
fn validation_rejects_open_or_untyped_nested_fences() {
    for fence in [json!(null), json!({}), json!({"snapshot_id":7}),
        json!({"snapshot_id":"s","shell":"ignored?"}),
        json!({"snapshot_id":"s","subject_revisions":[]}),
        json!({"snapshot_id":"s","subject_revisions":{"agent/a":42}})] {
        let mut args = send_arguments();
        args["fence"] = fence;
        assert_eq!(validate_arguments("st_message_send", &args).unwrap_err()["code"], "invalid-arguments");
    }
    for key in ["action_id", "idempotency_key", "to", "title", "content"] {
        let mut args = send_arguments();
        args[key] = json!(false);
        assert!(validate_arguments("st_message_send", &args).is_err(), "{key}");
        args.as_object_mut().unwrap().remove(key);
        assert!(validate_arguments("st_message_send", &args).is_err(), "missing {key}");
    }
}

#[tokio::test]
async fn invalid_calls_fail_before_contacting_gateway() {
    let error = call(&disconnected_adapter(), "st_message_send", &json!({"command":"no"})).await.unwrap_err();
    assert_eq!(error["code"], "invalid-arguments");
}

#[tokio::test]
async fn alert_answer_maps_to_work_done_and_returns_direct_action_result_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("client.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let expected = json!({
        "api_version":"st3.client.v0",
        "value":{"kind":"action-result", "action_id":"action/answer",
            "operation_id":"operation/client-answer", "status":"completed",
            "affected_ids":["step-run/test/ask"], "snapshot_id":"snapshot/after-answer"},
        "snapshot":{"id":"snapshot/after-answer"}
    });
    let response = expected.clone();
    let (sent, mut received) = tokio::sync::mpsc::channel(1);
    let gateway = Router::new().route("/v1/client/assistant/propose", axum::routing::post(
        move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
            sent.send((headers, body)).await.unwrap();
            axum::Json(response)
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, gateway).await.unwrap() });
    let adapter = Adapter {
        client: reqwest::Client::builder().unix_socket(socket).build().unwrap(),
        bearer: "test-only-answer-credential".into(),
    };
    let arguments = json!({
        "action_id":"action/answer", "idempotency_key":"answer-key",
        "fence":{"snapshot_id":"snapshot/before-answer"},
        "alert_id":"step-run/test/ask", "episode":"claim/ask", "option":"yes",
        "summary":"Proceed with the selected option"
    });
    let result = call(&adapter, "st_alert_answer", &arguments).await;
    server.abort();
    assert_eq!(result.unwrap(), expected);
    let (headers, body) = received.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer test-only-answer-credential");
    assert_eq!(body, json!({
        "api_version":"st3.client.v0", "id":"action/answer", "type":"work.done",
        "idempotency_key":"answer-key", "fence":{"snapshot_id":"snapshot/before-answer"},
        "parameters":{"target_id":"step-run/test/ask", "episode":"claim/ask",
            "summary":"Proceed with the selected option", "answer":{"id":"yes"}}
    }));
    assert!(expected["value"].get("approval_id").is_none());
}

#[tokio::test]
async fn http_router_rejects_get_mcp_and_unknown_paths_without_content() {
    let app = http_router(disconnected_adapter());
    for (method, path, status) in [
        ("GET", "/mcp", StatusCode::METHOD_NOT_ALLOWED),
        ("GET", "/unknown", StatusCode::NOT_FOUND),
        ("POST", "/unknown", StatusCode::NOT_FOUND),
    ] {
        let response = app.clone().oneshot(Request::builder().method(method).uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), status);
        assert!(to_bytes(response.into_body(), 1024).await.unwrap().is_empty());
    }
}

#[test]
fn credential_reader_accepts_only_bounded_owner_only_regular_token_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential");
    std::fs::write(&path, "test-token\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_credential(&path).unwrap(), "test-token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(read_credential(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(read_credential(&link).is_err());
    assert!(read_credential(dir.path()).is_err());
    for bad in ["", " \n", "two tokens", "token\ninjected", "nonascii-\u{e9}"] {
        std::fs::write(&path, bad).unwrap();
        assert!(read_credential(&path).is_err());
    }
    std::fs::write(&path, "a".repeat(4097)).unwrap();
    assert!(read_credential(&path).is_err());
    std::fs::write(&path, [0xff]).unwrap();
    assert!(read_credential(&path).is_err());
}
