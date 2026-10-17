//! Client tests that serve the real API on a socket; they live in st3-daemon because the API does.

#![allow(unused_imports)]

use std::time::Duration;

use crate::model::*;
use futures_util::{SinkExt as _, StreamExt as _};
use pty_client::{AttachParams, CURSOR_TO_BOTTOM, ClientIo, Reconnect, RouteRefusedError, TERMINAL_SANITIZE, attach};
use st3_store::client::*;
use axum::extract::ws::{Message as AxumWsMessage, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse as _;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio::io::AsyncReadExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::io::{Read as _, Seek as _};

fn test_api() -> Router {
    Router::new()
        .route(
            "/v1/test",
            get(|| async { Json(test_envelope(json!({"method": "get"}))) })
                .post(|Json(body): Json<Value>| async move { Json(test_envelope(body)) }),
        )
        .route(
            "/v1/person",
            get(|headers: HeaderMap| async move {
                Json(test_envelope(json!({
                    "person": headers
                        .get("x-st3-person")
                        .and_then(|value| value.to_str().ok())
                })))
            }),
        )
        .route(
            "/v1/client-test",
            get(|| async { Json(test_client_envelope(json!({"method": "client"}))) }),
        )
}

fn test_envelope(value: Value) -> ApiResponse<Value> {
    ApiResponse {
        api_version: "st3.v1".into(),
        request_id: "test-request".into(),
        snapshot_host: "test-node".into(),
        store_index: 1,
        value,
    }
}

fn test_client_envelope(value: Value) -> Value {
    json!({
        "api_version": "st3.client.v0",
        "request_id": "test-client-request",
        "snapshot": {
            "id": "snapshot/test-node/1/test",
            "host_id": "host/test-node",
            "store_index": 1,
            "projection_version": "client-projection.v0",
            "created_at": "2026-09-21T00:00:00.000Z"
        },
        "value": value
    })
}

fn fast_client(endpoint: Endpoint) -> Client {
    let deadlines = ClientDeadlines {
        connect: Duration::from_millis(20),
        request: Duration::from_millis(30),
        bulk: Duration::from_millis(40),
        event: Duration::from_millis(50),
        terminal_handshake: Duration::from_millis(30),
    };
    Client {
        endpoint,
        http: reqwest::Client::builder()
            .connect_timeout(deadlines.connect)
            .build()
            .unwrap(),
        deadlines,
        person: None,
        outage_wait: Duration::ZERO,
        announce_outage_wait: false,
        follow_retry: None,
        follow_test: None,
    }
}

#[tokio::test]
async fn message_retry_after_an_accepted_but_unanswered_send_creates_one_message() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let store = Arc::new(crate::store::Store::open_memory("message-retry").unwrap());
    let state = crate::api::AppState {
        owning_person: None,
        store: store.clone(),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: tokio::sync::watch::channel(0).0,
        node: "message-retry".into(),
        state_dir: directory.path().into(),
        pty_root: directory.path().join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = crate::api::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let count = count.clone();
            async move {
                let response = next.run(request).await;
                // Acceptance has committed. Lose only the first reply, as under load.
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                response
            }
        },
    ));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let request = MessageSendRequest {
        idempotency_key: "message-retry-accepted-001".into(),
        from: "person/ada".into(),
        to: "agent/example/worker".into(),
        content: "Hello once".into(),
        title: Some("A greeting".into()),
        in_reply_to: None,
        tags: vec!["example".into()],
        attachments: Vec::new(),
    };
    let mut client = fast_client(Endpoint::Unix(socket));
    client.deadlines.request = Duration::from_millis(200);
    let receipt = client.send_message(&request).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // The daemon says the retry found the message the first attempt sent.
    assert!(receipt.already_sent);
    assert_eq!(receipt.idempotency_key, request.idempotency_key);
    let message = receipt.message;
    let stored = store.messages(Some("agent/example/worker"), true).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(message.subject, stored[0].subject);
    assert_eq!(message.created_index, stored[0].created_index);
    // A later command retaining the key receives the same durable result as well.
    let replay = client.send_message(&request).await.unwrap();
    assert!(replay.already_sent);
    assert_eq!(replay.sent_at, receipt.sent_at);
    assert_eq!(replay.message.subject, message.subject);
    assert_eq!(replay.message.created_index, message.created_index);
    let found = client
        .sent_message(&request.idempotency_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.message.subject, message.subject);
    assert_eq!(found.sent_at, receipt.sent_at);
    assert!(client.sent_message("never-sent").await.unwrap().is_none());
    assert_eq!(store.messages(None, true).unwrap().len(), 1);
    // A reused key never silently changes the original message.
    let mut changed = request;
    changed.content = "A different message".into();
    let before = calls.load(Ordering::SeqCst);
    assert!(client.send_message(&changed).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(store.messages(None, true).unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn unanswered_message_retries_are_bounded_and_report_the_reusable_key() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(1)).await;
                Json(test_envelope(Value::Null))
            }
        }),
    );
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let error = fast_client(Endpoint::Unix(socket))
        .send_message(&MessageSendRequest {
            idempotency_key: "retry-this-key".into(),
            from: "person/ada".into(),
            to: "agent/example/worker".into(),
            content: "Hello".into(),
            title: None,
            in_reply_to: None,
            tags: vec![],
            attachments: Vec::new(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(error.contains("may have been sent"), "{error}");
    assert!(
        error.contains("--idempotency-key \"retry-this-key\""),
        "{error}"
    );
    assert!(error.contains("unconfirmed"), "{error}");
    server.abort();
}

#[tokio::test]
async fn follow_reads_wait_for_a_typed_connect_outage_with_daemon_wait_zero() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let client = fast_client(Endpoint::Unix(socket.clone())).with_follow_retry();
    let read = tokio::spawn(async move { client.get::<Value>("/v1/test").await });
    // Leave the socket absent through several backoff attempts, then bring it up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !read.is_finished(),
        "the follow read exited during the outage"
    );
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&socket, test_api()).await.unwrap();
    });
    let value = tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(value, json!({"method": "get"}));
    server.abort();
}

#[tokio::test]
async fn a_request_waits_out_a_daemon_restart() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let startup = Arc::new(crate::startup::Startup::begin(&socket).unwrap());
    startup.phase("open-store");
    let serving = Arc::clone(&startup);
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = std::fs::remove_file(&server_socket);
        crate::api::serve_unix_with_ready(&server_socket, test_api(), move || {
            serving.serving()
        })
        .await
        .unwrap();
    });
    let client = Client::unix(&socket).with_outage_wait(Duration::from_secs(10), true);
    let post: Value = client
        .post("/v1/test", &json!({"method": "post"}))
        .await
        .unwrap();
    assert_eq!(post, json!({"method": "post"}));
    server.abort();
}

#[tokio::test]
async fn a_first_page_get_retries_snapshot_churn_without_reusing_a_cursor() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let attempts = Arc::new(AtomicUsize::new(0));
    let route_attempts = attempts.clone();
    let app = Router::new().route(
        "/v1/client/test",
        get(move || {
            let route_attempts = route_attempts.clone();
            async move {
                if route_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    (
                        axum::http::StatusCode::GONE,
                        Json(json!({
                            "api_version": "st3.client.v0",
                            "error_version": "st3.client.error.v0",
                            "request_id": "request/expired",
                            "code": "page-cursor-expired",
                            "message": "the snapshot changed; restart pagination from the first page",
                            "retryable": true,
                            "details": {}
                        })),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        Json(test_client_envelope(json!({"items": []}))),
                    )
                }
            }
        }),
    );
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let value: Value = Client::unix(&socket).get("/v1/client/test").await.unwrap();
    assert_eq!(value, json!({"items": []}));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    server.abort();
}

async fn assert_request_transports(client: Client) {
    let get: Value = client.get("/v1/test").await.unwrap();
    assert_eq!(get, json!({"method": "get"}));
    let post: Value = client
        .post("/v1/test", &json!({"method": "post"}))
        .await
        .unwrap();
    assert_eq!(post, json!({"method": "post"}));
    let client_value: Value = client.get("/v1/client-test").await.unwrap();
    assert_eq!(client_value, json!({"method": "client"}));
}

#[tokio::test]
async fn the_unix_transport_completes_real_get_and_post_requests() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, test_api())
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(socket.exists(), "the Unix API socket did not start");
    assert_request_transports(Client::unix(&socket)).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_unix_posts_to_handlers_that_ignore_the_body_are_answered_once() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let app = Router::new().route(
        "/v1/test",
        get(|| async { Json(test_envelope(json!({}))) }).post(move || {
            let calls = handler_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Json(test_envelope(json!({"answered": true})))
            }
        }),
    );
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    let client = Client::unix(&socket).with_outage_wait(Duration::ZERO, false);
    for _ in 0..100 {
        if client.get::<Value>("/v1/test").await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // Legacy preview handlers intentionally ignore their small JSON request body.
    // They may answer and close as soon as Hyper parses the request headers.
    for request in 0..10_000 {
        let answer: Value = client
            .post("/v1/test", &json!({"request": request}))
            .await
            .unwrap_or_else(|error| panic!("request {request}: {error:#}"));
        assert_eq!(answer, json!({"answered": true}));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 10_000);
    server.abort();
}

#[tokio::test]
async fn only_unix_as_carries_explicit_person_authority() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, test_api())
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let ordinary: Value = Client::unix(&socket).get("/v1/person").await.unwrap();
    assert!(ordinary["person"].is_null());
    let named: Value = Client::unix_as(&socket, "person/alex")
        .unwrap()
        .get("/v1/person")
        .await
        .unwrap();
    assert_eq!(named["person"], "person/alex");
    assert!(Client::unix_as(&socket, "person/alex\r\nx-forged: yes").is_err());
    let agent: Value = Client::unix_agent(&socket, "agent/example/bridge")
        .unwrap().get("/v1/person").await.unwrap();
    assert_eq!(agent["person"], "agent/example/bridge");
    assert!(Client::unix_agent(&socket, "person/alex").is_err());
    assert!(Client::unix_agent(&socket, "agent/example\r\nx-forged: yes").is_err());
    assert!(Client::unix_agent(&socket, "agent/").is_err());
    server.abort();
}

#[tokio::test]
async fn the_http_transport_completes_real_get_and_post_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, test_api()).await.unwrap();
    });
    assert_request_transports(Client::new(Endpoint::Http(format!("http://{address}")))).await;
    server.abort();
}

#[allow(clippy::result_large_err)]
#[tokio::test]
async fn a_terminal_attachment_reconnects_across_a_temporary_gateway_restart() {
    use std::fs::File;
    use std::io::Read as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let attach_attempts = Arc::new(AtomicUsize::new(0));
    let stream_attempts = Arc::new(AtomicUsize::new(0));
    let route_attach_attempts = attach_attempts.clone();
    let route_stream_attempts = stream_attempts.clone();
    let app = Router::new()
        .route(
            "/v1/sessions/attach/{*subject}",
            post(move || {
                let attempt = route_attach_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "code": "gateway-restarting",
                                "message": "the st gateway is restarting",
                                "details": {}
                            })),
                        )
                            .into_response();
                    }
                    (
                        StatusCode::OK,
                        Json(test_envelope(json!({
                            "subject": "agent/test",
                            "runtime_id": "demo",
                            "incarnation_id": "incarnation/one",
                            "capability": "fresh-capability",
                            "websocket_path": "/terminal",
                            "expires_at_unix_ms": 9999999999999_u64
                        }))),
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/terminal",
            get(move |websocket: WebSocketUpgrade| {
                let attempt = route_stream_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    websocket.protocols(["st3.terminal.v1"]).on_upgrade(
                        move |mut socket| async move {
                            if attempt == 0 {
                                let _ = socket.close().await;
                                return;
                            }
                            let _ = socket
                                .send(AxumWsMessage::Binary(
                                    pty_core::protocol::encode_screen(b"resumed after restart")
                                        .into(),
                                ))
                                .await;
                            let _ = socket
                                .send(AxumWsMessage::Binary(
                                    pty_core::protocol::encode_exit(0).into(),
                                ))
                                .await;
                            // Keep the successful route alive until the attach client consumes
                            // the terminal exit and closes its bridge. Dropping the server side
                            // immediately races the exit frame and can induce a valid third
                            // reconnect before the fixture's result is observed.
                            let _ = socket.recv().await;
                        },
                    )
                }
            }),
        );
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let mut master_fd = -1;
    let mut slave_fd = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let attachment = Attachment {
        subject: "agent/test".into(),
        runtime_id: "demo".into(),
        incarnation_id: Some("incarnation/one".into()),
        capability: "initial-capability".into(),
        websocket_path: "/terminal".into(),
        expires_at_unix_ms: u128::MAX,
    };
    let io = ClientIo {
        stdin: slave.as_raw_fd(),
        stdout: slave.as_raw_fd(),
        stderr: slave.as_raw_fd(),
    };

    // Drain the PTY while the client runs. On macOS, restoring termios can wait for
    // pending output to drain; reading only after the client returns deadlocks it.
    let output_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        if let Err(error) = master.read_to_end(&mut output) {
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
        }
        output
    });
    let exit = Client::unix(&socket)
        .proxy_terminal_resilient_with_io("agent/test", &attachment, io)
        .await
        .unwrap();
    assert_eq!(exit, 0);
    // Under parallel test load, opening the first resumed stream can itself fail
    // transiently. The bridge must retry with another one-use capability rather
    // than promising an exact number of HTTP attaches.
    assert!((2..=4).contains(&attach_attempts.load(Ordering::SeqCst)));
    assert!((2..=4).contains(&stream_attempts.load(Ordering::SeqCst)));

    drop(slave);
    let output = output_reader.join().unwrap();
    assert!(
        output
            .windows(b"resumed after restart".len())
            .any(|window| window == b"resumed after restart"),
        "the resumed terminal screen was not rendered: {output:?}"
    );
    assert!(
        output
            .windows(b"[reconnecting".len())
            .any(|window| window == b"[reconnecting"),
        "the reconnect state was not rendered: {output:?}"
    );
    server.abort();
}

#[tokio::test]
async fn only_a_local_daemon_that_knows_the_route_names_a_local_terminal() {
    let remote = Client::new(Endpoint::Http("http://127.0.0.1:9".into()));
    assert!(
        remote
            .local_terminal("agent/worker")
            .await
            .unwrap()
            .is_none(),
        "an HTTP endpoint can be another host, so its terminals use the WebSocket"
    );

    let directory = tempfile::tempdir().unwrap();
    let older = directory.path().join("older.sock");
    let server_socket = older.clone();
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, Router::new())
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if older.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        Client::unix(&older)
            .local_terminal("agent/worker")
            .await
            .unwrap()
            .is_none(),
        "a daemon without the route leaves the attach to the WebSocket"
    );
    server.abort();

    let elsewhere = directory.path().join("elsewhere.sock");
    let server_socket = elsewhere.clone();
    let app = Router::new().route(
        "/v1/sessions/local-terminal/{*subject}",
        get(|| async {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "code": "runtime-not-local",
                    "message": "subject `agent/worker` is owned by `host/other`",
                    "details": {}
                })),
            )
        }),
    );
    let server = tokio::spawn(async move {
        crate::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if elsewhere.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let error = Client::unix(&elsewhere)
        .local_terminal("agent/worker")
        .await
        .unwrap_err();
    assert_eq!(api_error_code(&error), Some("runtime-not-local"));
    server.abort();
}
