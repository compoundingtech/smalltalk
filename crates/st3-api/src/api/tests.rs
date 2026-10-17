mod roster_fresh;

use super::*;
use crate::model::SESSION_CLAIMS;
use axum::body::to_bytes;
use axum::http::Request;
use std::path::PathBuf;

#[tokio::test]
async fn child_session_detail_resolves_readable_transcript_outside_session_inventory() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("native");
    let transcript = home.join(".omp/agent/sessions/example/native-test.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &transcript,
        concat!(
            "{\"type\":\"session\",\"id\":\"native-test\",\"cwd\":\"/work/example\",\"timestamp\":\"2026-10-06T12:00:00Z\"}\n",
            "{\"type\":\"message\",\"id\":\"parent-message\",\"timestamp\":\"2026-10-06T12:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"parent only\"}]}}\n",
        ),
    )
    .unwrap();
    let child_transcript = transcript.with_extension("").join("task-1.jsonl");
    std::fs::create_dir_all(child_transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &child_transcript,
        concat!(
            "{\"type\":\"session\",\"id\":\"child\",\"cwd\":\"/work/example\",\"timestamp\":\"2026-10-06T12:01:00Z\"}\n",
            "{\"type\":\"message\",\"id\":\"child-message\",\"timestamp\":\"2026-10-06T12:01:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"child only\"}]}}\n",
        ),
    )
    .unwrap();
    let parent = crate::external_sessions::discover(Some(&home), true)
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.native_id == "native-test")
        .unwrap();
    let child_id = crate::subagent_sessions::child_session_id(&parent, "task-1");
    let mut state = state(root.path());
    state.native_session_home = Some(home);
    let app = router(state);
    let path = format!("/v1/client/sessions/{child_id}");
    let (status, detail) = get_request(app.clone(), &path).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["id"], child_id);
    assert_eq!(detail["kind"], "session");
    assert_eq!(detail["managed"], false);
    assert_eq!(detail["driver"], "omp");
    assert_eq!(detail["native_session_id"], "native-test/task-1");
    assert_eq!(detail["workspace"], "/work/example");
    assert_eq!(detail["title"], "native-test · task-1");
    assert_eq!(detail["owner_id"], "external-session/omp/native-test/task-1");
    assert_eq!(detail["state"], "completed");
    assert_eq!(detail["updated_at"], detail["ended_at"]);
    assert!(detail["started_at"].as_str().is_some());
    assert!(detail["updated_at"].as_str().is_some());
    let (status, timeline) = get_request(app.clone(), &format!("{path}/timeline")).await;
    assert_eq!(status, StatusCode::OK, "{timeline}");
    assert_eq!(timeline["session_id"], child_id);
    assert!(
        timeline["items"].as_array().unwrap().iter().any(|item| {
            item["body"]["text"] == "child only"
        }),
        "{timeline}"
    );
    std::fs::remove_file(child_transcript).unwrap();
    let (status, missing) = get_request(app, &path).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
}

#[tokio::test]
async fn resources_page_keeps_rows_when_sync_forecast_is_unrepresentable() {
    use smallclaims::replication::MAX_SAFE_DURATION_SECONDS;
    use smallclaims::store::PeerSyncProgress;

    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.configured_peers = vec!["birch".into()];
    for row in 0..9 {
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("resource/example/{row}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), json!("vcs.pull-request")),
                    (
                        "facts".into(),
                        json!({"opened_by": "agent/example", "number": row}),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let now = client_now_ms();
    let mut progress = PeerSyncProgress::default();
    progress.observe(0, Some((10_000, 0)), now - 610_000);
    progress.observe(1_000, Some((9_000, 0)), now - 600_000);
    for window in 1..=60 {
        progress.observe(0, Some((9_000, 0)), now - 600_000 + window * 10_000);
    }
    assert_eq!(progress.view(now).unwrap().estimated_catch_up_seconds, None);
    state
        .store
        .replication_sync
        .lock()
        .unwrap()
        .insert("birch".into(), progress);

    // An actual HTTP request over an isolated daemon's Unix socket, with the production
    // router and resource projection. No fleet state or peer listener is involved.
    let socket = root.path().join("api.sock");
    let server_socket = socket.clone();
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        serve_unix(&server_socket, router(server_state))
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder()
        .unix_socket(socket.clone())
        .build()
        .unwrap();
    let url = "http://localhost/v1/client/resources?opened_by=agent/example&limit=50";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "isolated API did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let response = client.get(url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: Value = response.json().await.unwrap();
    let page = &envelope["value"];
    assert_eq!(page["items"].as_array().unwrap().len(), 9);
    assert_eq!(page["sync"]["state"], "catching-up");
    assert_eq!(page["sync"]["peers"][0]["peer_only_envelopes"], 9_000);
    assert_eq!(
        page["sync"]["peers"][0]["estimated_catch_up_seconds"],
        Value::Null
    );

    // Guard the API boundary too: a pre-existing measurement can contain an unsafe value.
    for (forecast, expected) in [
        (Some(90), json!(90)),
        (
            Some(MAX_SAFE_DURATION_SECONDS),
            json!(MAX_SAFE_DURATION_SECONDS),
        ),
        (Some(MAX_SAFE_DURATION_SECONDS + 1), Value::Null),
        (Some(u64::MAX), Value::Null),
        (None, Value::Null),
    ] {
        state
            .store
            .replication_sync
            .lock()
            .unwrap()
            .get_mut("birch")
            .unwrap()
            .measured
            .as_mut()
            .unwrap()
            .estimated_catch_up_seconds = forecast;
        let response = client.get(url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let envelope: Value = response.json().await.unwrap();
        assert_eq!(envelope["value"]["items"], page["items"]);
        assert_eq!(envelope["value"]["sync"]["state"], "catching-up");
        assert_eq!(
            envelope["value"]["sync"]["peers"][0]["estimated_catch_up_seconds"],
            expected
        );
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_seat_reads_its_own_desired_record_for_its_status_line() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state
        .store
        .apply_internal(
            &parse_intent(
                r#"version 2
agent "eval/named" { name "Quartz"; workspace "/tmp"; harness "claude" {} }
"#,
                "node",
            )
            .unwrap(),
            "desired-fixture",
        )
        .unwrap();
    let app = router(state);
    let (status, seat) = get_request(app.clone(), "/v1/desired/agent/eval/named").await;
    assert_eq!(status, StatusCode::OK, "{seat}");
    let seat: crate::model::DesiredSubject = serde_json::from_value(seat).unwrap();
    assert_eq!(
        crate::mailbox::seat_label(&seat, Some("gen")),
        "Quartz[gen]"
    );
    let (status, missing) = get_request(app, "/v1/desired/agent/eval/absent").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
}

#[tokio::test]
async fn delivery_hold_api_enforces_authority_and_keeps_presence_separate() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state
        .store
        .apply_internal(
            &parse_intent(
                r#"version 2
agent "eval/held" { workspace "/tmp"; harness "codex" {} }
agent "eval/channel" { workspace "/tmp"; harness "claude" {} }
"#,
                "node",
            )
            .unwrap(),
            "hold-fixture",
        )
        .unwrap();
    let store = state.store.clone();
    let app = router(state);
    let now = client_now_ms() as u64;
    let request = json!({"subject":"agent/eval/held", "actor":"person/alex", "held":true,
        "until_unix_ms": now + 60_000, "reason":"quiet interval", "idempotency_key":"hold-api"});
    let (status, claim) = json_request(app.clone(), "/v1/delivery/hold", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    let (status, replay) =
        json_request(app.clone(), "/v1/delivery/hold", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(claim["id"], replay["id"]);
    let (_, hold) =
        get_request(app.clone(), "/v1/delivery/hold?subject=agent%2Feval%2Fheld").await;
    assert_eq!(hold["active"], true);
    assert!(
        store
            .latest_claim("agent/eval/held", Some("agent.presence"))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_claim("agent/eval/held", Some("harness.observed"))
            .unwrap()
            .is_none()
    );
    let mut foreign = request.clone();
    foreign["actor"] = json!("agent/eval/other");
    let (status, error) = json_request(app.clone(), "/v1/delivery/hold", foreign).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["code"], "delivery-hold-forbidden");
    let mut unsupported = request.clone();
    unsupported["subject"] = json!("agent/eval/channel");
    let (_, error) = json_request(app.clone(), "/v1/delivery/hold", unsupported).await;
    assert_eq!(error["code"], "unsupported-delivery-hold");
    let mut release = request;
    release["held"] = json!(false);
    release["until_unix_ms"] = json!(0);
    release["actor"] = json!("agent/eval/held");
    release["idempotency_key"] = json!("hold-release");
    let (status, result) = json_request(app.clone(), "/v1/delivery/hold", release).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (_, hold) = get_request(app, "/v1/delivery/hold?subject=agent%2Feval%2Fheld").await;
    assert_eq!(hold["active"], false);
}

#[tokio::test]
async fn history_pages_and_detail_do_not_cache_the_whole_claim_log() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for number in 0..8 {
        state
            .store
            .append_claim(&ClaimInput {
                subject: "daemon/node".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("code".into(), Value::String("history-test".into())),
                    ("severity".into(), Value::String("error".into())),
                    ("reason".into(), Value::String(number.to_string())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let snapshot = new_client_snapshot(&state);
    let first = client_history(
        State(state.clone()),
        Extension(snapshot.clone()),
        Query(ClientListQuery {
            limit: Some(1),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(first.items.len(), 1);
    let first_index = first.items[0]["store_index"].as_u64().unwrap();
    let cursor = first.page.next_cursor.unwrap();
    assert!(
        !client_page_cache()
            .lock()
            .unwrap()
            .iter()
            .any(|entry| { entry.snapshot_id == snapshot.id && entry.collection == "history" }),
        "a history page must not retain every claim in the process cache"
    );
    let second = client_history(
        State(state.clone()),
        Extension(snapshot),
        Query(ClientListQuery {
            limit: Some(1),
            cursor: Some(cursor),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(second.items.len(), 1);
    assert!(second.items[0]["store_index"].as_u64().unwrap() < first_index);
    let detail = client_history_detail(State(state), AxumPath(first_index.to_string()))
        .await
        .unwrap()
        .0;
    assert_eq!(detail["store_index"], first_index);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bound_actor_refusals_decode_as_client_api_errors() {
    fn own_seat(_pid: u32) -> Option<String> {
        Some("agent/own".into())
    }
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("api.sock");
    let server_socket = socket.clone();
    // Reaching the handler would succeed. The listener must reject the foreign actor first.
    let app = Router::new().route(
        "/v1/client/glasses/{id}",
        axum::routing::put(|| async { Json(json!({})) }),
    );
    let server = tokio::spawn(async move {
        serve_unix_with_ancestor(&server_socket, None, app, true, own_seat).await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let error = st3_client::Client::unix_as(&socket, "person/alex")
        .put_glass(
            "0194b2e0-1234-7000-8000-000000000001",
            &st3_client::GlassPut {
                body: st3_client::GlassBody {
                    name: "Main".into(),
                    layout: st3_client::GlassLayout::Group { tabs: vec![] },
                },
                base_revision: None,
            },
            "glass-foreign-actor-test-key",
        )
        .await
        .unwrap_err();
    server.abort();
    let st3_client::ClientError::Api(code, message, envelope) = error else {
        panic!("an actor refusal must be a typed client API error: {error}");
    };
    assert_eq!(code, st3_client::ErrorCode::Forbidden);
    assert!(message.contains("cannot act as"));
    let value = serde_json::to_value(envelope).unwrap();
    assert_eq!(value["api_version"], CLIENT_API_VERSION);
    assert_eq!(value["error_version"], "st3.client.error.v0");
    assert!(
        value["request_id"]
            .as_str()
            .unwrap()
            .starts_with("request/")
    );
    assert_eq!(value["retryable"], false);
    assert!(value["details"].is_object());
}

#[tokio::test]
async fn a_bound_harness_cannot_act_as_another_actor() {
    for path in [
        "/v1/agent-queue-moves",
        "/v1/agents/rename",
        "/v1/agents/restart",
        "/v1/agents/rollout",
        "/v1/agents/start",
        "/v1/agents/suspend",
        "/v1/agents/resume",
        "/v1/agents/native-session",
        "/v1/work/revision/approve/proposal",
        "/v1/work/delegation",
        "/v1/messages/example/claims",
        "/v1/mission-runs/example%2Fdemo%2F1/outcome",
        "/v1/mission-runs/example%2Fdemo%2F1/revision",
        "/v1/missions/example%2Fdemo/retire",
        "/v1/subscription-requests/release/subscription-request%2Fexample",
        "/v1/reviews/step-run/example",
        "/v1/schema/registrations",
        "/v1/custom/reply",
    ] {
        for actor in ["agent/peer", "person/operator"] {
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .body(Body::from(
                    json!({"actor": actor, "agent": "agent/worker"}).to_string(),
                ))
                .unwrap();
            let error = match guard_bound_request(request, Some("agent/own")).await {
                Ok(_) => panic!("bound harness acted as {actor} at {path}"),
                Err(error) => error,
            };
            assert_eq!(error.code, "foreign-agent-actor");
        }
    }
    // Free mode keeps the binding for client-v0 too: a harness names only its own seat.
    for person in ["person/operator", "agent/peer"] {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/client/actions")
            .header(client_v0::LOCAL_PERSON_HEADER, person)
            .body(Body::from("{}"))
            .unwrap();
        let error = guard_bound_request(request, Some("agent/own"))
            .await
            .expect_err("a bound harness acted as another client-v0 actor");
        assert_eq!(error.code, "foreign-agent-actor", "{person}");
    }
    let request = Request::builder()
        .method("POST")
        .uri("/v1/client/actions")
        .header(client_v0::LOCAL_PERSON_HEADER, "agent/own")
        .body(Body::from("{}"))
        .unwrap();
    assert!(
        guard_bound_request(request, Some("agent/own"))
            .await
            .is_ok()
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/agent-queue-moves")
        .body(Body::from(json!({"actor": "agent/own"}).to_string()))
        .unwrap();
    assert!(
        guard_bound_request(request, Some("agent/own"))
            .await
            .is_ok()
    );
}

/// #901: a seat suspends only once its driver reports it quiet and it has a native session
/// to resume; a suspension holds the seat against restarts until a resume.
#[tokio::test]
async fn suspend_waits_for_a_quiet_seat_with_a_native_session() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let source = format!(
        "version 2\nagent \"test/seat\" {{\n host \"node\"\n workspace {:?}\n harness \"claude\" {{}}\n}}\n",
        root.path().display().to_string()
    );
    let request = apply_request(&state, &source, "person/test", "declare");
    let (status, body) = json_request(
        app.clone(),
        "/v1/intent/apply",
        serde_json::to_value(request).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let subject = "agent/test/seat";
    let incarnation = "4242:2026-10-02T20:00:00.000Z";
    let append = |kind: &str, fields: Value| {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(subject.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    let ask = |path: &'static str, key: &str| {
        json_request(
            app.clone(),
            path,
            json!({"subject": subject, "actor": "person/test", "idempotency_key": key}),
        )
    };
    let (status, body) = ask("/v1/agents/suspend", "stopped").await;
    assert_eq!(body["code"], "suspend-not-running", "{status} {body}");
    append(
        "runtime.observed",
        json!({"status": "running", "runtime_id": "seat", "incarnation_id": incarnation}),
    );
    append(
        "harness.observed",
        json!({"state": "working", "incarnation_id": incarnation, "quiescent": false,
               "blocking": ["turn-in-flight"]}),
    );
    let (status, body) = ask("/v1/agents/suspend", "busy").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "suspend-blocked");
    assert_eq!(
        body["details"]["blocking"],
        json!(["native-session-unbound", "turn-in-flight"])
    );
    // Only the seat itself reports its native session.
    let report = |actor: &str| {
        json!({"subject": subject, "actor": actor, "incarnation_id": incarnation,
               "harness": "claude", "session_id": "native-one", "account_ref": "ada/one"})
    };
    let (_, body) = json_request(
        app.clone(),
        "/v1/agents/native-session",
        report("agent/other"),
    )
    .await;
    assert_eq!(body["code"], "foreign-agent-actor");
    let (status, body) =
        json_request(app.clone(), "/v1/agents/native-session", report(subject)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["body"]["fields"]["account_ref"], "ada/one");
    append(
        "harness.observed",
        json!({"state": "idle", "incarnation_id": incarnation, "quiescent": true,
               "blocking": []}),
    );
    let (_, body) = ask("/v1/agents/resume", "early").await;
    assert_eq!(body["code"], "not-suspended");
    let (status, body) = ask("/v1/agents/suspend", "quiet").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["body"]["fields"]["action"], "suspend");
    let suspension = crate::suspension::current(&state.store, subject)
        .unwrap()
        .unwrap();
    assert_eq!(suspension.phase, "quiescing");
    assert!(suspension.holds_seat());
    let (_, body) = ask("/v1/agents/suspend", "again").await;
    assert_eq!(body["code"], "already-suspended");
    let (_, body) = ask("/v1/agents/restart", "restart").await;
    assert_eq!(body["code"], "restart-suspended");
}

#[test]
fn doctor_flags_low_descriptor_limits_and_high_usage() {
    let low = descriptor_usage_check(256, 8192, Some(92));
    assert_eq!(low.status, "warn");
    assert!(
        low.message
            .contains("92 open; soft limit 256, hard limit 8192")
    );
    assert!(low.message.contains("low soft limit"));
    assert_eq!(
        descriptor_usage_check(8192, 8192, Some(7000)).status,
        "warn"
    );
    assert_eq!(descriptor_usage_check(8192, 8192, Some(92)).status, "pass");
    assert_eq!(descriptor_usage_check(8192, 8192, None).status, "warn");
}

#[test]
fn doctor_reader_memory_reports_targets_and_warns_on_limits_and_pressure() {
    let usage = smallclaims::sqlite::ReaderUsage {
        open: 97,
        idle: 90,
        peak: 100,
        opened: 100,
    };
    let groups = |max_bytes, max_events| {
        Ok(Some(vec![crate::memory::GroupMemory {
            path: "/invented/service".into(),
            max_bytes,
            max_events,
        }]))
    };
    let normal = reader_memory_check(usage, 2048, 128, 32, groups(Some(1 << 30), 0));
    assert_eq!(normal.status, "pass");
    assert!(
        normal
            .message
            .contains("97 open readers (90 idle, 7 active)")
    );
    assert!(normal.message.contains("current reader targets 194 MiB"));
    assert!(normal.message.contains("planning envelope 800 MiB"));
    assert!(normal.message.contains("API read-worker admission 32"));
    assert!(normal.message.contains("Raw/background reader checkout is uncapped"));
    assert_eq!(
        reader_memory_check(usage, 8192, 128, 32, groups(Some(1 << 30), 0)).status,
        "warn"
    );
    let pressure = reader_memory_check(usage, 2048, 128, 32, groups(Some(1 << 30), 1225));
    assert_eq!(pressure.status, "warn");
    assert!(pressure.message.contains("memory.events:max 1225"));
    assert_eq!(
        reader_memory_check(usage, 2048, 128, 32, groups(None, 0)).status,
        "pass"
    );
    assert_eq!(
        reader_memory_check(usage, 2048, 128, 32, Err(anyhow::anyhow!("unavailable"))).status,
        "warn"
    );
    let burst = smallclaims::sqlite::ReaderUsage {
        open: 300,
        idle: 0,
        peak: 300,
        opened: 300,
    };
    assert_eq!(
        reader_memory_check(burst, 2048, 128, 32, groups(Some(1 << 30), 0)).status,
        "warn"
    );
}

#[test]
fn descriptor_exhaustion_bounds_accept_retries_and_log_reports() {
    for code in [libc::EMFILE, libc::ENFILE] {
        let error = std::io::Error::from_raw_os_error(code);
        let mut failures = AcceptFailures::default();
        let start = std::time::Instant::now();
        let mut now = start;
        let mut delays = Vec::new();
        let mut reports = Vec::new();
        while now.duration_since(start) < Duration::from_secs(60) {
            let (delay, count) = failures.failed(&error, now);
            assert!(delay <= Duration::from_secs(2));
            delays.push(delay.as_millis());
            if let Some(count) = count {
                reports.push((now, count));
            }
            now += delay;
        }
        assert_eq!(&delays[..7], &[100, 200, 400, 800, 1600, 2000, 2000]);
        assert!(delays.len() <= 35, "retries: {}", delays.len());
        assert!(reports.len() <= 12, "reports: {}", reports.len());
        assert!(
            reports
                .windows(2)
                .all(|pair| pair[1].0.duration_since(pair[0].0) >= Duration::from_secs(5))
        );
        assert_eq!(reports[0].1, 1);
        assert!(reports.iter().any(|(_, count)| *count > 1));
        failures.resource_failures = 0;
        let (delay, report) = failures.failed(&error, now);
        assert_eq!(delay, Duration::from_millis(100));
        failures.resource_failures = 0;
        let (_, immediate_report) = failures.failed(&error, now);
        if report.is_some() {
            assert!(immediate_report.is_none());
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_unix_peer_is_bound_to_its_harness_environment() {
    let mut child = std::process::Command::new("sleep")
        .arg("10")
        .env("ST_AGENT", "agent/fixture/worker")
        .spawn()
        .unwrap();
    let mut bound = None;
    for _ in 0..50 {
        bound = harness_ancestor(child.id());
        if bound.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(bound.as_deref(), Some("agent/fixture/worker"));
    child.kill().unwrap();
    child.wait().unwrap();
}

#[tokio::test]
async fn the_run_start_api_refuses_placeholder_requesters() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for requester in [None, Some("person/requester".to_owned())] {
        let request = MissionRunRequest {
            mission: "demo".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester,
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "placeholder-run".into(),
        };
        let error = match start_mission_run_action(State(state.clone()), Json(request)).await {
            Ok(_) => panic!("placeholder requester created a run"),
            Err(error) => error,
        };
        assert!(matches!(
            error.code.as_str(),
            "missing-run-requester" | "placeholder-run-requester"
        ));
    }
}

#[test]
fn owner_cursor_expiry_remains_a_typed_retryable_gateway_error() {
    let rejected = crate::peer::ClientReadRejected::new(
        "page-cursor-expired",
        StatusCode::GONE,
        "the owner page expired",
    );
    let error = remote_read_error("host/owner", rejected.into());
    assert_eq!(error.status, StatusCode::GONE);
    assert_eq!(error.code, "page-cursor-expired");
    let rejected = crate::peer::ClientReadRejected::new(
        "validation-failed",
        StatusCode::UNPROCESSABLE_ENTITY,
        "remote terminal input is invalid",
    );
    let terminal_error = remote_read_error("host/owner", rejected.into());
    assert_eq!(terminal_error.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(terminal_error.code, "validation-failed");
    let unavailable = remote_read_error("host/owner", anyhow::anyhow!("transport down"));
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unavailable.code, "remote-unavailable");
    assert_eq!(unavailable.details["reason"], "transport-error");
    assert_eq!(unavailable.details["owner_host_id"], "host/owner");
}

#[test]
fn an_owner_read_deadline_keeps_its_typed_gateway_timeout() {
    let rejected = crate::peer::ClientReadRejected::new(
        "read-deadline",
        StatusCode::GATEWAY_TIMEOUT,
        "the owner read exceeded its budget",
    );
    let error = remote_read_error("host/owner", rejected.into());
    assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error.code, "read-deadline");
    assert_eq!(error.details["owner_host_id"], "host/owner");
}

#[test]
fn an_unreachable_owner_says_why_and_how_far_the_read_got() {
    let mut rejected = crate::peer::ClientReadRejected::unreachable(
        "dial-failed",
        "owner host/owner did not answer from gateway (dial-failed after 1 attempt(s))",
    );
    rejected.details.insert(
        "attempts".into(),
        json!([{"via": "host/relay", "reason": "dial-failed", "next": [
            {"via": "host/owner", "reason": "dial-failed"}
        ]}]),
    );
    rejected.details.insert("elapsed_ms".into(), 7.into());
    let error = remote_read_error("host/owner", rejected.into());
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.code, "remote-unavailable");
    assert!(!error.message.contains("temporarily"), "{}", error.message);
    assert_eq!(error.details["reason"], "dial-failed");
    assert_eq!(error.details["hops"], 2);
    assert_eq!(error.details["elapsed_ms"], 7);
    assert_eq!(error.details["owner_host_id"], "host/owner");

    let none = remote_unavailable("host/owner");
    assert_eq!(none.code, "remote-unavailable");
    assert_eq!(none.details["reason"], "no-route");
    assert_eq!(none.details["hops"], 0);
    assert!(none.message.starts_with("no route to owner host/owner"));
}

#[test]
fn remote_timeline_cursor_keeps_its_owner_snapshot_out_of_gateway_admission() {
    let root = tempfile::tempdir().unwrap();
    let gateway = state(root.path());
    let local = new_client_snapshot(&gateway);
    let mut owner = local.clone();
    owner.id = "snapshot/owner/9000/ahead".into();
    owner.host_id = "host/owner".into();
    owner.store_index = 9000;

    let admitted = client_request_snapshot(&gateway, Some(owner));
    assert_eq!(admitted.id, local.id);
    assert_eq!(admitted.store_index, local.store_index);
    let admitted_local = client_request_snapshot(&gateway, Some(local.clone()));
    assert_eq!(admitted_local.id, local.id);
    assert_eq!(admitted_local.store_index, local.store_index);
}

#[tokio::test]
async fn slow_request_does_not_change_the_graph() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let before = state.store.index().unwrap();
    record_request_latency(
        &axum::http::Method::GET,
        "/v1/client/agents",
        "/v1/client/agents",
        None,
        "stui",
        Instant::now() - Duration::from_secs(2),
        Served::default(),
    );
    // The old implementation spawned a blocking write, so give that write
    // time to finish before proving the request caused no graph change.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state.store.index().unwrap(), before);
    assert!(
        state
            .store
            .latest_claim("daemon/node", Some("daemon.diagnostic"))
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn busy_agent_projection_does_not_block_other_async_requests() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let snapshot = new_client_snapshot(&state);
    let store = state.store.clone();
    let (ready_send, ready_recv) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        store.hold_read_connections_for_test(|| {
            ready_send.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
    });
    ready_recv.recv().unwrap();
    let handler = tokio::spawn(client_agents(
        State(state),
        Extension(snapshot),
        Query(ClientListQuery::default()),
    ));
    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(started.elapsed() < Duration::from_millis(150));
    holder.join().unwrap();
    let _ = handler.await.unwrap().unwrap();
}

#[test]
fn repeated_agent_list_reads_only_local_frontier_without_rebuilding() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let index = state.store.index().unwrap();
    client_agent_resources(&state.store, false, "first", index).unwrap();
    let index = state
        .store
        .append_claim(&ClaimInput {
            subject: "daemon/node".into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: BTreeMap::from([
                ("severity".into(), Value::String("error".into())),
                ("code".into(), Value::String("slow-request".into())),
                (
                    "reason".into(),
                    Value::String("a request exceeded one second".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
        .store_index;
    client_agent_resources(&state.store, false, "after diagnostic", index).unwrap();
    // The former zero-reader cache hit missed same-index local activity. Correct warm
    // reads now perform one indexed frontier seek, but never reduce cards again.
    let builds = state.store.agent_resources_builds_for_test();
    let work = smallclaims::sqlite::work::total();
    let rows = client_agent_resources(&state.store, false, "second", index).unwrap();
    assert!(rows.is_empty());
    assert_eq!(state.store.agent_resources_builds_for_test(), builds);
    assert!((smallclaims::sqlite::work::total() - work).vm_steps < 100);
}

#[tokio::test]
async fn agent_detail_does_not_materialize_unrelated_cached_cards() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = "version 2\nagent \"amber\" { command \"true\" }\nagent \"cobalt\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &plan.subject_tokens, "point-card")
        .unwrap();
    let snapshot = new_client_snapshot(&state);
    // An unrelated cached card cannot be decoded as a harness. A point read must not touch it.
    state
        .store
        .cached_agent_resources(snapshot.store_index, false, |_| {
            Ok(vec![
                json!({"id":"agent/node.cobalt", "_status_source":true}),
            ])
        })
        .unwrap();
    let Json(card) = client_agents_detail(
        State(state),
        Extension(snapshot),
        AxumPath("node.amber".into()),
        Query(ClientListQuery::default()),
    )
    .await
    .unwrap();
    assert_eq!(card["id"], "agent/node.amber");
    assert_eq!(card["name"], "node.amber");
    assert!(card.get("todo").is_some());
    assert!(card.get("_status_source").is_none());
}

#[tokio::test]
async fn agent_page_overlays_only_returned_cards_and_retains_continuation() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = "version 2\nagent \"amber\" { command \"true\" }\nagent \"cobalt\" { command \"true\" }\nagent \"indigo\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &plan.subject_tokens, "page-cards")
        .unwrap();
    let snapshot = new_client_snapshot(&state);
    let oracle = client_agent_resources(
        &state.store,
        false,
        &snapshot.created_at,
        snapshot.store_index,
    )
    .unwrap();
    let (_, Json(first)) = client_agents(
        State(state.clone()),
        Extension(snapshot.clone()),
        Query(ClientListQuery {
            limit: Some(1),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(first.items, oracle[..1]);
    assert!(first.page.has_more);
    // A later runtime observation must not enter the second card's original cut.
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/node.cobalt".into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), json!("running")),
                ("runtime_id".into(), json!("node.cobalt")),
                ("incarnation_id".into(), json!("cobalt-new")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let (_, Json(second)) = client_agents(
        State(state),
        Extension(snapshot),
        Query(ClientListQuery {
            limit: Some(1),
            cursor: first.page.next_cursor,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(second.items, oracle[1..2]);
    assert!(second.page.has_more);
}

#[test]
fn agent_page_constructs_only_its_selected_subject_and_matches_full_projection() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = format!(
        "version 2\n{}",
        (0..16)
            .map(|n| format!("agent \"card-{n:02}\" {{ command \"true\" }}\n"))
            .collect::<String>()
    );
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let plan = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.clone(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &plan.subject_tokens, "page-reduction-work")
        .unwrap();
    let index = state.store.index().unwrap();
    let oracle = client_agent_resources(&state.store, false, "cut", index).unwrap();
    state.store.forget_current_views();
    crate::store::SUBJECT_REDUCTIONS.with(|count| count.set(0));
    let refs = client_agent_page_refs(&state.store, false, index).unwrap();
    assert_eq!(refs.len(), 16);
    assert_eq!(
        crate::store::SUBJECT_REDUCTIONS.with(std::cell::Cell::get),
        0
    );
    let cards =
        client_agent_cards_for_page(&state.store, false, index, &refs[..1], "cut").unwrap();
    assert_eq!(
        crate::store::SUBJECT_REDUCTIONS.with(std::cell::Cell::get),
        1
    );
    assert_eq!(cards, oracle[..1]);
    let builds = state.store.agent_resources_builds_for_test();
    assert_eq!(client_agent_cards_for_page(&state.store, false, index, &refs[..1], "cut")
        .unwrap(), cards);
    assert_eq!(state.store.agent_resources_builds_for_test(), builds);
    assert_eq!(client_agent_resources(&state.store, false, "cut", index).unwrap(), oracle);
    assert_eq!(crate::store::SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 16);
    assert_eq!(state.store.agent_resources_builds_for_test(), builds + 1);
    client_agent_cards_for_page(&state.store, false, index, &refs[1..2], "cut").unwrap();
    assert_eq!(state.store.agent_resources_builds_for_test(), builds + 1);
}

#[test]
fn agent_page_refs_reuse_warm_metadata_and_rebuild_for_new_membership() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let append = |subject: &str| state.store.append_claim(&ClaimInput {
        subject: subject.into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(json!({"status":"running", "runtime_id":subject,
            "incarnation_id":"one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    append("agent/refs-first");
    let index = state.store.index().unwrap();
    let refs = client_agent_page_refs(&state.store, false, index).unwrap();
    assert_eq!(refs, client_agent_page_refs_uncached(&state.store, false, index).unwrap());
    let work = smallclaims::sqlite::work::total();
    assert_eq!(client_agent_page_refs(&state.store, false, index).unwrap(), refs);
    assert_eq!((smallclaims::sqlite::work::total() - work).statements, 0);
    // A repeated live status has the same result; runtime changes still rebuild shallow refs.
    append("agent/refs-first");
    let index = state.store.index().unwrap();
    assert_eq!(client_agent_page_refs(&state.store, false, index).unwrap(), refs);
    append("agent/refs-second");
    let index = state.store.index().unwrap();
    let added = client_agent_page_refs(&state.store, false, index).unwrap();
    assert_eq!(added.len(), 2);
    assert_eq!(added, client_agent_page_refs_uncached(&state.store, false, index).unwrap());
    state.store.forget_current_views();
    assert_eq!(added, client_agent_page_refs(&state.store, false, index).unwrap());
    state.store.append_claim(&ClaimInput {
        subject: "agent/refs-second".into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(json!({"status":"stopped", "runtime_id":"agent/refs-second",
            "incarnation_id":"one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let index = state.store.index().unwrap();
    let removed = client_agent_page_refs(&state.store, false, index).unwrap();
    assert_eq!(removed.len(), 1, "a stopped undeclared runtime moves to history");
    assert_eq!(removed, client_agent_page_refs_uncached(&state.store, false, index).unwrap());
}

#[test]
fn agent_lifecycle_roster_tracks_current_declaration_without_defaults() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    for (version, lifecycle) in [None, Some("standing"), Some("owner"), Some("bounded"), None]
        .into_iter().enumerate()
    {
        let field = lifecycle.map(|value| format!("lifecycle \"{value}\";")).unwrap_or_default();
        let source = format!("version 2\nagent \"example/purpose\" {{ {field} command \"true\" }}");
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let plan = store.mission(&intent, crate::model::IntentInput {
            kdl: source, source_name: None,
        }).unwrap();
        store.apply(&intent, &plan.subject_tokens, &format!("lifecycle-{version}")).unwrap();
        let index = store.index().unwrap();
        for history in [false, true] {
            let refs = client_agent_page_refs(store, history, index).unwrap();
            let rows = checked_agent_cache(store, history, index);
            let cards = client_agent_cards_for_page(store, history, index, &refs, "0").unwrap();
            for row in refs.iter().chain(rows.iter()).chain(cards.iter()) {
                assert_eq!(row.get("lifecycle"), lifecycle.map(|value| json!(value)).as_ref());
            }
        }
    }
}

#[test]
fn agent_page_refs_rebuild_for_work_desired_membership_and_order_changes() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = |command: &str, extra: &str| format!(r#"version 2
agent "amber" {{ command "{command}"; name "Amber" }}
{extra}
mission "refs-work" state="ready" {{
  concurrent-runs max=2
  goal "Exercise roster reference invalidation."
  step "work" {{ assigned-to "agent/node.amber" }}
}}
"#);
    let apply = |source: String, key: &str| {
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let plan = store.mission(&intent, crate::model::IntentInput {
            kdl: source, source_name: None,
        }).unwrap();
        store.apply(&intent, &plan.subject_tokens, key).unwrap();
    };
    apply(source("true", ""), "refs-original");
    let builds = std::cell::Cell::new(0_usize);
    let read = |rebuild: bool| {
        let index = store.index().unwrap();
        let before = builds.get();
        let refs = store.cached_agent_page_refs(index, false, || {
            builds.set(builds.get() + 1);
            client_agent_page_refs_uncached(store, false, index)
        }).unwrap();
        assert_eq!(builds.get(), before + usize::from(rebuild));
        assert_eq!(refs, client_agent_page_refs_uncached(store, false, index).unwrap());
        refs
    };
    read(true);
    read(false);
    store.append_claim(&ClaimInput {
        subject: "agent/node.amber".into(), kind: "harness.observed".into(),
        actor: Some("agent/node.amber".into()),
        fields: BTreeMap::from([("state".into(), Value::String("working".into()))]),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    read(false);
    let new_run = |key: &str| {
        let run = store.create_mission_run(&MissionRunRequest {
            mission: "refs-work".into(), revision: None, workspace: "/tmp".into(),
            requester: Some("person/test".into()), mode: Some("run".into()),
            inputs: BTreeMap::new(), idempotency_key: key.into(),
        }).unwrap();
        store.set_step_state(&run.steps[0].subject, "ready", None).unwrap();
        run
    };
    let first = new_run("refs-first-work");
    let refs = read(true);
    assert_eq!(refs[0]["next_work_id"], first.steps[0].subject);
    let second = new_run("refs-second-work");
    read(true);
    // Queue ordering is a separate claim on the existing agent, not an observation.
    store.move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
        agent: "agent/node.amber".into(), run: second.id.clone(), placement: "top".into(),
        anchor: None, reason: None, actor: "person/test".into(),
        idempotency_key: "refs-reorder".into(),
    }).unwrap();
    let refs = read(true);
    assert_eq!(refs[0]["next_work_id"], second.steps[0].subject);
    let action = |name: &str| store.work_action(&second.steps[0].subject, name, &WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.amber".into()), incarnation: Some("refs-runtime".into()),
        summary: Some("Complete fixture work.".into()), reason: None, evidence: Vec::new(),
        idempotency_key: format!("refs-{name}"),
    }).unwrap();
    action("claim");
    let refs = read(true);
    assert_eq!(refs[0]["current_work_ids"], json!([second.steps[0].subject]));
    action("complete");
    let refs = read(true);
    assert_eq!(refs[0]["next_work_id"], first.steps[0].subject);
    apply(source("false", ""), "refs-desired-change");
    read(true);
    apply(source("false", r#"agent "beta" { command "true"; name "Beta" }"#), "refs-new-agent");
    let refs = read(true);
    assert_eq!(refs.len(), 2);
    store.rename_agent("agent/node.amber", Some("Zeta"), "refs-name-order").unwrap();
    let refs = read(true);
    assert_eq!(refs.iter().map(|item| item["name"].as_str().unwrap()).collect::<Vec<_>>(),
        ["Beta", "Zeta"]);
}

#[tokio::test]
async fn agent_page_continuation_does_not_seed_frozen_queues_into_the_shared_roster() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = r#"version 2
agent "amber" { command "true" }
agent "zeta" { command "true" }
mission "expiring-work" state="ready" {
  goal "Refresh queue state at its lease boundary."
  step "work" { assigned-to "agent/node.zeta" }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = store.mission(&intent, crate::model::IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    store.apply(&intent, &plan.subject_tokens, "lease-fence-fixture").unwrap();
    let run = store.create_mission_run(&MissionRunRequest {
        mission: "expiring-work".into(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/test".into()), mode: Some("run".into()),
        inputs: BTreeMap::new(), idempotency_key: "lease-fence-run".into(),
    }).unwrap();
    let step = &run.steps[0].subject;
    store.set_step_state(step, "ready", None).unwrap();
    store.work_action(step, "claim", &WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.zeta".into()), incarnation: Some("fixture-runtime".into()),
        summary: None, reason: None, evidence: Vec::new(), idempotency_key: "lease-fence-claim".into(),
    }).unwrap();
    // Shorten this fixture's lease before any cache is built; no claims are changed.
    let expires = client_now_ms() + 1_000;
    store.connection.lock().expect("fixture writer").execute(
        "UPDATE step_runs SET lease_expires_at_unix_ms=?1 WHERE subject=?2",
        rusqlite::params![expires.to_string(), step],
    ).unwrap();
    let index = store.index().unwrap();
    let (status, claimed) = get_request(router(state.clone()), "/v1/client/agents").await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["items"][1]["current_work_ids"], json!([step]));
    let full = client_agent_resources_cached(store, false, index).unwrap();
    assert_eq!(full[1]["current_work_ids"], json!([step]));
    assert_eq!(store.agent_roster_valid_until(index), Some(expires));
    let (status, first) = get_request(router(state.clone()), "/v1/client/agents?limit=1").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"][0]["id"], "agent/node.amber");
    let cursor = first["page"]["next_cursor"].as_str().unwrap();
    // Wait for the actual captured lease boundary, not an arbitrary settling interval.
    tokio::time::sleep(Duration::from_millis(
        u64::try_from(expires.saturating_sub(client_now_ms()) + 1).unwrap(),
    )).await;
    assert_eq!(store.index().unwrap(), index, "lease expiry must not append a claim");
    let continuation = format!("/v1/client/agents?limit=1&cursor={}", urlencoding::encode(cursor));
    let (status, second) = get_request(router(state.clone()), &continuation).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["items"][0]["id"], "agent/node.zeta");
    assert_eq!(second["items"][0]["current_work_ids"], json!([step]),
        "the continuation must preserve the first page's frozen queue cut");
    let full = client_agent_resources_cached(store, false, index).unwrap();
    assert_eq!(full[1]["current_work_ids"], json!([]), "frozen queues must not poison WS cards");
    assert_eq!(full[1]["next_work_id"], step.as_str());
    let (status, ready) = get_request(router(state.clone()), "/v1/client/agents").await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["items"][1]["current_work_ids"], json!([]));
    assert_eq!(ready["items"][1]["next_work_id"], step.as_str());
    assert_eq!(store.agent_roster_valid_until(index), None);
}

#[test]
fn message_delivery_exposes_preparation_fifo_blockers_and_clears_only_current_mail() {
    let store = Store::open_memory("claude-preparation-delivery").unwrap();
    let seat = "agent/claude-preparation-worker";
    let append = |subject: &str, kind: &str, actor: &str, fields: Value, evidence: Vec<String>| {
        store.append_client_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: Some(actor.into()),
            fields: serde_json::from_value(fields).unwrap(), evidence,
            expected_subject: None, idempotency_key: None,
        }).unwrap()
    };
    append(seat, "runtime.observed", seat, json!({
        "status":"running", "runtime_id":"claude-runtime", "incarnation_id":"first"
    }), Vec::new());
    let messages: Vec<_> = ["head", "later", "last"].into_iter().map(|name| {
        let subject = format!("message/claude-preparation-{name}");
        let sent = append(&subject, "message.sent", "person/sender", json!({
            "from":"person/sender", "to":seat, "content":name, "status":"sent"
        }), Vec::new());
        (store.message(&subject).unwrap().unwrap(), sent.accepted_at_unix_ms)
    }).collect();
    let diagnostic = |message: &MessageView, incarnation: &str, status: &str, reason: &str| {
        append(seat, "harness.diagnostic", seat, json!({
            "severity":"warning", "code":"claude-message-unforwarded",
            "status":status, "reason":reason, "incarnation_id":incarnation
        }), vec![store.claims_for(&message.subject, Some("message.sent")).unwrap()[0].id.clone()])
    };
    diagnostic(&messages[0].0, "first", "waiting",
        "FIFO head preparation failed: /private/internal/body.txt unavailable; token=private-detail");
    for (message, _) in &messages[1..] {
        diagnostic(message, "first", "waiting",
            "queued behind FIFO head message/claude-preparation-head: body unavailable; later mail cannot overtake it");
    }
    for (message, sent_at) in &messages {
        let delivery = message_delivery_for(&store, message, *sent_at, *sent_at).unwrap();
        assert_eq!(delivery["state"], "waiting");
        assert_eq!(delivery["phase"], "sent");
        assert_eq!(delivery["reason"], crate::claude_channel::PREPARATION_WAIT_REASON);
        assert!(!delivery["reason"].as_str().unwrap().contains("/private/"));
        assert!(!delivery["reason"].as_str().unwrap().contains("private-detail"));
        let mut accepted = message.clone();
        accepted.status = "delivered".into();
        assert!(!message_delivery_for(&store, &accepted, *sent_at, *sent_at).unwrap()["reason"]
            .as_str().unwrap().contains("FIFO"));
    }
    // Match an operator's actor-scoped list: a person filter selects that person's
    // incoming mailbox, not the messages they sent to this seat.
    let resources = client_message_resources(&store, None, false, Some(seat)).unwrap();
    assert_eq!(resources.len(), 3);
    assert!(resources.iter().all(|resource| resource["delivery"]["state"] == "waiting"));
    // A different message's clear cannot release the head or its queued successors.
    diagnostic(&messages[2].0, "first", "ready", "preparation succeeded; FIFO blocker cleared");
    assert!(message_delivery_for(&store, &messages[0].0, messages[0].1, messages[0].1)
        .unwrap()["reason"].as_str().unwrap().contains("preparation is unavailable"));
    // Recovery clears each message. Late previous-incarnation reports cannot undo it.
    for (message, sent_at) in &messages {
        diagnostic(message, "first", "ready", "preparation succeeded; FIFO blocker cleared");
        diagnostic(message, "old", "waiting", "stale incarnation failure");
        let delivery = message_delivery_for(&store, message, *sent_at, *sent_at).unwrap();
        assert_eq!(delivery["state"], "pending");
        assert!(!delivery["reason"].as_str().unwrap().contains("failure"));
    }
    append(seat, "runtime.observed", seat, json!({
        "status":"running", "runtime_id":"claude-runtime", "incarnation_id":"second"
    }), Vec::new());
    diagnostic(&messages[0].0, "second", "waiting", "current attachment preparation failed");
    diagnostic(&messages[0].0, "first", "ready", "old incarnation clear");
    assert_eq!(message_delivery_for(&store, &messages[0].0, messages[0].1, messages[0].1)
        .unwrap()["reason"], crate::claude_channel::PREPARATION_WAIT_REASON);
    let stale = message_delivery_for(&store, &messages[0].0, messages[0].1, messages[0].1 + 90_001).unwrap();
    assert_eq!(stale["reason"], crate::claude_channel::PREPARATION_WAIT_REASON);
    assert_eq!(messages.iter().map(|(message, _)| store.message(&message.subject).unwrap().unwrap().content)
        .collect::<Vec<_>>(), ["head", "later", "last"]);
}

#[test]
fn message_delivery_distinguishes_pending_acceptance_and_recipient_read() {
    let store = Store::open_memory("message-delivery-test").unwrap();
    let to = "agent/remote/message-delivery-test";
    assert_eq!(
        message_delivery_value(&store, to, "sent", 1_000, 2_000).unwrap()["state"],
        "pending"
    );
    let late = message_delivery_value(&store, to, "staged", 1_000, 12_000).unwrap();
    assert_eq!(late["state"], "waiting");
    assert_eq!(late["phase"], "staged");
    assert_eq!(late["recipient_delivery"], Value::Null);
    let accepted = message_delivery_value(&store, to, "delivered", 1_000, 12_000).unwrap();
    assert_eq!(accepted["state"], "waiting");
    assert!(
        accepted["reason"]
            .as_str()
            .unwrap()
            .contains("waiting for recipient read")
    );
    assert_eq!(
        message_delivery_value(&store, to, "read", 1_000, 12_000).unwrap()["state"],
        "read"
    );
}

#[test]
fn message_delivery_keeps_expired_claude_hook_mail_held_until_native_closure() {
    let store = Store::open_memory("expired-claude-hook-mail-hold").unwrap();
    let seat = "agent/expired-claude-hook-mail-hold";
    let append = |kind: &str, fields: Value| {
        store.append_claim(&ClaimInput {
            subject: seat.into(), kind: kind.into(), actor: Some(seat.into()),
            fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
            expected_subject: None, idempotency_key: None,
        }).unwrap()
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    let now = client_now_ms();
    let expired = now - crate::store::CLAUDE_PROMPT_ANSWER_WAIT_MS - 1_000;
    append("harness.observed", json!({
        "state":"working", "driver":"claude", "incarnation_id":"one",
        "blocked_on":"human", "ask":"permission", "reason":"Read",
        "observed_at_ms":expired, "ownership_sequence":1, "transition_sequence":5
    }));
    let heartbeat = append("harness.observed", json!({
        "state":"working", "driver":"claude", "incarnation_id":"one",
        "blocked_on":"human", "ask":"permission", "reason":"Read",
        "observed_at_ms":now, "ownership_sequence":1, "transition_sequence":5
    }));
    assert_eq!(store.native_prompt_state(seat, 1, 5).unwrap(), crate::store::NativePromptState::Open);
    for phase in ["sent", "staged"] {
        let delivery = message_delivery_value(&store, seat, phase, now, heartbeat.accepted_at_unix_ms).unwrap();
        assert_eq!(delivery["state"], "waiting");
        assert!(delivery["reason"].as_str().unwrap().contains("held outside"));
    }
    let closed = append("harness.observed", json!({
        "state":"idle", "driver":"claude", "incarnation_id":"one",
        "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":6
    }));
    assert_eq!(store.native_prompt_state(seat, 1, 5).unwrap(), crate::store::NativePromptState::Gone);
    for phase in ["sent", "staged"] {
        assert!(!message_delivery_value(&store, seat, phase, now, closed.accepted_at_unix_ms).unwrap()["reason"]
            .as_str().unwrap().contains("held outside"));
    }
}

#[test]
fn message_delivery_holds_native_decisions_until_the_current_observation_clears() {
    for driver in ["omp", "pi", "claude", "codex", "opencode"] {
        for ask in ["question", "permission", "approval"] {
            for release in ["answer", "cancel", "timeout"] {
                let store = Store::open_memory("message-decision-hold").unwrap();
                let to = "agent/remote/message-decision-hold";
                let append = |kind: &str, fields: Value| {
                    store.append_claim(&ClaimInput {
                        subject: to.into(),
                        kind: kind.into(),
                        actor: Some(to.into()),
                        fields: serde_json::from_value(fields).unwrap(),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    }).unwrap()
                };
                append("runtime.observed", json!({
                    "status": "running", "runtime_id": "hold-runtime", "incarnation_id": "first"
                }));
                let observed = append("harness.observed", json!({
                    "state": "working", "driver": driver, "incarnation_id": "first",
                    "blocked_on": "human", "ask": ask
                }));
                let now = observed.accepted_at_unix_ms;
                for phase in ["sent", "staged"] {
                    let delivery = message_delivery_value(&store, to, phase, now, now).unwrap();
                    assert_eq!(delivery["state"], "waiting", "{driver}/{ask}/{release}: {delivery}");
                    assert_eq!(delivery["phase"], phase);
                    assert_eq!(delivery["age_ms"], 0);
                    assert!(delivery["reason"].as_str().unwrap().contains("held outside the native harness"));
                    assert_eq!(delivery["recipient_delivery"], Value::Null);
                }
                for phase in ["delivered", "read", "closed"] {
                    let delivery = message_delivery_value(&store, to, phase, now, now).unwrap();
                    assert!(!delivery["reason"].as_str().unwrap().contains("held"));
                    assert_eq!(delivery["phase"], phase);
                    assert_eq!(delivery["state"], if phase == "delivered" { "pending" } else { "read" });
                }
                let resumed = append("harness.observed", json!({
                    "state": "working", "driver": driver, "incarnation_id": "first",
                    "blocked_on": null, "ask": null, "reason": release
                }));
                for phase in ["sent", "staged"] {
                    let delivery = message_delivery_value(
                        &store, to, phase, now, resumed.accepted_at_unix_ms
                    ).unwrap();
                    assert!(!delivery["reason"].as_str().unwrap().contains("held"),
                        "{driver}/{ask}/{release}: {delivery}");
                }
            }
        }
    }
}

#[test]
fn message_delivery_does_not_hold_stale_or_restarted_native_decisions() {
    let store = Store::open_memory("message-decision-restart").unwrap();
    let to = "agent/remote/message-decision-restart";
    let append = |kind: &str, fields: Value| {
        store.append_claim(&ClaimInput {
            subject: to.into(),
            kind: kind.into(),
            actor: Some(to.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap()
    };
    append("runtime.observed", json!({
        "status": "running", "runtime_id": "hold-runtime", "incarnation_id": "first"
    }));
    let observed = append("harness.observed", json!({
        "state": "blocked", "driver": "omp", "incarnation_id": "first",
        "blocked_on": "human", "ask": "question"
    }));
    let now = observed.accepted_at_unix_ms;
    assert!(message_delivery_value(&store, to, "staged", now, now).unwrap()["reason"]
        .as_str().unwrap().contains("held"));
    let stale = message_delivery_value(&store, to, "staged", now, now + 90_001).unwrap();
    assert_eq!(stale["state"], "waiting");
    assert!(!stale["reason"].as_str().unwrap().contains("held"));

    let restarted = append("runtime.observed", json!({
        "status": "running", "runtime_id": "hold-runtime", "incarnation_id": "second"
    }));
    // Even a subsequently received old-incarnation block cannot restore the hold.
    let late = append("harness.observed", json!({
        "state": "blocked", "driver": "omp", "incarnation_id": "first",
        "blocked_on": "human", "ask": "permission", "reason": "late old prompt"
    }));
    for phase in ["sent", "staged"] {
        for at in [restarted.accepted_at_unix_ms, late.accepted_at_unix_ms] {
            assert!(!message_delivery_value(&store, to, phase, now, at).unwrap()["reason"]
                .as_str().unwrap().contains("held"));
        }
    }
    append("harness.observed", json!({
        "state": "working", "driver": "omp", "incarnation_id": "second",
        "blocked_on": "human", "ask": "question"
    }));
    let ended = append("harness.observed", json!({
        "state": "ended", "driver": "omp", "incarnation_id": "second"
    }));
    assert!(!message_delivery_value(&store, to, "staged", now, ended.accepted_at_unix_ms)
        .unwrap()["reason"].as_str().unwrap().contains("held"));
}

#[test]
fn waiting_native_seats_retain_delivery_presence() {
    let recipient = "agent/eval/waiting-delivery-presence";
    delivery_presence::record_legacy(recipient, "omp-channel", std::process::id());
    for harness in [
        "ready",
        "working",
        "idle",
        "blocked",
        "indeterminate",
        "unauthenticated",
    ] {
        let mut item = json!({
            "id": recipient, "driver": "omp", "host_id": "local",
            "state": "waiting", "harness_state": harness,
            "blocked_on": "human", "ask": "approval",
        });
        overlay_delivery_presence(&mut item, "local");
        assert_eq!(item["delivery"]["state"], "legacy", "{harness}: {item}");
        assert_eq!(item["state"], "waiting");
        assert_eq!(item["blocked_on"], "human");
        assert_eq!(item["ask"], "approval");
    }
    for (state, host, driver) in [
        ("stopped", "local", "omp"),
        ("failed", "local", "omp"),
        ("desired", "local", "omp"),
        ("waiting", "remote", "omp"),
        ("waiting", "local", "shell"),
    ] {
        let mut item = json!({
            "id": recipient, "driver": driver, "host_id": host,
            "state": state, "harness_state": "working",
        });
        overlay_delivery_presence(&mut item, "local");
        assert!(item.get("delivery").is_none(), "{item}");
        assert_eq!(item["state"], state);
    }
}

#[test]
fn mailbox_reads_do_not_renew_a_native_delivery_beat() {
    let recipient = "agent/eval/ordinary-mailbox-read";
    record_legacy_poll(None, Some(recipient), false);
    assert!(delivery_presence::known(recipient).is_none());
    let peer = NativeDeliveryPeer {
        agent: recipient.into(),
        transport: "omp-channel",
        pid: 37,
        archives_inbox: false,
    };
    record_legacy_poll(Some(&peer), Some("agent/eval/other-mailbox"), false);
    record_legacy_poll(Some(&peer), Some(recipient), true);
    assert!(delivery_presence::known(recipient).is_none());
    record_legacy_poll(Some(&peer), Some(recipient), false);
    assert_eq!(delivery_presence::known(recipient).unwrap().state, "legacy");
}

#[test]
fn native_title_subscriptions_recognize_the_outer_pi_and_omp_drivers() {
    for driver in ["pi", "omp"] {
        let args = vec!["st".into(), "driver".into(), driver.into()];
        let env = vec!["ST_AGENT=agent/eval.worker".into()];
        let peer = native_delivery_identity(37, &args, &env).expect("native title owner");
        assert_eq!(peer.agent, "agent/eval.worker");
        assert_eq!(peer.transport, format!("{driver}-channel"));
        assert!(!peer.archives_inbox);
        assert!(native_delivery_identity(37, &args, &[]).is_none(), "argv alone cannot authorize a seat");
    }
}

#[test]
fn legacy_claude_mcp_is_a_delivery_peer_but_ordinary_mailbox_queries_are_not() {
    let env = vec!["ST_AGENT=agent/example/legacy".into()];
    let args = ["st3", "driver", "claude-mcp"].map(str::to_owned);
    let peer = native_delivery_identity(37, &args, &env).unwrap();
    assert_eq!(peer.transport, "claude-channel");
    assert_eq!(peer.agent, "agent/example/legacy");
    let query = ["st3", "conversations", "ls"].map(str::to_owned);
    assert!(native_delivery_identity(37, &query, &env).is_none());
    assert!(native_delivery_identity(37, &args, &[]).is_none());
}

#[tokio::test]
async fn legacy_native_drivers_renew_delivery_when_their_projection_includes_closed_mail() {
    // Before a63824ef, native drivers polled include_closed=true to archive
    // inbox files. This is the real pre-September-25 request shape, rather
    // than the newer pre-reexec channel's active-mail-only poll.
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    for driver in ["claude", "codex", "opencode"] {
        for endpoint in ["messages/page", "messages"] {
            let recipient = format!(
                "agent/example/closed-{driver}-{}",
                endpoint.replace('/', "-")
            );
            let query = format!("/v1/{endpoint}?to={recipient}&include_closed=true&limit=100");
            let (status, _) = get_request(app.clone(), &query).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                delivery_presence::known(&recipient).is_none(),
                "ordinary history query renewed a beat"
            );

            let args = ["st3", "driver", driver].map(str::to_owned);
            let env = vec![format!("ST_AGENT={recipient}")];
            let peer = native_delivery_identity(37, &args, &env).unwrap();
            let other = format!("{recipient}-other");
            let other_query =
                format!("/v1/{endpoint}?to={other}&include_closed=true&limit=100");
            let (status, _) =
                get_request(app.clone().layer(Extension(peer.clone())), &other_query).await;
            assert_eq!(status, StatusCode::OK);
            assert!(delivery_presence::known(&other).is_none());
            assert!(delivery_presence::known(&recipient).is_none());
            let (status, _) = get_request(app.clone().layer(Extension(peer)), &query).await;
            assert_eq!(status, StatusCode::OK);
            let assessment = delivery_presence::known(&recipient)
                .expect("native projection did not renew its beat");
            assert_eq!(assessment.state, "legacy");
            assert_eq!(assessment.polled_seconds_ago, Some(0));
        }
    }
}

#[tokio::test]
async fn channel_history_queries_do_not_renew_delivery() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    for driver in ["claude-mcp", "pi-channel", "omp-channel"] {
        for endpoint in ["messages/page", "messages"] {
            let recipient = format!(
                "agent/example/history-{driver}-{}",
                endpoint.replace('/', "-")
            );
            let args = ["st3", "driver", driver].map(str::to_owned);
            let env = vec![format!("ST_AGENT={recipient}")];
            let peer = native_delivery_identity(37, &args, &env).unwrap();
            let query = format!("/v1/{endpoint}?to={recipient}&include_closed=true&limit=100");
            let (status, _) = get_request(app.clone().layer(Extension(peer)), &query).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                delivery_presence::known(&recipient).is_none(),
                "channel history query renewed a beat"
            );
        }
    }
}

#[test]
fn doctor_names_failed_admission_even_if_opencode_reports_ready() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"version 2
agent "fixture" { workspace "/tmp"; harness "opencode" {} }
"#;
    let intent = parse_intent(source, "node").unwrap();
    let plan = state.store.mission(&intent, crate::model::IntentInput { kdl: source.into(), source_name: None }).unwrap();
    state.store.apply(&intent, &plan.subject_tokens, "admission-doctor").unwrap();
    let subject = "agent/node.fixture";
    let append = |kind: &str, fields: BTreeMap<String, Value>| {
        state.store.append_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
            fields, evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    append("runtime.observed", BTreeMap::from([
        ("status".into(), json!("running")), ("runtime_id".into(), json!("fixture")),
        ("incarnation_id".into(), json!("first")),
    ]));
    append("harness.diagnostic", BTreeMap::from([
        ("code".into(), json!("harness-admission-failed")), ("status".into(), json!("degraded")),
        ("severity".into(), json!("warning")),
        ("reason".into(), json!("opencode 99.42.7 admission failed at admissionIdleEdge")),
        ("incarnation_id".into(), json!("first")),
    ]));
    append("harness.observed", BTreeMap::from([
        ("state".into(), json!("ready")), ("driver".into(), json!("opencode")),
        ("incarnation_id".into(), json!("first")),
    ]));
    let report = doctor_report(&state).unwrap().0;
    let readiness = report.checks.iter().find(|check| check.name == "driver-readiness").unwrap();
    assert_eq!(readiness.status, "warn");
    assert!(readiness.message.contains("admissionIdleEdge"), "{readiness:?}");
    let index = state.store.index().unwrap();
    let resources = client_agent_resources(&state.store, false, "snapshot", index).unwrap();
    assert_eq!(resources[0]["state"], "waiting");
}

#[test]
fn doctor_unread_counts_exclude_historical_recipients_and_people() {
    let store = Store::open_memory("amber").unwrap();
    for (id, recipient) in [
        ("current", "agent/example/current"),
        ("retired", "agent/example/retired"),
        ("person", "person/eval"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: format!("message/{id}"),
                kind: "message.sent".into(),
                actor: Some("person/operator".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/operator")),
                    ("to".into(), json!(recipient)),
                    ("status".into(), json!("sent")),
                    ("content".into(), json!("probe")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(id.into()),
            })
            .unwrap();
    }
    let current = BTreeSet::from(["agent/example/current"]);
    let messages = store.operational_messages(None, false).unwrap();
    assert_eq!(
        unread_current_seat_counts(&store, &current, &messages, client_now_ms()).unwrap(),
        (0, 0)
    );
    assert_eq!(
        unread_current_seat_counts(&store, &current, &messages, client_now_ms() + 20_000)
            .unwrap(),
        (1, 0)
    );
    assert_eq!(
        store.messages(None, false).unwrap().len(),
        3,
        "inspection does not erase historical messages"
    );
}

pub fn state(root: &Path) -> AppState {
    AppState {
        owning_person: None,
        store: Arc::new(Store::open_memory("node").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "node".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    }
}

#[test]
fn doctor_distinguishes_no_fleet_from_explicitly_leaving_a_fleet() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let replication_check = || {
        doctor_report(&state)
            .unwrap()
            .0
            .checks
            .into_iter()
            .find(|check| check.name == "replication")
            .unwrap()
    };
    let check = replication_check();
    assert_eq!(check.status, "pass");
    assert!(check.message.contains("no fleet is configured"));
    assert!(check.message.contains("st fleet create"));
    assert!(check.message.contains("st fleet join"));
    assert!(!check.message.contains("intentionally"));

    fs::write(
        root.path().join("left-fleet.json"),
        r#"{"fleet_id":"previous-fleet","offline":true,"confirmed_by":null}"#,
    )
    .unwrap();
    let check = replication_check();
    assert_eq!(check.status, "pass");
    assert!(check.message.contains("intentionally local-only"));
    assert!(check.message.contains("after leaving its fleet"));
}

#[tokio::test]
async fn rules_audit_an_agents_write_then_refuse_it_once_enforced() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    for (name, rule) in crate::rules::lockdown(&[]) {
        let (status, body) = json_request(
            app.clone(),
            "/v1/rules/set",
            json!({"actor": "person/ada", "name": name, "rule": rule}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, rules) = get_request(app.clone(), "/v1/rules").await;
    assert_eq!(rules.as_array().unwrap().len(), 3, "{rules}");
    assert!(
        rules
            .as_array()
            .unwrap()
            .iter()
            .all(|rule| rule["mode"] == "audit"),
        "{rules}"
    );
    let publish = |name: &str| {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/documents")
            .header("content-type", "application/json")
            .extension(BoundAgent("agent/team/web/reviewer".into()))
            .body(Body::from(
                serde_json::to_vec(&DocumentPutRequest {
                    name: name.into(),
                    bytes: b"notes".to_vec(),
                    expected_document: None,
                    idempotency_key: format!("put:{name}"),
                })
                .unwrap(),
            ))
            .unwrap();
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
        }
    };
    // Inside its namespace nothing is logged; outside, the write proceeds and is logged.
    assert_eq!(publish("doc/team/web/plan").await.0, StatusCode::OK);
    assert_eq!(publish("doc/team/api/plan").await.0, StatusCode::OK);
    let (_, audits) = get_request(app.clone(), "/v1/rules/audit").await;
    let audits = audits.as_array().unwrap();
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0]["rule"], "agents-publish-in-namespace");
    assert_eq!(audits[0]["actor"], "agent/team/web/reviewer");
    assert_eq!(audits[0]["action"], "doc.bound");
    assert_eq!(audits[0]["target"], "doc/team/api/plan");

    // Enforced, the same write is refused with a typed reason and nothing is stored.
    let mut rule = crate::rules::lockdown(&[])
        .into_iter()
        .find(|(name, _)| *name == "agents-publish-in-namespace")
        .unwrap()
        .1;
    rule.mode = smallclaims::rules::Mode::Enforce;
    let (status, body) = json_request(
        app.clone(),
        "/v1/rules/set",
        json!({"actor": "person/ada", "name": "agents-publish-in-namespace", "rule": rule}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, refused) = publish("doc/team/api/later").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["code"], "rule-denied", "{refused}");
    assert!(state.store.claims_for("doc/team/api/later", None).unwrap().is_empty());
    assert_eq!(publish("doc/team/web/later").await.0, StatusCode::OK);

    // Only a person sets rules.
    let (status, refused) = json_request(
        app.clone(),
        "/v1/rules/set",
        json!({"actor": "agent/team/web/reviewer", "name": "agents-publish-in-namespace", "rule": rule}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
}

#[tokio::test]
async fn slo_lists_objectives_and_account_budgets_and_charts_the_local_history() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let now = client_now_ms() as u64;
    let (status, body) = json_request(
        app.clone(),
        "/v1/claims",
        json!({"subject": "agent/example.live", "kind": "harness.limits", "actor": "agent/example.live",
            "fields": {"driver": "claude", "account_ref": "ada/claude-1", "weekly_percent": 30.0,
                "five_hour_percent": 10.0, "measured_at_unix_ms": now - 10_000},
            "evidence": [], "idempotency_key": "slo-limits"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, list) = get_request(app.clone(), "/v1/slo").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let rows = list["rows"].as_array().unwrap();
    let find = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap_or_else(|| panic!("no {id}"));
    assert_eq!(find("person-read")["kind"], "objective");
    assert_eq!(find("ada/claude-1/weekly")["kind"], "budget");
    assert_eq!(find("ada/claude-1/weekly")["remaining_percent"], 70.0);
    assert_eq!(find("ada/claude-1/5h")["remaining_percent"], 90.0);

    // The history is read from the log the daemon appends; a bad period is refused.
    let dir = crate::slo_history::dir(&state.state_dir);
    crate::slo_history::append(&dir, now - 180_000, &[json!({"id": "cpu", "remaining_percent": 80.0})]).unwrap();
    crate::slo_history::append(&dir, now - 60_000, &[json!({"id": "cpu", "remaining_percent": 60.0})]).unwrap();
    let (status, history) = get_request(
        app.clone(),
        &format!("/v1/slo/history?id=cpu&since_ms={}&until_ms={now}&steps=2", now - 240_000),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["samples"], 2);
    assert_eq!(history["steps"], json!([80.0, 60.0]));
    // Clients read the same rows and the same history, typed.
    let (status, client_list) = get_request(app.clone(), "/v1/client/slo").await;
    assert_eq!(status, StatusCode::OK, "{client_list}");
    // get_request unwraps the client envelope, asserting its version, snapshot and request id.
    let typed: st3_client::SloList = serde_json::from_value(client_list.clone()).unwrap();
    let weekly = typed.rows.iter().find(|row| row.id == "ada/claude-1/weekly").unwrap();
    assert_eq!((weekly.kind.as_str(), weekly.remaining_percent), ("budget", Some(70.0)));
    assert!(weekly.resets_at_unix_ms.is_none() && weekly.measured_at_unix_ms.is_some());
    assert!(typed.rows.iter().any(|row| row.kind == "objective" && row.id == "person-read"));
    let (status, client_history) = get_request(
        app.clone(),
        &format!("/v1/client/slo/history?id=cpu&since_ms={}&until_ms={now}&steps=2", now - 240_000),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{client_history}");
    let typed: st3_client::SloHistory = serde_json::from_value(client_history.clone()).unwrap();
    assert_eq!((typed.id.as_str(), typed.samples, typed.steps), ("cpu", 2, vec![Some(80.0), Some(60.0)]));
    for query in [
        format!("id=cpu&since_ms={now}&until_ms={now}"),
        format!("id=cpu&since_ms=0&until_ms={now}"),
        format!("id=cpu&since_ms={}&until_ms={now}&steps=0", now - 1000),
    ] {
        let (status, _) = get_request(app.clone(), &format!("/v1/slo/history?{query}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        // The client route refuses a bad period the same way, as a validation failure in the client vocabulary.
        let (status, refused) = get_request(app.clone(), &format!("/v1/client/slo/history?{query}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}: {refused}");
        assert_eq!(refused["code"], "validation-failed", "{query}: {refused}");
    }
    // A client session without read.projections reads neither route.
    let session = || Extension(client_v0::ClientSession::for_tests_scoped("person/fixture", "person/fixture", &["read.attention"]));
    let denied = client_v0::slo_list(State(state.clone()), session()).await.unwrap_err();
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let denied = client_v0::slo_history(
        State(state.clone()),
        session(),
        Query(SloHistoryQuery { id: "cpu".into(), since_ms: None, until_ms: None, steps: None }),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_usage_report_marks_only_old_limits_readings_stale() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let now = client_now_ms() as u64;
    for (seat, account, measured) in [
        ("agent/example.old", "ada/claude-1", now - 60 * 3_600_000),
        ("agent/example.live", "ada/claude-2", now - 10_000),
    ] {
        let (status, body) = json_request(
            app.clone(),
            "/v1/claims",
            json!({"subject": seat, "kind": "harness.limits", "actor": seat,
                "fields": {"driver": "claude", "account_ref": account, "weekly_percent": 84.0,
                    "measured_at_unix_ms": measured},
                "evidence": [], "idempotency_key": format!("limits-{seat}")}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, report) = get_request(app, "/v1/usage").await;
    let stale = |name: &str| {
        report["limits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|limit| limit["account_ref"] == name)
            .unwrap_or_else(|| panic!("{name} missing in {report}"))["stale"]
            .clone()
    };
    assert_eq!(stale("ada/claude-1"), true);
    assert_eq!(stale("ada/claude-2"), false);
}

#[tokio::test]
async fn isolated_daemon_exposes_only_rollups_in_the_fleet_usage_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let subject = "agent/example.worker";
    let request = ClaimInput {
        subject: subject.into(),
        kind: "harness.timeline".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("operation".into(), Value::String("append".into())),
            ("entry_id".into(), Value::String("response-a".into())),
            (
                "source_id".into(),
                Value::String("response-a-source".into()),
            ),
            ("sequence".into(), Value::from(1)),
            ("revision".into(), Value::from(1)),
            ("role".into(), Value::String("system".into())),
            ("entry_type".into(), Value::String("usage".into())),
            ("final".into(), Value::Bool(true)),
            ("driver".into(), Value::String("claude".into())),
            ("incarnation_id".into(), Value::String("inc-one".into())),
            (
                "body".into(),
                json!({"semantics":"response","turn_id":"turn-a","model":"claude-example",
                "input_tokens":4,"output_tokens":2,"cache_write_tokens":3,"cached_tokens":20,"total_tokens":29}),
            ),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("usage-api-response-a".into()),
    };
    let (status, observation) = json_request(
        app.clone(),
        "/v1/claims",
        serde_json::to_value(&request).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{observation}");
    assert!(
        observation["id"]
            .as_str()
            .unwrap()
            .starts_with("local-observation/")
    );
    let (status, repeated) = json_request(
        app.clone(),
        "/v1/claims",
        serde_json::to_value(&request).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(repeated["id"], observation["id"]);
    let (_, report) = get_request(
        app.clone(),
        &format!("/v1/usage?since_ms=0&until_ms={}", client_now_ms() + 60_000),
    )
    .await;
    assert_eq!(report["rows"].as_array().unwrap().len(), 1);
    assert_eq!(report["rows"][0]["total_tokens"], 29);
    assert_eq!(report["rows"][0]["cache_write_tokens"], 3);
    assert_eq!(report["rows"][0]["cached_tokens"], 20);
    // Clients read the same rows, without the identities st does not know.
    let (status, period) = get_request(
        app.clone(),
        &format!(
            "/v1/client/usage?since_ms=0&until_ms={}",
            client_now_ms() + 60_000
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{period}");
    let period = period.get("value").unwrap_or(&period);
    let row = &period["rows"][0];
    assert_eq!(period["rows"].as_array().unwrap().len(), 1, "{period}");
    assert_eq!(row["agent"], subject);
    assert_eq!(row["total_tokens"], 29);
    assert!(
        row.get("mission_run").is_none() && row.get("step").is_none(),
        "{row}"
    );
    let typed: st3_client::UsagePeriod = serde_json::from_value(period.clone()).unwrap();
    assert_eq!(typed.rows[0].model.as_deref(), Some("claude-example"));
    assert!(
        typed.limits.is_empty(),
        "no harness has reported limits here"
    );
    let backwards = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/client/usage?since_ms=2&until_ms=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(backwards.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // The response timeline and its latest-retention rollup both stay local.
    assert_eq!(
        state.store.local_observations_after(0, 10).unwrap().len(),
        2
    );
    assert_eq!(
        state
            .store
            .claims_for(subject, Some("harness.usage"))
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_local_only_harness_observation_wakes_only_the_client_feed() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let wake_file = root.path().join("replication.wake");
    let mut client_feed = state.event_notify.subscribe();
    let subject = "agent/node.worker";
    let observed = |state_name: &str, observed_at_ms: u64| ClaimInput {
        subject: subject.into(),
        kind: "harness.observed".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("state".into(), Value::String(state_name.into())),
            ("incarnation_id".into(), Value::String("inc-1".into())),
            ("observed_at_ms".into(), Value::from(observed_at_ms)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("wake-{state_name}-{observed_at_ms}")),
    };
    let usage = |tokens: u64| ClaimInput {
        subject: subject.into(),
        kind: "harness.usage".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("driver".into(), Value::String("codex".into())),
            ("incarnation_id".into(), Value::String("inc-1".into())),
            (
                "semantics".into(),
                Value::String("context_occupancy".into()),
            ),
            ("context_used_tokens".into(), Value::from(tokens)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("wake-usage-{tokens}")),
    };
    let post = |input: ClaimInput| post_claim(State(state.clone()), Json(input));
    let reconciler_woke = || async {
        tokio::time::timeout(Duration::from_millis(20), state.notify.notified())
            .await
            .is_ok()
    };
    let mut client_feed_woke = || {
        let changed = client_feed.has_changed().unwrap();
        client_feed.borrow_and_update();
        changed
    };

    let change = post(observed("working", 1)).await.unwrap().0;
    assert!(crate::store::local_observation_position(&change).is_none());
    assert!(reconciler_woke().await);
    assert!(wake_file.exists());
    assert!(client_feed_woke());
    fs::remove_file(&wake_file).unwrap();

    let heartbeat = post(observed("working", 300_001)).await.unwrap().0;
    assert!(crate::store::local_observation_position(&heartbeat).is_some());
    assert!(!reconciler_woke().await, "a heartbeat does not reconcile");
    assert!(!wake_file.exists(), "a heartbeat does not wake replication");
    assert!(client_feed_woke(), "clients still see the observation");

    let first_usage = post(usage(10)).await.unwrap().0;
    assert!(crate::store::local_observation_position(&first_usage).is_none());
    assert!(wake_file.exists(), "replicated usage wakes replication");
    assert!(!reconciler_woke().await, "usage never reconciles");
    assert!(client_feed_woke());
    fs::remove_file(&wake_file).unwrap();

    let throttled = post(usage(20)).await.unwrap().0;
    assert!(crate::store::local_observation_position(&throttled).is_some());
    assert!(
        !wake_file.exists(),
        "usage kept local does not wake replication"
    );
    assert!(!reconciler_woke().await);
    assert!(client_feed_woke());

    // The reconciler reads no limits claim; the limits policy runs on its own timer.
    let limits = |weekly: f64| ClaimInput {
        subject: subject.into(),
        kind: "harness.limits".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("driver".into(), Value::String("codex".into())),
            ("account".into(), Value::String("codex/ada".into())),
            ("account_ref".into(), Value::String("ada".into())),
            ("weekly_percent".into(), Value::from(weekly)),
            ("measured_at_unix_ms".into(), Value::from(1u64)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("wake-limits-{weekly}")),
    };
    let _ = post(limits(10.0)).await.unwrap();
    assert!(!reconciler_woke().await, "limits never reconcile");
    assert!(client_feed_woke());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_admission_does_not_block_the_async_runtime_when_store_reads_wait() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state
        .store
        .put_document("doc/admission-probe", b"probe", &None, "admission-probe")
        .unwrap();
    let store = state.store.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        store.hold_read_connections_for_test(|| {
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let app = router(state);
    let requests: Vec<_> = (0..8)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move { get_request(app, "/v1/client/capabilities").await })
        })
        .collect();
    let started = std::time::Instant::now();
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "a blocked SQLite read must not pin the async request acceptor (elapsed {:?})",
        started.elapsed()
    );

    for request in requests {
        let (status, _) = request.await.unwrap();
        assert_eq!(status, StatusCode::OK);
    }
    holder.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_terminal_attach_does_not_delay_an_independent_request() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        store.hold_read_connections_for_test(|| {
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let app = router(state);
    let started = Instant::now();
    let attach = tokio::spawn(json_request(
        app.clone(),
        "/v1/sessions/attach/agent/probe",
        json!({}),
    ));
    tokio::task::yield_now().await;
    let (status, _) = get_request(app, "/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "health waited {:?} behind a blocked terminal attach",
        started.elapsed()
    );

    holder.join().unwrap();
    let (status, _) = attach.await.unwrap();
    assert!(!status.is_success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_reads_answer_during_a_long_write_and_busy_query_pool() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    // A disk-backed store uses WAL like the daemon. Shared-cache memory stores
    // deliberately make readers wait for an uncommitted writer.
    state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
    let store = state.store.clone();
    // Each holder keeps its connections until the test has asked its three questions and says
    // so (or drops the sender by panicking), rather than for a fixed 30 s.
    let (release_reads, read_released) = std::sync::mpsc::channel::<()>();
    let (read_ready_tx, read_ready_rx) = std::sync::mpsc::channel();
    let read_holder = std::thread::spawn(move || {
        store.hold_read_connections_for_test(|| {
            read_ready_tx.send(()).unwrap();
            let _ = read_released.recv();
        });
    });
    read_ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let store = state.store.clone();
    let (release_write, write_released) = std::sync::mpsc::channel::<()>();
    let (write_ready_tx, write_ready_rx) = std::sync::mpsc::channel();
    let write_holder = std::thread::spawn(move || {
        store.hold_write_transaction_for_test(|| {
            write_ready_tx.send(()).unwrap();
            let _ = write_released.recv();
        });
    });
    write_ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let app = router(state);
    for path in ["/v1/status", "/v1/client/agents", "/v1/client/machines"] {
        let started = Instant::now();
        let response =
            tokio::time::timeout(Duration::from_millis(100), get_request(app.clone(), path))
                .await;
        assert!(
            response.is_ok(),
            "{path} waited {:?} behind a long write or query",
            started.elapsed()
        );
        assert_eq!(response.unwrap().0, StatusCode::OK);
    }
    release_reads.send(()).unwrap();
    release_write.send(()).unwrap();
    read_holder.join().unwrap();
    write_holder.join().unwrap();
}

fn probe_claim(key: &str, state: &str) -> ClaimInput {
    ClaimInput {
        subject: "agent/probe".into(),
        kind: "harness.observed".into(),
        actor: Some("agent/probe".into()),
        fields: BTreeMap::from([("state".into(), Value::String(state.into()))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(key.into()),
    }
}

/// A page whose read a commit lands in, as another thread commits it midway.
async fn page_read_across_a_commit(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    client_snapshot_page(&state, snapshot, "probes", &query, |state, snapshot| {
        let newest = |store: &Store| -> anyhow::Result<u64> {
            Ok(store
                .claims_page(None, None, 0, None, true, 1)?
                .claims
                .first()
                .map_or(0, |claim| claim.store_index))
        };
        let before = newest(&state.store)?;
        let store = state.store.clone();
        std::thread::spawn(move || store.append_claim(&probe_claim("during", "working")))
            .join()
            .unwrap()?;
        anyhow::ensure!(state.store.index()? > snapshot.store_index);
        Ok(vec![json!({
            "id": "probe/page",
            "read_at": snapshot.store_index,
            "before": before,
            "after": newest(&state.store)?,
        })])
    })
    .await
}

/// A first page reads inside one snapshot: a commit that lands while it reads neither tears
/// it nor refuses it, and the envelope names the snapshot the page was read in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_page_answers_from_its_snapshot_when_a_commit_lands_during_it() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    // A file store, as the daemon runs: in shared-cache memory a reader locks writers out.
    state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
    state
        .store
        .append_claim(&probe_claim("first", "idle"))
        .unwrap();
    let admitted_at = state.store.index().unwrap();
    // A page read at the store's first claim names that snapshot, not the newer one the
    // request was admitted at.
    async fn page_read_earlier(
        State(state): State<AppState>,
        Query(query): Query<ClientListQuery>,
    ) -> Result<ClientPageResponse, ApiError> {
        let snapshot = client_snapshot_at(&state, 1);
        let page = client_page_read(&state, &snapshot, "earlier", Vec::new(), &query, true)?;
        Ok((Extension(snapshot), Json(page)))
    }
    let app = Router::new()
        .route("/v1/client/probes", get(page_read_across_a_commit))
        .route("/v1/client/earlier", get(page_read_earlier))
        .layer(from_fn_with_state(
            (state.clone(), ClientTransportBoundary::Unix),
            response_envelope,
        ))
        .with_state(state.clone());
    let envelope = |path: &'static str| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let (status, page) = envelope("/v1/client/probes").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let item = &page["value"]["items"][0];
    assert_eq!(item["read_at"], admitted_at);
    assert_eq!(page["snapshot"]["store_index"], admitted_at);
    // The page saw neither the claim committed while it read nor anything after its snapshot.
    assert_eq!(item["before"], admitted_at);
    assert_eq!(item["after"], admitted_at);
    assert!(state.store.index().unwrap() > admitted_at);

    state
        .store
        .append_claim(&probe_claim("second", "idle"))
        .unwrap();
    let (status, page) = envelope("/v1/client/earlier").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["snapshot"], json!(client_snapshot_at(&state, 1)));
}

/// A seat's mailbox poll, one subject's status, a client's admission, and replication and
/// fleet status each open a read connection of their own. They answer while the writer is
/// held, more reads than the pool keeps idle are held, and long snapshots run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_reads_answer_while_the_writer_and_long_reads_are_held() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
    // A local batch that no exchange has sealed yet: sealing it would need the writer.
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/probe".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/probe".into()),
            fields: BTreeMap::from([("state".into(), Value::String("idle".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let watchdog = Duration::from_secs(30);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut holders = Vec::new();
    // Keep every resource held until all probes finish. Dropping the senders also
    // releases the holders on assertion failure, so a failed probe cannot strand them.
    let mut releases = Vec::new();
    let store = state.store.clone();
    let ready = ready_tx.clone();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    releases.push(release_tx);
    holders.push(std::thread::spawn(move || {
        store.hold_write_transaction_for_test(|| {
            ready.send(()).unwrap();
            let _ = release_rx.recv();
        });
    }));
    let store = state.store.clone();
    let ready = ready_tx.clone();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    releases.push(release_tx);
    holders.push(std::thread::spawn(move || {
        store.hold_read_connections_for_test(|| {
            ready.send(()).unwrap();
            let _ = release_rx.recv();
        });
    }));
    for _ in 0..8 {
        let (store, ready) = (state.store.clone(), ready_tx.clone());
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        releases.push(release_tx);
        holders.push(std::thread::spawn(move || {
            store
                .read_snapshot(|_| {
                    ready.send(()).unwrap();
                    let _ = release_rx.recv();
                    Ok(())
                })
                .unwrap();
        }));
    }
    for _ in 0..holders.len() {
        ready_rx.recv_timeout(watchdog).unwrap();
    }

    // As the daemon does when it starts, so no read makes the first diagnostic report.
    let _ = start_operation_report(&state);
    let app = router(state);
    for path in [
        "/v1/messages/page?include_closed=false&limit=100&to=agent%2Fprobe",
        "/v1/status?subject=agent%2Fprobe",
        "/v1/client/now",
        "/v1/replication/status",
        "/v1/internal/fleet/membership",
        "/v1/client/operations",
    ] {
        // This is a deadlock watchdog, not a latency bound: runner scheduling can
        // delay a request, but it must answer before any held resource is released.
        let response = tokio::time::timeout(watchdog, get_request(app.clone(), path)).await;
        assert!(
            response.is_ok(),
            "{path} did not answer while the writer and other reads were held"
        );
        assert_eq!(response.unwrap().0, StatusCode::OK, "{path}");
    }
    drop(releases);
    for holder in holders {
        holder.join().unwrap();
    }
}

#[test]
fn health_response_does_not_queue_for_a_blocking_thread() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = crate::api::read_deadline::spawn_blocking(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let started = Instant::now();
        let response =
            tokio::time::timeout(Duration::from_millis(250), get_request(app, "/v1/health"))
                .await;
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        assert!(
            response.is_ok(),
            "health waited {:?} for the busy blocking pool",
            started.elapsed()
        );
        assert_eq!(response.unwrap().0, StatusCode::OK);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_seed_first_launch_contract_survives_concurrency_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let database = root.path().join("seed.sqlite3");
    state.store = Arc::new(Store::open(&database, "node").unwrap());
    let socket = root.path().join("seed.sock");
    let server_socket = socket.clone();
    let server_state = state.clone();
    let server = tokio::spawn(async move { serve_unix(&server_socket, router(server_state)).await });
    while !socket.exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
    let client = crate::client::Client::new(crate::client::Endpoint::Unix(socket));
    let id = uuid::Uuid::now_v7().to_string();
    let seed = root.path().join(format!("time_{id}.jsonl"));
    let invalid = root.path().join("missing.jsonl");
    std::fs::write(&seed, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n")).unwrap();
    let launch = |seat: &'static str, incarnation: &'static str, driver: &'static str, seed: Option<PathBuf>, strict: bool| {
        let client = client.clone();
        let inventory = root.path().join(seat.replace('/', "-"));
        async move { crate::native_seed::first_launch(&client, seat, incarnation, driver, seed.as_deref(), &inventory, strict).await }
    };
    assert!(launch("agent/invalid", "one", "omp", Some(invalid.clone()), false).await.is_err());
    let corrupt = root.path().join(format!("corrupt_{id}.jsonl"));
    std::fs::write(&corrupt, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\nbroken\n")).unwrap();
    assert!(launch("agent/invalid", "corrupt", "omp", Some(corrupt), false).await.is_err());
    assert_eq!(launch("agent/invalid", "two", "omp", Some(seed.clone()), false).await.unwrap(), Some(id.clone()), "invalid input must not consume the opportunity");
    assert_eq!(launch("agent/invalid", "three", "omp", Some(seed.clone()), false).await.unwrap(), None, "an unbound seeded receipt starts fresh even while seed remains declared");

    let (one, two) = tokio::join!(
        launch("agent/concurrent", "one", "omp", Some(seed.clone()), false),
        launch("agent/concurrent", "two", "omp", Some(seed.clone()), false)
    );
    let outcomes = [one.unwrap(), two.unwrap()];
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_some()).count(), 1, "only the receipt winner launches seeded; the other launch proceeds fresh");
    let append = |seat: &str, kind: &str, fields: Value| {
        state.store.append_claim(&ClaimInput { subject: seat.into(), kind: kind.into(), actor: Some(seat.into()), fields: serde_json::from_value(fields).unwrap(), evidence: vec![], expected_subject: None, idempotency_key: None }).unwrap();
    };
    append("agent/legacy-binding", "harness.session-file", json!({"harness":"omp","session_id":"legacy"}));
    assert_eq!(launch("agent/legacy-binding", "adopt", "omp", Some(invalid.clone()), false).await.unwrap(), None, "an existing binding wins even without a first-launch receipt");
    append("agent/concurrent", "harness.session-file", json!({"harness":"omp","session_id":id}));
    assert_eq!(launch("agent/concurrent", "restart", "omp", Some(invalid.clone()), false).await.unwrap(), None, "bound history beats edits and invalid seeds");
    assert_eq!(launch("agent/concurrent", "other-account", "claude", None, false).await.unwrap(), None, "history fence is not harness or account scoped");
    let reopened = Store::open(&database, "node").unwrap();
    assert_eq!(crate::suspension::continue_session(&reopened, "agent/concurrent", "omp", None, None).unwrap().unwrap().0, id, "a restarted daemon continues the bound session");
    assert_eq!(reopened.claims_for("agent/concurrent", Some("harness.session-file")).unwrap().len(), 1);
    append("agent/concurrent", "runtime.action.requested", json!({"action":"fresh-context"}));
    append("agent/concurrent", "harness.diagnostic", json!({"code":crate::suspension::CONTINUE_UNAVAILABLE_CODE,"status":"transcript-missing"}));
    assert_eq!(launch("agent/concurrent", "fresh", "omp", Some(seed.clone()), false).await.unwrap(), None);

    assert_eq!(launch("agent/fresh", "one", "claude", None, false).await.unwrap(), None);
    assert_eq!(launch("agent/fresh", "two", "omp", Some(seed.clone()), false).await.unwrap(), None, "a fresh receipt never blocks or re-arms, even if seed is later added");
    for driver in crate::skill::HARNESSES {
        assert_eq!(launch("agent/fresh", "seedless-second-launch", driver, None, false).await.unwrap(), None, "seedless restarts must work for every harness");
    }
    assert_eq!(launch("agent/invalid", "seedless-incomplete", "omp", None, false).await.unwrap(), None, "an incomplete seeded launch proceeds fresh when seed is removed");
    let notice = state.store.latest_claim("agent/invalid", Some("harness.diagnostic")).unwrap().unwrap();
    assert_eq!(notice.body["fields"]["code"], "first-native-launch-incomplete");
    assert_eq!(notice.body["fields"]["severity"], "warning");
    assert_eq!(launch("agent/invalid", "still-declared", "omp", Some(invalid.clone()), false).await.unwrap(), None, "even a missing still-declared seed is not validated or staged again");
    let declared_notice = state.store.latest_claim("agent/invalid", Some("harness.diagnostic")).unwrap().unwrap();
    assert_eq!(declared_notice.body["fields"]["code"], "first-native-launch-incomplete");
    assert_eq!(declared_notice.body["fields"]["incarnation_id"], "still-declared");
    assert!(declared_notice.body["fields"]["reason"].as_str().unwrap().contains("starting fresh"));
    assert!(!declared_notice.body["fields"]["reason"].as_str().unwrap().contains("acknowledge-seed"), "launch notices do not prescribe recovery before starting");
    assert!(crate::native_seed::acknowledge(&client, "agent/fresh", "person/operator", "fresh needs no recovery").await.is_err());
    assert!(crate::native_seed::acknowledge(&client, "agent/invalid", "person/operator", " ").await.is_err());
    let acknowledgement = crate::native_seed::acknowledge(&client, "agent/invalid", "person/operator", "accept fresh after interrupted import").await.unwrap();
    assert_eq!(acknowledgement.kind, "custom.agent.first-native-launch-acknowledged");
    assert_eq!(acknowledgement.actor.as_deref(), Some("person/operator"));
    assert_eq!(acknowledgement.body["fields"]["reason"], "accept fresh after interrupted import");
    assert_eq!(acknowledgement.body["evidence"][0], acknowledgement.body["fields"]["receipt"]);
    assert_eq!(launch("agent/invalid", "recovered", "omp", Some(invalid.clone()), false).await.unwrap(), None, "acknowledgement records acceptance without reseeding or changing fresh launch behavior");
    assert_eq!(state.store.latest_claim("agent/invalid", Some("harness.diagnostic")).unwrap().unwrap().id, declared_notice.id, "acknowledgement suppresses later incomplete notices, not launches");
    append("agent/deliberately-fresh", "runtime.action.requested", json!({"action":"fresh-context"}));
    assert_eq!(launch("agent/deliberately-fresh", "one", "omp", Some(invalid.clone()), false).await.unwrap(), None);
    assert!(launch("agent/strict", "conflicting", "omp", Some(invalid), true).await.unwrap_err().to_string().contains("native seed cannot accompany resume environment"));
    assert_eq!(launch("agent/strict", "one", "omp", None, true).await.unwrap(), None, "a rejected seed conflict must not consume the opportunity; strict resume proceeds without a seed");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_seed_restart_before_first_message_never_wedges_fresh_seat() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("restart.sqlite3");
    let inventory = root.path().join("inventory");
    for attempt in 0..2 {
        let mut state = state(root.path());
        state.store = Arc::new(Store::open(&database, "node").unwrap());
        let socket = root.path().join(format!("restart-{attempt}.sock"));
        let client = crate::client::Client::new(crate::client::Endpoint::Unix(socket.clone()));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve_unix(&server_socket, router(state)).await });
        while !socket.exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
        assert_eq!(crate::native_seed::first_launch(
            &client, "agent/before-message", &format!("incarnation-{attempt}"), "omp",
            None, &inventory, false,
        ).await.unwrap(), None);
        server.abort();
        let _ = server.await;
    }
    let reopened = Store::open(&database, "node").unwrap();
    assert!(reopened.claims_for("agent/before-message", Some("harness.session-file")).unwrap().is_empty());
    assert!(reopened.claims_for("agent/before-message", Some("custom.agent.initial-message")).unwrap().is_empty());
    assert!(!inventory.exists(), "unseeded restarts require no seed inventory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_daemon_answers_health_under_stalled_attaches() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let server_state = state.clone();
    let server =
        tokio::spawn(async move { serve_unix(&server_socket, router(server_state)).await });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let store = state.store.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        store.hold_read_connections_for_test(|| {
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(1_200));
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let client = crate::client::Client::new(crate::client::Endpoint::Unix(socket));
    let pending = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                let _: anyhow::Result<Value> = client
                    .post("/v1/sessions/attach/agent/probe", &json!({}))
                    .await;
                started.elapsed()
            })
        })
        .collect::<Vec<_>>();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let mut latencies = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        let _: Value = client.get("/v1/health").await.unwrap();
        latencies.push(started.elapsed());
    }
    latencies.sort();
    assert!(
        latencies[29] < Duration::from_secs(1),
        "health p99 was {:?}",
        latencies[29]
    );
    let mut attach_latencies = Vec::new();
    for request in pending {
        attach_latencies.push(request.await.unwrap());
    }
    assert!(
        attach_latencies
            .iter()
            .all(|latency| *latency < Duration::from_secs(1)),
        "terminal attaches waited behind background reads: {attach_latencies:?}"
    );
    holder.join().unwrap();
    let doctor: DoctorReport = client.get("/v1/doctor").await.unwrap();
    assert!(doctor.checks.iter().any(|check| {
        check.name.starts_with("request-latency/") && check.message.contains("p99")
    }));
    let (status, latency) = get_request(router(state), "/v1/client/request-latency").await;
    assert_eq!(status, StatusCode::OK);
    assert!(latency["routes"].as_array().unwrap().iter().any(|route| {
        route["route"] == "/v1/health" && route["count"].as_u64().unwrap_or_default() >= 30
    }));
    server.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn slow_peer_identity_lookup_does_not_stop_existing_api_connections() {
    use http_body_util::{BodyExt as _, Empty};
    use hyper::client::conn::http1::SendRequest;

    fn slow_ancestor(_pid: u32) -> Option<String> {
        std::thread::sleep(Duration::from_millis(500));
        None
    }

    async fn health(sender: &mut SendRequest<Empty<axum::body::Bytes>>) {
        let response = sender
            .send_request(
                Request::builder()
                    .uri("/v1/health")
                    .header("host", "local")
                    .body(Empty::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.into_body().collect().await.unwrap();
    }

    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let app = router(state(root.path()));
    let server = tokio::spawn(async move {
        serve_unix_with_ancestor(&server_socket, None, app, true, slow_ancestor).await
    });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    health(&mut sender).await;

    let _slow_peer = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    health(&mut sender).await;
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "an unrelated identity lookup stalled the API for {:?}",
        started.elapsed()
    );
    server.abort();
}

#[test]
fn cached_page_cursor_survives_graph_change_and_stays_person_scoped() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let snapshot = new_client_snapshot(&state);
    let query = ClientListQuery {
        limit: Some(2),
        person: Some("person/alex".into()),
        ..ClientListQuery::default()
    };
    let first = client_page(
        &state,
        &snapshot,
        "messages",
        vec![
            json!({"id":"message/1"}),
            json!({"id":"message/2"}),
            json!({"id":"message/3"}),
        ],
        &query,
    )
    .unwrap();
    let cursor = first
        .page
        .next_cursor
        .expect("three rows require a second page");
    state
        .store
        .append_claim(&crate::model::ClaimInput {
            subject: "custom/client/pairing-page-churn".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("credential_hash".into(), Value::String("unused".into())),
                (
                    "session_actor".into(),
                    Value::String("client/unused".into()),
                ),
                ("person_id".into(), Value::String("person/alex".into())),
                ("scopes".into(), json!(["read.projections"])),
                ("expires_at_unix_ms".into(), json!(u64::MAX)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert_ne!(
        new_client_snapshot(&state).store_index,
        snapshot.store_index
    );
    let continuation = ClientListQuery {
        cursor: Some(cursor),
        ..query
    };
    let second = client_page(&state, &snapshot, "messages", Vec::new(), &continuation).unwrap();
    assert_eq!(second.items[0]["id"], "message/3");
    let other_person = ClientListQuery {
        person: Some("person/other".into()),
        ..continuation
    };
    assert_eq!(
        client_page(&state, &snapshot, "messages", Vec::new(), &other_person)
            .unwrap_err()
            .code,
        "page-cursor-expired"
    );
}

#[test]
fn timeline_pages_preserve_large_typed_events_and_advance_by_returned_count() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let snapshot = new_client_snapshot(&state);
    let expected_ids: Vec<_> = (0..100).map(|id| format!("native/{id}")).collect();
    // Typed native blocks: an `omp_event` view whose content and details are each at the
    // native-value bound, beside the owner-fetchable raw record.
    let items = expected_ids.iter().map(|id| {
        let view = json!({
            "type":"omp_event", "kind":"irc:incoming", "attribution":"agent",
            "content":"x".repeat(8000),
            "details":{"from":"worker","message":"x".repeat(8000)}
        });
        json!({"id":id,"body":{"blocks":[{
            "id":format!("{id}/0"), "kind":"status", "source_type":"custom_message",
            "payload":{"raw":{"type":"custom_message","customType":"irc:incoming"}}, "view":view
        }]}})
    }).collect();
    let mut query = ClientListQuery {
        limit: Some(100),
        ..ClientListQuery::default()
    };
    let mut page =
        client_page(&state, &snapshot, "timeline/session/smoke", items, &query).unwrap();
    let mut received_ids = Vec::new();
    loop {
        assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES - 1024);
        for item in &page.items {
            received_ids.push(item["id"].as_str().unwrap().to_owned());
            let view = &item["body"]["blocks"][0]["view"];
            assert_eq!(view["content"], "x".repeat(8000));
            assert_eq!(view["details"]["message"], "x".repeat(8000));
        }
        let Some(cursor) = page.page.next_cursor else {
            assert!(!page.page.has_more);
            break;
        };
        assert!(page.page.has_more);
        query.cursor = Some(cursor);
        page = client_page(
            &state,
            &snapshot,
            "timeline/session/smoke",
            Vec::new(),
            &query,
        )
        .unwrap();
    }
    assert_eq!(received_ids, expected_ids);
}

#[test]
fn timeline_cached_page_serves_lone_entry_above_target_below_hard_ceiling() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let snapshot = new_client_snapshot(&state);
    let large = json!({"id":"large", "body":{"text":"x".repeat(896 * 1024)}});
    let small = json!({"id":"small"});
    let mut query = ClientListQuery {
        limit: Some(100),
        ..Default::default()
    };
    let page = client_page(
        &state,
        &snapshot,
        "timeline/session/lone",
        vec![large.clone(), small.clone()],
        &query,
    )
    .unwrap();
    assert_eq!(page.items, vec![large]);
    assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
    query.cursor = page.page.next_cursor;
    assert!(query.cursor.is_some());
    let page = client_page(
        &state,
        &snapshot,
        "timeline/session/lone",
        Vec::new(),
        &query,
    )
    .unwrap();
    assert_eq!(page.items, vec![small]);
    assert!(!page.page.has_more);
}

#[test]
fn timeline_cached_page_cuts_one_oversized_entry_visibly_without_skipping_it() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let snapshot = new_client_snapshot(&state);
    let large = json!({"id":"large", "body":{"text":"x".repeat(CLIENT_MAX_RESPONSE_BYTES)}});
    let mut query = ClientListQuery::default();
    let page = client_page(
        &state,
        &snapshot,
        "timeline/session/oversized",
        vec![large, json!({"id":"small"})],
        &query,
    )
    .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0]["id"], "large");
    assert_eq!(page.items[0]["type"], "error");
    assert_eq!(page.items[0]["body"]["code"], "timeline-entry-too-large");
    assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
    query.cursor = page.page.next_cursor;
    assert!(query.cursor.is_some(), "the entry after it is not skipped");
    let page = client_page(
        &state,
        &snapshot,
        "timeline/session/oversized",
        Vec::new(),
        &query,
    )
    .unwrap();
    assert_eq!(page.items, vec![json!({"id":"small"})]);
    assert_eq!(client_timeline_page_end(&[], 9, 10).unwrap(), 0);
    assert_eq!(
        client_timeline_page_end(&[json!({"id":"small"})], 9, 10).unwrap(),
        1
    );
}

#[test]
fn managed_session_cache_tracks_a_new_runtime_incarnation() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let subject = "agent/cache-test";
    for incarnation in ["first", "second"] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("runtime_id".into(), Value::String("cache-runtime".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let sessions = client_session_resources(
            &state.store,
            false,
            &snapshot.created_at,
            snapshot.store_index,
            None,
            false,
        )
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["revision"], incarnation);
    }
}

#[test]
fn managed_session_dates_incarnation_after_claim_cap_without_local_timeline() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let subject = "agent/ada";
    // Build both subjects' old history in one transaction, without projection rebuilds.
    {
        let mut connection = state.store.connection.write();
        let transaction = connection.transaction().unwrap();
        for _ in 0..=SESSION_CLAIMS {
            crate::store::append_claim_tx(
                &transaction,
                &state.store.origin,
                subject,
                "harness.observed",
                None,
                &json!({"fields": {"state": "idle", "incarnation_id": "first"}}),
                &[],
                None,
            )
            .unwrap();
            crate::store::append_claim_tx(
                &transaction,
                &state.store.origin,
                "agent/reviewer",
                "harness.observed",
                None,
                &json!({"fields": {"state": "idle"}}),
                &[],
                None,
            )
            .unwrap();
        }
        transaction.commit().unwrap();
    }
    let first = state.store.append_claim(&ClaimInput {
        subject: subject.into(),
        kind: "runtime.observed".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("status".into(), json!("running")),
            ("runtime_id".into(), json!("runtime-ada")),
            ("incarnation_id".into(), json!("second")),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    assert_eq!(
        state.store.latest_local_timeline_at(subject, "second", first.store_index).unwrap(),
        None
    );
    for at in ["2026-10-11T10:00:00.000Z", "2026-10-11T10:01:00.000Z"] {
        let sessions = managed_session_resources(
            &state.store, false, at, first.store_index,
        ).unwrap().0;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["started_at"], client_timestamp(first.accepted_at_unix_ms));
        assert_eq!(sessions[0]["updated_at"], client_timestamp(first.accepted_at_unix_ms));
    }
    // The unindexed legacy path stays capped, but its required start date is durable too.
    let legacy = state.store.append_claim(&ClaimInput {
        subject: "agent/reviewer".into(),
        kind: "runtime.observed".into(),
        actor: Some("agent/reviewer".into()),
        fields: BTreeMap::from([
            ("status".into(), json!("running")),
            ("runtime_id".into(), json!("runtime-reviewer")),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    for at in ["2026-10-11T10:02:00.000Z", "2026-10-11T10:03:00.000Z"] {
        let sessions = managed_session_resources(
            &state.store, false, at, legacy.store_index,
        ).unwrap().0;
        let legacy_session = sessions.iter().find(|session| {
            session["owner_id"] == "agent/reviewer"
        }).unwrap();
        assert!(legacy_session["runtime_incarnation"].is_null());
        assert_eq!(legacy_session["started_at"], client_timestamp(legacy.accepted_at_unix_ms));
    }
}

#[tokio::test]
async fn document_history_prefix_filters_before_limit_and_has_a_next_page() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for index in 0..202 {
        let name = format!("doc/early/{index:03}");
        state
            .store
            .put_document(&name, b"early", &None, &format!("early-{index}"))
            .unwrap();
    }
    let mut head = None;
    for index in 0..2 {
        let version = state
            .store
            .put_document(
                "doc/late/target",
                format!("version-{index}").as_bytes(),
                &head,
                &format!("late-{index}"),
            )
            .unwrap();
        head = Some(version.binding_claim_id);
    }
    let app = router(state);
    let (status, first) = get_request(
        app.clone(),
        "/v1/documents?prefix=doc%2Flate&history=true&limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"][0]["name"], "doc/late/target");
    assert_eq!(first["has_more"], true);
    assert!(first["next_cursor"].is_string(), "{first}");
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = get_request(
        app,
        &format!(
            "/v1/documents?prefix=doc%2Flate&history=true&limit=1&cursor={}",
            urlencoding::encode(cursor)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["items"][0]["name"], "doc/late/target");
    assert_ne!(first["items"][0]["hash"], second["items"][0]["hash"]);
    assert_eq!(second["has_more"], false);
}

fn observe_terminal(store: &Store, subject: &str, incarnation: &str, terminal: bool) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("worker-runtime".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(terminal)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

/// example-linux, 2026-09-29: under CI load an attach waited behind the store writer until its
/// WebSocket handshake gave up. Naming a local terminal must not wait for the writer.
#[tokio::test]
async fn a_local_terminal_is_named_by_reads_alone_while_the_writer_is_busy() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    // An isolated daemon may be started with a relative PTY root.
    state.pty_root = PathBuf::from("isolated/pty");
    observe_terminal(
        &state.store,
        "agent/worker",
        "4242:2026-09-29T08:00:00.000Z",
        true,
    );
    let store = state.store.clone();
    let index = store.index().unwrap();
    let app = router(state);
    let (held, holding) = std::sync::mpsc::channel();
    let holder_store = store.clone();
    let holder = std::thread::spawn(move || {
        holder_store.hold_writer_for_test(|| {
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(1_500));
        });
    });
    holding.recv().unwrap();

    let started = std::time::Instant::now();
    let (status, terminal) = get_request(app, "/v1/sessions/local-terminal/agent/worker").await;
    let elapsed = started.elapsed();
    holder.join().unwrap();

    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert!(
        elapsed < Duration::from_millis(750),
        "naming a local terminal waited {elapsed:?} for the writer"
    );
    assert_eq!(terminal["subject"], "agent/worker");
    assert_eq!(terminal["runtime_id"], "worker-runtime");
    assert_eq!(terminal["incarnation_id"], "4242:2026-09-29T08:00:00.000Z");
    assert_eq!(
        terminal["pty_root"],
        std::env::current_dir()
            .unwrap()
            .join("isolated/pty")
            .display()
            .to_string()
    );
    assert_eq!(store.index().unwrap(), index, "it must write nothing");
}

#[tokio::test]
async fn a_local_terminal_keeps_the_attachment_checks() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    observe_terminal(
        &state.store,
        "agent/headless",
        "1:2026-09-29T08:00:00.000Z",
        false,
    );
    let owner = Store::open_memory("owner-node").unwrap();
    observe_terminal(&owner, "agent/remote", "2:2026-09-29T08:00:00.000Z", true);
    state
        .store
        .import_replication("owner-node", &owner.export_replication(0).unwrap())
        .unwrap();
    observe_terminal(
        &state.store,
        "agent/local",
        "3:2026-09-29T08:00:00.000Z",
        true,
    );
    // A paired client never learns a PTY path: the gateway serves only client v0.
    let (status, refused) = get_request(
        fabric_router(state.clone()),
        "/v1/sessions/local-terminal/agent/local",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let app = router(state);

    let (status, refused) =
        get_request(app.clone(), "/v1/sessions/local-terminal/agent/headless").await;
    assert!(status.is_client_error(), "{refused}");
    assert_eq!(refused["code"], "unsupported-capability");

    let (status, refused) =
        get_request(app.clone(), "/v1/sessions/local-terminal/agent/remote").await;
    assert!(status.is_client_error(), "{refused}");
    assert_eq!(refused["code"], "runtime-not-local");

    // An unknown subject is refused exactly as its WebSocket attachment is.
    let (status, refused) =
        get_request(app.clone(), "/v1/sessions/local-terminal/agent/missing").await;
    let (attach_status, attach_refused) =
        json_request(app, "/v1/sessions/attach/agent/missing", json!({})).await;
    assert!(status.is_client_error(), "{refused}");
    assert_eq!(status, attach_status);
    assert_eq!(refused["code"], attach_refused["code"], "{refused}");
}

#[tokio::test]
async fn a_claim_waiting_for_the_store_writer_leaves_the_api_answering() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let (held, holding) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        store.hold_writer_for_test(|| {
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(1_500));
        });
    });
    holding.recv().unwrap();

    // This test runs on one thread. A claim that waited for the writer on it would hold the
    // whole runtime, even this sleep, until the writer was released.
    let started = std::time::Instant::now();
    let claim = tokio::spawn(json_request(
        app.clone(),
        "/v1/claims",
        json!({
            "subject": "custom/acme/fact",
            "kind": "custom.acme.found",
            "fields": {},
        }),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, _) = get_request(app, "/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_millis(750),
        "health waited {:?} behind a claim that was waiting for the writer",
        started.elapsed()
    );

    holder.join().unwrap();
    let (status, body) = claim.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn json_request(app: Router, path: &str, value: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&value).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
    if path.starts_with("/v1/client/") {
        assert_eq!(envelope["api_version"], "st3.client.v0");
        assert_eq!(envelope["snapshot"]["host_id"], "host/node");
        assert!(envelope["request_id"].as_str().unwrap().contains('-'));
        assert!(envelope["snapshot"]["store_index"].is_u64());
    } else {
        assert_eq!(envelope["api_version"], "st3.v1");
        assert_eq!(envelope["snapshot_host"], "node");
        assert!(envelope["request_id"].as_str().unwrap().contains('-'));
        assert!(envelope["store_index"].is_u64());
    }
    let value = if status.is_success() {
        envelope["value"].clone()
    } else {
        envelope
    };
    (status, value)
}

#[tokio::test]
async fn an_exact_work_retry_survives_provider_exit_after_commit() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let response: StepRunView = serde_json::from_value(json!({
        "subject": "step-run/run/work",
        "run": "mission-run/run",
        "generation": "run-generation/run",
        "step": "work",
        "definition_hash": "definition",
        "status": "verifying",
        "attempt": 1,
        "assigned_to": "agent/node.worker",
        "agentless": false,
        "title": null,
        "worker_reported": true,
        "claimant": null,
        "claim_incarnation": null,
        "claim_expires_at_unix_ms": null,
        "execution_elapsed_ms": 1,
        "readiness_epoch": 1,
        "blocked_reason": null,
        "not_before_unix_ms": null,
        "created_at_unix_ms": 1,
        "updated_at_unix_ms": 2
    }))
    .unwrap();
    state
        .store
        .cache_idempotency_response("work-retry", &response)
        .unwrap();

    let (status, body) = json_request(
        router(state),
        "/v1/work/complete/step-run/run/work",
        serde_json::to_value(WorkRequest {
            wait_ms: None,
            actor: Some("agent/node.worker".into()),
            incarnation: Some("ended-incarnation".into()),
            summary: Some("complete".into()),
            reason: None,
            evidence: Vec::new(),
            idempotency_key: "work-retry".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["subject"], response.subject);
    assert_eq!(body["status"], "verifying");
}

async fn get_request(app: Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
    if path.starts_with("/v1/client/") {
        assert_eq!(envelope["api_version"], "st3.client.v0");
        assert!(envelope["request_id"].as_str().unwrap().contains('-'));
        if status.is_success() {
            assert_eq!(envelope["snapshot"]["host_id"], "host/node");
            assert!(envelope["snapshot"]["store_index"].is_u64());
        } else {
            assert_eq!(envelope["error_version"], "st3.client.error.v0");
            assert!(envelope["code"].as_str().is_some());
        }
    } else {
        assert_eq!(envelope["api_version"], "st3.v1");
        assert_eq!(envelope["snapshot_host"], "node");
        assert!(envelope["request_id"].as_str().unwrap().contains('-'));
        assert!(envelope["store_index"].is_u64());
    }
    let value = if status.is_success() {
        envelope["value"].clone()
    } else {
        envelope
    };
    (status, value)
}

#[test]
fn replication_heartbeats_do_not_process_the_backlog() {
    assert!(!replication_receive_has_new_data(0));
    assert!(replication_receive_has_new_data(1));
}

#[tokio::test]
async fn visible_change_updates_clients_and_peer_without_waking_reconciler() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let mut events = state.event_notify.subscribe();
    signal_visible_change(&state);
    events.changed().await.unwrap();
    assert_eq!(*events.borrow(), 1);
    assert!(state.state_dir.join("replication.wake").exists());
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            state.notify.notified(),
        )
        .await
        .is_err()
    );
    signal_changed(&state);
    tokio::time::timeout(
        std::time::Duration::from_millis(20),
        state.notify.notified(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn only_work_wake_messages_wake_the_reconciler() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let woke = || {
        let notify = state.notify.clone();
        async move {
            tokio::time::timeout(Duration::from_millis(50), notify.notified())
                .await
                .is_ok()
        }
    };
    let _ = woke().await;
    let mut events = state.event_notify.subscribe();
    let send = |key: &str, tags: &[&str]| {
        serde_json::to_value(MessageSendRequest {
            idempotency_key: key.into(),
            from: "person/example".into(),
            to: "agent/receiver".into(),
            content: "A note.".into(),
            title: None,
            in_reply_to: None,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            attachments: Vec::new(),
        })
        .unwrap()
    };
    // A conversation and its lifecycle reach clients but never wake the reconciler.
    let (status, sent) =
        json_request(app.clone(), "/v1/messages", send("talk", &["chat"])).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert!(!woke().await, "a conversation message woke the reconciler");
    assert!(events.has_changed().unwrap());
    events.borrow_and_update();
    let id = sent["value"]["subject"]
        .as_str()
        .or(sent["subject"].as_str())
        .unwrap()
        .trim_start_matches("message/")
        .to_owned();
    let (status, claim) = json_request(
        app.clone(),
        &format!("/v1/messages/{id}/claims"),
        json!({"lifecycle": "delivered", "actor": "agent/receiver", "idempotency_key": "talk-delivered"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert!(
        !woke().await,
        "a conversation message's lifecycle woke the reconciler"
    );
    assert!(events.has_changed().unwrap());
    events.borrow_and_update();
    // A repeat, or a transition the message has already passed, changes nothing a reader
    // can see and wakes no reader (#1085).
    for (lifecycle, key) in [("delivered", "talk-delivered"), ("staged", "talk-staged")] {
        let (status, claim) = json_request(
            app.clone(),
            &format!("/v1/messages/{id}/claims"),
            json!({"lifecycle": lifecycle, "actor": "agent/receiver", "idempotency_key": key}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{claim}");
        assert!(
            !events.has_changed().unwrap(),
            "a no-op {lifecycle} woke readers"
        );
    }
    // A daemon's work wake does; ordinary senders cannot assert reserved event tags.
    let mut work_wake = send("wake", &["st3-work:step-run/example/work"]);
    work_wake["from"] = json!("daemon/node");
    let (status, sent) = json_request(
        app.clone(),
        "/v1/messages",
        work_wake,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert!(woke().await, "a work wake did not wake the reconciler");
}

#[tokio::test]
async fn quiet_replication_wakes_skip_full_graph_projection() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let Json(first) = replication_wake(State(state.clone())).await.unwrap();
    assert_eq!(first["projection_attempted"], true);
    assert_eq!(first["projected"], true);
    let Json(second) = replication_wake(State(state)).await.unwrap();
    assert_eq!(second["projection_attempted"], false);
    assert_eq!(second["projected"], true);
    assert_eq!(second["admitted"], 0);
    assert_eq!(second["repairs"], 0);
}

fn materialize_run_agents(state: &AppState, run: &MissionRunView) {
    let mission = state
        .store
        .mission_spec(
            run.mission.strip_prefix("mission/").unwrap_or(&run.mission),
            Some(&run.revision),
        )
        .unwrap()
        .unwrap();
    let Some(source) = mission.declarations_kdl.as_deref() else {
        return;
    };
    let intent = crate::graph::parse_execution_intent(source, &state.node, &run.id).unwrap();
    state
        .store
        .apply_internal(&intent, &format!("test-materialize:{}", run.id))
        .unwrap();
}

fn apply_request(state: &AppState, source: &str, actor: &str, key: &str) -> ApplyRequest {
    let intent = parse_intent(source, &state.node).unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    ApplyRequest {
        intent: crate::model::IntentInput {
            kdl: source.into(),
            source_name: None,
        },
        expected_subjects: preview.subject_tokens,
        idempotency_key: key.into(),
        actor: Some(actor.into()),
    }
}

#[tokio::test]
async fn main_performance_failures_registration_public_preview_and_apply() {
    use crate::store::message_subscriptions::{PERFORMANCE_DECLARATIONS, PERFORMANCE_REGISTRATION};
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let declarations = crate::graph::parse_internal_intent(PERFORMANCE_DECLARATIONS, &state.node).unwrap();
    state.store.apply_internal(&declarations, "workflow-registration-fixture").unwrap();
    let app = router(state.clone());
    let (status, preview) = json_request(app.clone(), "/v1/intent/mission", serde_json::json!({
        "intent": {"kdl": PERFORMANCE_REGISTRATION}
    })).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["blockers"], serde_json::json!([]));
    assert!(!state.store.desired_subjects().unwrap().iter().any(|item| item.kind == "subscription"));
    let request = apply_request(&state, PERFORMANCE_REGISTRATION, "agent/fleet/fixture-project/speed", "workflow-register");
    let request = serde_json::to_value(request).unwrap();
    let (status, first) = json_request(app.clone(), "/v1/intent/apply", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, again) = json_request(app.clone(), "/v1/intent/apply", request).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(first["claim_ids"], again["claim_ids"]);
    assert!(state.store.active_mission_runs().unwrap().is_empty());
}

#[tokio::test]
async fn main_performance_failures_registration_rejects_foreign_bound_actor() {
    use crate::store::message_subscriptions::{PERFORMANCE_DECLARATIONS, PERFORMANCE_REGISTRATION};
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let declarations = crate::graph::parse_internal_intent(PERFORMANCE_DECLARATIONS, &state.node).unwrap();
    state.store.apply_internal(&declarations, "workflow-registration-fixture").unwrap();
    let app = router(state.clone());
    let request = apply_request(&state, PERFORMANCE_REGISTRATION, "agent/foreign", "forged-workflow-register");
    let response = app.oneshot(Request::builder().method("POST").uri("/v1/intent/apply")
        .header("content-type", "application/json").extension(BoundAgent("agent/own".into()))
        .body(Body::from(serde_json::to_vec(&request).unwrap())).unwrap()).await.unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["code"], "foreign-agent-actor");
    assert!(!state.store.desired_subjects().unwrap().iter().any(|item| item.kind == "subscription"));
}

#[tokio::test]
async fn a_seat_writes_only_its_own_handoff() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let write = |writer: Option<&'static str>, name: &str, key: &str| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/documents")
            .header("content-type", "application/json");
        if let Some(writer) = writer {
            request = request.extension(BoundAgent(writer.into()));
        }
        let body = serde_json::json!({
            "name": name,
            "bytes": b"Waiting on PR 12.".to_vec(),
            "expected_document": null,
            "idempotency_key": key,
        });
        let app = app.clone();
        async move {
            let response = app
                .oneshot(request.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&body).unwrap())
        }
    };
    let (status, written) =
        write(Some("agent/garden/ash"), "doc/agents/garden/ash/handoff", "own").await;
    assert_eq!(status, StatusCode::OK, "{written}");

    // Not even its own other names are open to it under doc/agents/.
    let (status, refused) =
        write(Some("agent/garden/ash"), "doc/agents/garden/ash/other", "own-other").await;
    assert_ne!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["code"], "document-not-yours");

    let (status, refused) =
        write(Some("agent/garden/birch"), "doc/agents/garden/ash/handoff", "other").await;
    assert_ne!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["code"], "document-not-yours");

    // A seat's other documents and a person's writes are unaffected.
    let (status, ordinary) = write(Some("agent/garden/birch"), "doc/garden/notes", "notes").await;
    assert_eq!(status, StatusCode::OK, "{ordinary}");
    let (status, person) =
        write(None, "doc/agents/garden/ash/handoff", "person").await;
    assert_eq!(status, StatusCode::OK, "{person}");
}

#[tokio::test]
async fn an_event_waiter_cannot_consume_the_reconciler_signal() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let mut event = state.event_notify.subscribe();

    signal_changed(&state);

    tokio::time::timeout(Duration::from_millis(50), event.changed())
        .await
        .expect("the event waiter did not wake")
        .expect("the event sender closed");
    tokio::time::timeout(Duration::from_millis(50), state.notify.notified())
        .await
        .expect("the reconciler signal was lost");
}

fn retention_quick_request(root: &std::path::Path) -> QuickAgentRequest {
    QuickAgentRequest {
        subject: "retention-worker".into(),
        worktree: root.display().to_string(),
        model: None,
        effort: None,
        arguments: vec![],
        expected_subject: vec![],
        idempotency_key: "retention-quick".into(),
    }
}

#[tokio::test]
async fn retention_quick_response_failure_rolls_back_declaration() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let before = state.store.index().unwrap();
    state
        .store
        .connection
        .write()
        .execute_batch(
            "CREATE TRIGGER retention_quick_failure BEFORE INSERT ON idempotency
             WHEN json_type(NEW.response,'$.runtime_id') IS NOT NULL
             BEGIN SELECT RAISE(ABORT,'injected lost quick response'); END;",
        )
        .unwrap();
    assert!(
        quick_agent(&state, retention_quick_request(root.path()), "codex")
            .await
            .is_err()
    );
    assert_eq!(state.store.index().unwrap(), before);
    assert!(
        state
            .store
            .selected_desired_token("agent/node.retention-worker")
            .unwrap()
            .is_none()
    );
    state
        .store
        .connection
        .write()
        .execute_batch("DROP TRIGGER retention_quick_failure")
        .unwrap();
    let first = quick_agent(&state, retention_quick_request(root.path()), "codex")
        .await
        .unwrap();
    let index = state.store.index().unwrap();
    let second = quick_agent(&state, retention_quick_request(root.path()), "codex")
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    assert_eq!(state.store.index().unwrap(), index);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_quick_concurrent_calls_share_the_response() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    // Shared-cache in-memory SQLite returns SQLITE_LOCKED when one caller reads
    // during another's write. Exercise the daemon's file-backed WAL concurrency.
    state.store = Arc::new(Store::open(&root.path().join("quick.sqlite"), "node").unwrap());
    let start = Arc::new(tokio::sync::Barrier::new(12));
    let request = retention_quick_request(root.path());
    let workers = (0..12)
        .map(|_| {
            let state = state.clone();
            let request = request.clone();
            let start = start.clone();
            tokio::spawn(async move {
                start.wait().await;
                quick_agent(&state, request, "codex").await.unwrap()
            })
        })
        .collect::<Vec<_>>();
    let mut responses = Vec::new();
    for worker in workers {
        responses.push(worker.await.unwrap());
    }
    let first = serde_json::to_value(&responses[0]).unwrap();
    assert!(
        responses
            .iter()
            .all(|response| serde_json::to_value(response).unwrap() == first)
    );
    assert_eq!(
        state
            .store
            .claims_for("agent/node.retention-worker", Some("intent.desired"))
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn retention_quick_legacy_missing_receipt_is_explicitly_reconstructed() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let original = quick_agent(&state, retention_quick_request(root.path()), "codex")
        .await
        .unwrap();
    assert!(!original.response_reconstructed);
    let index = state.store.index().unwrap();
    state
        .store
        .connection
        .write()
        .execute(
            "DELETE FROM idempotency WHERE operation_id=?1",
            [smallclaims::store::opaque_cache_key(
                "quick-agent-response:retention-quick",
            )],
        )
        .unwrap();
    let recovered = quick_agent(&state, retention_quick_request(root.path()), "codex")
        .await
        .unwrap();
    assert!(recovered.response_reconstructed);
    assert_eq!(recovered.event_cursor, original.event_cursor);
    assert_eq!(state.store.index().unwrap(), index);
    let repeated = quick_agent(&state, retention_quick_request(root.path()), "codex")
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(recovered).unwrap(),
        serde_json::to_value(repeated).unwrap()
    );
    assert_eq!(
        state
            .store
            .claims_for("agent/node.retention-worker", Some("intent.desired"))
            .unwrap()
            .len(),
        1
    );
}

fn retention_event_query(after: u64, owner: Option<&str>, limit: usize) -> EventQuery {
    EventQuery {
        after,
        subject: None,
        owner_run: owner.map(str::to_owned),
        wait: Some(false),
        timeout_ms: None,
        limit: Some(limit),
    }
}

#[tokio::test]
async fn event_page_owner_filter_preserves_populated_membership_and_scanned_cursor() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let subject_a = "agent/node.event-page-a";
    let subject_b = "agent/node.event-page-b";
    let mut intent = crate::graph::parse_test_intent(
        &format!(
            "version 2\n\
                 agent \"event-page-a\" {{ command \"true\" }}\n\
                 agent \"event-page-b\" {{ command \"true\" }}\n\
                 mission \"event-page-owner-a\" state=\"ready\" {{\n\
                   goal \"Own the first invented agent.\"\n\
                   step \"work\" {{ assigned-to \"{subject_a}\" }}\n\
                 }}\n\
                 mission \"event-page-owner-b\" state=\"ready\" {{\n\
                   goal \"Own the second invented agent.\"\n\
                   step \"work\" {{ assigned-to \"{subject_b}\" }}\n\
                 }}\n"
        ),
        "node",
    )
    .unwrap();
    roster_fixture_apply(&state.store, &intent, "event-page-owners-initial");
    let runs = ["event-page-owner-a", "event-page-owner-b"]
        .into_iter()
        .map(|mission| {
            state
                .store
                .create_mission_run(&MissionRunRequest {
                    mission: mission.into(),
                    revision: None,
                    workspace: root.path().display().to_string(),
                    requester: Some("person/test".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("event-page-run-{mission}"),
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    for (subject, run) in [(subject_a, &runs[0]), (subject_b, &runs[1])] {
        let desired = intent.subjects.get_mut(subject).unwrap();
        desired.owner_run = Some(run.subject.clone());
        desired.owner_generation = Some(run.generation.clone());
        desired.owner_step = Some(run.steps[0].subject.clone());
    }
    roster_fixture_apply(&state.store, &intent, "event-page-owners-set");
    let owned_a = state
        .store
        .desired_subjects_for_owner_run(&runs[0].subject)
        .unwrap();
    let owned_b = state
        .store
        .desired_subjects_for_owner_run(&runs[1].subject)
        .unwrap();
    assert!(owned_a.iter().any(|desired| desired.subject == subject_a));
    assert!(!owned_a.iter().any(|desired| desired.subject == subject_b));
    assert!(owned_b.iter().any(|desired| desired.subject == subject_b));
    assert!(!owned_b.iter().any(|desired| desired.subject == subject_a));
    let (_, setup_cursor) = state.store.event_bounds().unwrap();

    let accepted = [subject_b, subject_b, subject_a, subject_a, subject_b]
        .into_iter()
        .enumerate()
        .map(|(number, subject)| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "agent.presence".into(),
                    actor: Some(subject.into()),
                    fields: serde_json::from_value(json!({
                        "presence": "available",
                        "reason": format!("invented event {number}"),
                    }))
                    .unwrap(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("event-page-presence-{number}")),
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    for subject in [subject_a, subject_b] {
        let admitted = state.store.claims_for(subject, Some("agent.presence")).unwrap();
        let expected_ids = accepted
            .iter()
            .filter(|claim| claim.subject == subject)
            .map(|claim| &claim.id)
            .collect::<Vec<_>>();
        assert_eq!(admitted.iter().map(|claim| &claim.id).collect::<Vec<_>>(), expected_ids);
    }

    let app = router(state.clone());
    let path = |after| {
        format!(
            "/v1/events/page?after={after}&owner_run={}&limit=2&wait=false",
            runs[0].subject.replace('/', "%2F")
        )
    };
    let (status, first) = get_request(app.clone(), &path(setup_cursor)).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let first = first.get("value").unwrap_or(&first);
    assert_eq!(first["items"], json!([]));
    assert_eq!(first["next_after"], json!(accepted[1].store_index));
    assert_eq!(first["has_more"], json!(true));

    let (status, second) = get_request(app.clone(), &path(accepted[1].store_index)).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let second = second.get("value").unwrap_or(&second);
    let expected_a = accepted[2..4]
        .iter()
        .map(|claim| {
            json!({
                "store_index": claim.store_index,
                "kind": &claim.kind,
                "subject": &claim.subject,
                "body": &claim.body,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(second["items"], json!(expected_a));
    assert_eq!(second["next_after"], json!(accepted[3].store_index));
    assert_eq!(second["has_more"], json!(true));

    let (status, third) = get_request(app.clone(), &path(accepted[3].store_index)).await;
    assert_eq!(status, StatusCode::OK, "{third}");
    let third = third.get("value").unwrap_or(&third);
    assert_eq!(third["items"], json!([]));
    assert_eq!(third["next_after"], json!(accepted[4].store_index));
    assert_eq!(third["has_more"], json!(false));
    assert!(third["frontier"].as_u64().unwrap() >= accepted[4].store_index);

    let (status, done) = get_request(app.clone(), &path(accepted[4].store_index)).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let done = done.get("value").unwrap_or(&done);
    assert_eq!(done["items"], json!([]));
    assert_eq!(done["next_after"], Value::Null);
    assert_eq!(done["has_more"], json!(false));

    // Read the original five-event window through the same router before and
    // after ownership changes. New apply events lie after this raw scan.
    let owner_window = |owner: &str| {
        format!(
            "/v1/events/page?after={setup_cursor}&owner_run={}&limit=5&wait=false",
            owner.replace('/', "%2F")
        )
    };
    let unrelated_path = format!(
        "{}&subject={}",
        owner_window(&runs[1].subject),
        subject_b.replace('/', "%2F")
    );
    let expected_all = accepted.iter().map(|claim| json!({
        "store_index": claim.store_index,
        "kind": &claim.kind,
        "subject": &claim.subject,
        "body": &claim.body,
    })).collect::<Vec<_>>();
    let expected_b = expected_all.iter()
        .filter(|event| event["subject"].as_str() == Some(subject_b))
        .cloned().collect::<Vec<_>>();
    let (status, before_b) = get_request(app.clone(), &owner_window(&runs[1].subject)).await;
    assert_eq!(status, StatusCode::OK, "{before_b}");
    let before_b = before_b.get("value").unwrap_or(&before_b);
    assert_eq!(before_b["items"], json!(expected_b));
    let (status, unrelated_before) = get_request(app.clone(), &unrelated_path).await;
    assert_eq!(status, StatusCode::OK, "{unrelated_before}");
    let unrelated_before = unrelated_before.get("value").unwrap_or(&unrelated_before);
    assert_eq!(unrelated_before["items"], json!(expected_b));
    assert_eq!(unrelated_before["next_after"], json!(accepted[4].store_index));

    let desired = intent.subjects.get_mut(subject_a).unwrap();
    desired.owner_run = Some(runs[1].subject.clone());
    desired.owner_generation = Some(runs[1].generation.clone());
    desired.owner_step = Some(runs[1].steps[0].subject.clone());
    roster_fixture_apply(&state.store, &intent, "event-page-owner-reassigned");
    assert!(!state.store.desired_subjects_for_owner_run(&runs[0].subject).unwrap()
        .iter().any(|desired| desired.subject == subject_a));
    let reassigned = state.store.desired_subjects_for_owner_run(&runs[1].subject).unwrap();
    assert!(reassigned.iter().any(|desired| desired.subject == subject_a));
    assert!(reassigned.iter().any(|desired| desired.subject == subject_b));

    let (status, old_owner) = get_request(app.clone(), &owner_window(&runs[0].subject)).await;
    assert_eq!(status, StatusCode::OK, "{old_owner}");
    let old_owner = old_owner.get("value").unwrap_or(&old_owner);
    assert_eq!(old_owner["items"], json!([]));
    assert_eq!(old_owner["next_after"], json!(accepted[4].store_index));
    let (status, new_owner) = get_request(app.clone(), &owner_window(&runs[1].subject)).await;
    assert_eq!(status, StatusCode::OK, "{new_owner}");
    let new_owner = new_owner.get("value").unwrap_or(&new_owner);
    assert_eq!(new_owner["items"], json!(expected_all));
    assert_eq!(new_owner["next_after"], json!(accepted[4].store_index));
    let (status, unrelated_after) = get_request(app.clone(), &unrelated_path).await;
    assert_eq!(status, StatusCode::OK, "{unrelated_after}");
    let unrelated_after = unrelated_after.get("value").unwrap_or(&unrelated_after);
    assert_eq!(unrelated_after["items"], unrelated_before["items"]);
    assert_eq!(unrelated_after["next_after"], unrelated_before["next_after"]);

    let desired = intent.subjects.get_mut(subject_a).unwrap();
    desired.owner_run = Some(runs[0].subject.clone());
    desired.owner_generation = Some(runs[0].generation.clone());
    desired.owner_step = Some(runs[0].steps[0].subject.clone());
    roster_fixture_apply(&state.store, &intent, "event-page-owner-restored");
    assert!(state.store.desired_subjects_for_owner_run(&runs[0].subject).unwrap()
        .iter().any(|desired| desired.subject == subject_a));
    assert!(!state.store.desired_subjects_for_owner_run(&runs[1].subject).unwrap()
        .iter().any(|desired| desired.subject == subject_a));
    let (status, restored_a) = get_request(app.clone(), &owner_window(&runs[0].subject)).await;
    assert_eq!(status, StatusCode::OK, "{restored_a}");
    let restored_a = restored_a.get("value").unwrap_or(&restored_a);
    assert_eq!(restored_a["items"], second["items"]);
    let (status, restored_b) = get_request(app, &owner_window(&runs[1].subject)).await;
    assert_eq!(status, StatusCode::OK, "{restored_b}");
    let restored_b = restored_b.get("value").unwrap_or(&restored_b);
    assert_eq!(restored_b["items"], before_b["items"]);
}

#[tokio::test]
async fn retention_event_http_envelope_preserves_pacing_continuation_and_410() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let mut indexes = Vec::new();
    for subject in ["custom/test/other", "custom/test/selected"] {
        indexes.push(
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "custom.test.recorded".into(),
                    actor: None,
                    fields: BTreeMap::new(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
                .store_index,
        );
    }
    let app = router(state.clone());
    let started = tokio::time::Instant::now();
    let (status, empty) = get_request(
        app.clone(),
        "/v1/events/page?after=0&subject=custom%2Ftest%2Fselected&limit=1&wait=true&timeout_ms=40",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert!(started.elapsed() >= Duration::from_millis(40));
    assert!(started.elapsed() < Duration::from_millis(500));
    let page = empty.get("value").unwrap_or(&empty);
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["next_after"], json!(indexes[0]));
    assert_eq!(page["has_more"], json!(true));

    let (status, next) = get_request(app.clone(), &format!(
        "/v1/events/page?after={}&subject=custom%2Ftest%2Fselected&limit=1&wait=true&timeout_ms=40",
        indexes[0]
    )).await;
    assert_eq!(status, StatusCode::OK, "{next}");
    let page = next.get("value").unwrap_or(&next);
    assert_eq!(page["items"][0]["store_index"], json!(indexes[1]));
    assert_eq!(page["next_after"], json!(indexes[1]));

    state.store.connection.write().execute(
        "INSERT INTO meta(key,value) VALUES('event_resume_floor',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [indexes[1].to_string()],
    ).unwrap();
    let (status, gap) = get_request(
        app,
        &format!(
            "/v1/events/page?after={}&wait=true&timeout_ms=30000",
            indexes[0]
        ),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{gap}");
    assert_eq!(gap["code"], json!("cursor-gap"));
    assert_eq!(gap["details"]["full_resync"], json!(true));
    assert_eq!(gap["details"]["resume_floor"], json!(indexes[1]));
}

#[tokio::test]
async fn retention_event_pages_advance_by_scanned_rows_and_refuse_stale_cursors() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let mut indexes = Vec::new();
    for number in 0..7 {
        indexes.push(
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("custom/test/page-{number}"),
                    kind: "custom.test.recorded".into(),
                    actor: None,
                    fields: BTreeMap::new(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
                .store_index,
        );
    }
    let empty = read_event_page(
        &state,
        &retention_event_query(0, Some("mission-run/missing"), 3),
    )
    .await
    .unwrap();
    assert!(empty.items.is_empty());
    assert_eq!(empty.next_after, Some(indexes[2]));
    assert!(empty.has_more);
    let second = read_event_page(&state, &retention_event_query(indexes[2], None, 3))
        .await
        .unwrap();
    assert_eq!(
        second
            .items
            .iter()
            .map(|event| event.store_index)
            .collect::<Vec<_>>(),
        indexes[3..6]
    );
    assert_eq!(second.next_after, Some(indexes[5]));
    assert!(second.has_more);
    let last = read_event_page(&state, &retention_event_query(indexes[5], None, 3))
        .await
        .unwrap();
    assert_eq!(last.next_after, Some(indexes[6]));
    assert!(!last.has_more);
    let done = read_event_page(&state, &retention_event_query(indexes[6], None, 3))
        .await
        .unwrap();
    assert!(done.items.is_empty());
    assert_eq!(done.next_after, None);
    assert!(!done.has_more);
    state.store.prune_events_before(indexes[3]).unwrap();
    for _ in 0..2 {
        let error = read_event_page(&state, &retention_event_query(0, None, 3))
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, StatusCode::GONE);
        assert_eq!(error.code, "cursor-gap");
        assert_eq!(
            error.details.get("resume_floor"),
            Some(&json!(indexes[3] - 1))
        );
        assert_eq!(error.details.get("full_resync"), Some(&json!(true)));
    }
}

#[tokio::test]
async fn retention_global_empty_wait_pages_are_paced_without_losing_eligible_events() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let path = root.path().join("pending.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let append = |store: &Store, subject: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.test.recorded".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
            .store_index
    };
    let floor = append(&store, "custom/test/old");
    store.connection.write().execute_batch(
        "BEGIN;
             CREATE TABLE legacy_fixture_events(store_index INTEGER PRIMARY KEY,kind TEXT,subject TEXT,body TEXT);
             INSERT INTO legacy_fixture_events SELECT * FROM events;
             DROP VIEW events;
             ALTER TABLE legacy_fixture_events RENAME TO events;
             DROP TABLE event_positions;
             PRAGMA user_version=16;
             COMMIT;"
    ).unwrap();
    drop(store);
    state.store = Arc::new(Store::open(&path, "node").unwrap());
    let skipped = append(&state.store, "custom/test/other");
    let eligible = append(&state.store, "custom/test/selected");
    let mut query = retention_event_query(floor, None, 1);
    query.subject = Some("custom/test/selected".into());
    query.wait = Some(true);
    query.timeout_ms = Some(1_000);
    let started = tokio::time::Instant::now();
    let empty = events_page(State(state.clone()), Query(query))
        .await
        .unwrap()
        .0;
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "an empty continuation must not invite immediate repoll"
    );
    assert!(empty.items.is_empty() && empty.has_more);
    assert_eq!(empty.next_after, Some(skipped));
    let mut next = retention_event_query(skipped, None, 1);
    next.subject = Some("custom/test/selected".into());
    let pending = read_event_page(&state, &next).await.unwrap();
    assert_eq!(pending.items[0].store_index, eligible);
    assert_eq!(pending.next_after, Some(eligible));
    let report = crate::maintenance::migrate_event_payloads(state.store.clone())
        .await
        .unwrap();
    assert!(report.pending_at_start && report.completed);
    assert!(!state.store.event_payload_migration_pending().unwrap());
    let complete = read_event_page(&state, &next).await.unwrap();
    assert_eq!(
        serde_json::to_value(pending).unwrap(),
        serde_json::to_value(complete).unwrap()
    );
    let mut from_floor = next;
    from_floor.after = floor;
    let global = read_event_page(&state, &from_floor).await.unwrap();
    assert!(global.items.is_empty() && global.has_more);
    assert_eq!(global.next_after, Some(skipped));
    from_floor.after = global.next_after.unwrap();
    let complete_continuation = read_event_page(&state, &from_floor).await.unwrap();
    assert_eq!(complete_continuation.items[0].store_index, eligible);
    assert_eq!(complete_continuation.next_after, Some(eligible));
    assert!(!complete_continuation.has_more);
}

#[tokio::test]
async fn retention_empty_wait_page_pacing_respects_the_caller_deadline() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state
        .store
        .append_claim(&ClaimInput {
            subject: "custom/test/other".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let mut query = retention_event_query(0, Some("mission-run/missing"), 1);
    query.wait = Some(true);
    query.timeout_ms = Some(40);
    let started = tokio::time::Instant::now();
    let page = events_page(State(state), Query(query)).await.unwrap().0;
    assert!(page.items.is_empty() && page.next_after.is_some());
    assert!(started.elapsed() >= Duration::from_millis(40));
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "250ms pacing is capped by the requested timeout"
    );
}

#[tokio::test]
async fn retention_event_upgrade_resyncs_lagging_cursors_and_continues_the_frontier() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let snapshot_before_upgrade = new_client_snapshot(&state);
    assert!(
        read_event_page(&state, &retention_event_query(0, None, 3))
            .await
            .is_ok()
    );
    let path = root.path().join("legacy.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let record = store
        .append_claim(&ClaimInput {
            subject: "custom/test/upgrade".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    store.connection.write().execute_batch(
        "BEGIN;
             CREATE TABLE legacy_fixture_events(store_index INTEGER PRIMARY KEY,kind TEXT NOT NULL,subject TEXT NOT NULL,body TEXT NOT NULL);
             INSERT INTO legacy_fixture_events SELECT store_index,kind,subject,body FROM events;
             DROP VIEW events;
             ALTER TABLE legacy_fixture_events RENAME TO events;
             DROP TABLE event_positions;
             PRAGMA user_version=16;
             COMMIT;"
    ).unwrap();
    drop(store);
    state.store = Arc::new(Store::open(&path, "node").unwrap());
    let capabilities = client_capabilities(
        State(state.clone()),
        Extension(snapshot_before_upgrade),
        Extension(client_v0::ClientSession::for_tests(
            "person/alex",
            "person/alex",
            "unix",
        )),
    )
    .await
    .unwrap()
    .0;
    let frontier_cursor = format!("event-cursor/{}/{}", state.node, record.store_index);
    assert_eq!(capabilities["event_cursor"], frontier_cursor);
    assert_eq!(capabilities["oldest_event_cursor"], frontier_cursor);
    let gap = read_event_page(&state, &retention_event_query(0, None, 3))
        .await
        .err()
        .unwrap();
    assert_eq!(gap.status, StatusCode::GONE);
    assert_eq!(
        gap.details.get("resume_floor"),
        Some(&json!(record.store_index))
    );
    let caught_up =
        read_event_page(&state, &retention_event_query(record.store_index, None, 3))
            .await
            .unwrap();
    assert!(caught_up.items.is_empty());
    let next = state
        .store
        .append_claim(&ClaimInput {
            subject: "custom/test/next".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let continued =
        read_event_page(&state, &retention_event_query(record.store_index, None, 3))
            .await
            .unwrap();
    assert_eq!(continued.items[0].store_index, next.store_index);
    assert_eq!(continued.next_after, Some(next.store_index));
}

#[tokio::test]
async fn retention_event_timeout_rereads_a_new_unsignaled_floor() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let waiter_state = state.clone();
    let waiter = tokio::spawn(async move {
        let mut query = retention_event_query(0, None, 3);
        query.wait = Some(true);
        query.timeout_ms = Some(200);
        events_page(State(waiter_state), Query(query)).await
    });
    // Observe registration, then leave time for its initial empty read. No commit signal
    // follows below, reproducing a migration/checkpoint floor change during the wait.
    while state.event_notify.receiver_count() == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiter.is_finished());
    let index = state
        .store
        .append_claim(&ClaimInput {
            subject: "custom/test/migration-floor".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
        .store_index;
    state.store.prune_events_before(index + 1).unwrap();
    let error = waiter.await.unwrap().err().unwrap();
    assert_eq!(error.status, StatusCode::GONE);
    assert_eq!(error.details.get("resume_floor"), Some(&json!(index)));
}

#[tokio::test]
async fn an_event_wait_ignores_a_wake_without_a_matching_event() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let waiter_state = state.clone();
    let waiter = tokio::spawn(async move {
        let response = events(
            State(waiter_state),
            Query(EventQuery {
                after: 0,
                subject: None,
                owner_run: None,
                wait: Some(true),
                timeout_ms: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Vec<EventRecord>>(&bytes).unwrap()
    });
    tokio::time::sleep(Duration::from_millis(10)).await;

    signal_changed(&state);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!waiter.is_finished(), "an empty wake ended the event wait");

    state
        .store
        .append_claim(&ClaimInput {
            subject: "custom/test/event".into(),
            kind: "custom.test.recorded".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    signal_changed(&state);
    let observed = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the event wait did not finish")
        .unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].kind, "custom.test.recorded");
}

#[test]
fn agents_list_the_subagents_their_harness_runs_now() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = format!(
        "version 2\nagent \"busy\" {{ workspace {0:?}; command \"true\" }}\n\
             agent \"quiet\" {{ workspace {0:?}; command \"true\" }}\n",
        root.path().display().to_string()
    );
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "subagents")
        .unwrap();
    let now = client_now_ms() as u64;
    let claim = |kind: &str, id: &str, fields: &[(&str, Value)]| {
        let mut all = BTreeMap::from([("subagent_id".to_owned(), json!(id))]);
        for (name, value) in fields {
            all.insert((*name).to_owned(), value.clone());
        }
        state
            .store
            .append_client_claim(&ClaimInput {
                subject: "agent/node.busy".into(),
                kind: kind.into(),
                actor: Some("agent/node.busy".into()),
                fields: all,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    let appear = |id: &str, lease: u64| {
        claim(
            "subagent.appeared",
            id,
            &[
                ("driver", json!("claude")),
                ("incarnation_id", json!("inc-1")),
                ("subagent_type", json!("Explore")),
                ("description", json!("map the code")),
                ("session_id", json!("session-1")),
                ("step_run", json!("step-run/run-1/build")),
                ("started_at_unix_ms", json!(1_790_931_600_000_u64)),
                ("lease_expires_at_unix_ms", json!(lease)),
            ],
        )
    };
    appear("running", now + 600_000);
    appear("lapsed", now - 1);
    appear("finished", now + 600_000);
    claim(
        "subagent.ended",
        "finished",
        &[("outcome", json!("completed"))],
    );

    let agents =
        client_agent_resources(&state.store, false, "now", state.store.index().unwrap())
            .unwrap();
    let agent = |id: &str| agents.iter().find(|agent| agent["id"] == id).unwrap();
    assert_eq!(
        agent("agent/node.busy")["subagents"],
        json!([{
            "id": "running", "subagent_type": "Explore", "description": "map the code",
            "driver": "claude", "session_id": "session-1",
            "work_id": "step-run/run-1/build",
            "started_at": "2026-10-02T09:00:00.000Z",
            "lease_expires_at": client_timestamp(u128::from(now + 600_000)),
        }])
    );
    assert_eq!(agent("agent/node.quiet")["subagents"], json!([]));
    // Clients read it as the typed field.
    let typed: st3_client::Resource =
        serde_json::from_value(agent("agent/node.busy").clone()).unwrap();
    let st3_client::Resource::Agent(typed) = typed else {
        panic!("an agent resource");
    };
    assert_eq!(typed.subagents.len(), 1);
}

#[test]
fn member_faults_are_visible_in_agents_and_doctor_until_recovery() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = format!(
        r#"version 2
agent "bad" {{ workspace {:?}; command "true" }}
agent "good" {{ workspace {:?}; command "true" }}
"#,
        root.path().display().to_string(),
        root.path().display().to_string()
    );
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "fault-surface")
        .unwrap();
    for status in ["failed", "resolved"] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/node.bad".into(),
                kind: "runtime.reconcile-decision".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("key".into(), Value::String("member-reconcile".into())),
                    (
                        "decision".into(),
                        Value::String(
                            if status == "failed" {
                                "member-fault"
                            } else {
                                "member-recovered"
                            }
                            .into(),
                        ),
                    ),
                    (
                        "reason".into(),
                        Value::String(
                            "render refuses to change tracked file .claude/settings.local.json"
                                .into(),
                        ),
                    ),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let agents =
            client_agent_resources(&state.store, false, "now", state.store.index().unwrap())
                .unwrap();
        let bad = agents.iter().find(|a| a["id"] == "agent/node.bad").unwrap();
        let good = agents
            .iter()
            .find(|a| a["id"] == "agent/node.good")
            .unwrap();
        assert!(good["fault"].is_null());
        assert_ne!(good["state"], "failed");
        let report = doctor_report(&state).unwrap().0;
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "member-reconcile/agent/node.bad");
        if status == "failed" {
            assert_eq!(bad["state"], "failed");
            assert!(
                bad["fault"]
                    .as_str()
                    .unwrap()
                    .contains("settings.local.json")
            );
            assert_eq!(check.unwrap().status, "fail");
            assert!(check.unwrap().message.contains("settings.local.json"));
        } else {
            assert!(bad["fault"].is_null());
            assert_ne!(bad["state"], "failed");
            assert!(check.is_none());
        }
    }
}

#[tokio::test]
async fn doctor_flags_local_foreign_placement_and_exempts_actual_remote_runtimes() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = format!(
        "version 2\nagent \"example/orphan\" {{ host \"orchid\"; command \"true\"; workspace {:?} }}\nagent \"example/remote\" {{ host \"fern\"; command \"true\"; workspace {:?} }}\n",
        root.path().display().to_string(),
        root.path().display().to_string()
    );
    let intent = crate::graph::parse_intent(&source, "node").unwrap();
    state
        .store
        .apply_internal(&intent, "doctor-orphan")
        .unwrap();
    let orphan = state
        .store
        .desired_subject_with_writer("agent/example/orphan")
        .unwrap()
        .unwrap()
        .0
        .member
        .unwrap();
    fs::create_dir_all(&state.pty_root).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(
        state.pty_root.join(format!("{}.sock", orphan.runtime_id)),
    )
    .unwrap();
    fs::write(
        state.pty_root.join(format!("{}.pid", orphan.runtime_id)),
        std::process::id().to_string(),
    )
    .unwrap();
    fs::write(state.pty_root.join(format!("{}.json", orphan.runtime_id)), json!({"createdAt":"2026-10-01T00:00:00Z","tags":{"st3.subject":"agent/example/orphan"}}).to_string()).unwrap();
    let report = doctor_report(&state).unwrap().0;
    let ownership = report
        .checks
        .iter()
        .find(|c| c.name == "runtime-ownership")
        .unwrap();
    assert_eq!(ownership.status, "fail", "{ownership:?}");
    assert!(
        ownership.message.contains("host/orchid"),
        "{}",
        ownership.message
    );
    assert!(!ownership.message.contains("fern"), "{}", ownership.message);
    let drift = report
        .checks
        .iter()
        .find(|c| c.name == "runtime-drift")
        .unwrap();
    assert_eq!(drift.status, "warn");
    assert!(drift.message.contains(&orphan.runtime_id));
    fs::remove_file(state.pty_root.join(format!("{}.pid", orphan.runtime_id))).unwrap();
    drop(_socket);
    fs::remove_file(state.pty_root.join(format!("{}.sock", orphan.runtime_id))).unwrap();
    fs::remove_file(state.pty_root.join(format!("{}.json", orphan.runtime_id))).unwrap();
    let report = doctor_report(&state).unwrap().0;
    assert_eq!(
        report
            .checks
            .iter()
            .find(|c| c.name == "runtime-ownership")
            .unwrap()
            .status,
        "pass",
        "remote placement alone is valid"
    );
}

#[tokio::test]
async fn health_build_commit_matches_version_revision() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let (status, health) = get_request(app, "/v1/health").await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    let commit = health["build"]["commit"].as_str().unwrap();
    assert_eq!(commit, st_drivers::version::build_commit());
    let version = st_drivers::version::machine_version();
    let revision = version.split_once('+').unwrap().1;
    let revision = revision.strip_prefix("local.").unwrap_or(revision);
    let revision = revision.strip_suffix(".dirty").unwrap_or(revision);
    let revision = revision.strip_suffix("-dirty").unwrap_or(revision);
    if revision == "dev" {
        assert_eq!(commit, "unknown");
    } else {
        assert!(commit.starts_with(revision), "{version}: {commit}");
    }
}

#[tokio::test]
async fn health_and_doctor_report_runtime_metadata() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let (status, health) = get_request(app.clone(), "/v1/health").await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert!(health["isolation"].is_string());

    let (status, doctor) = get_request(app, "/v1/doctor").await;
    assert_eq!(status, StatusCode::OK, "{doctor}");
    assert!(doctor["status"].is_string());
    assert!(
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "runtime-ownership")
    );
    let disk = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "disk-space")
        .expect("doctor reports disk space");
    assert!(
        disk["message"].as_str().unwrap().contains("GiB free"),
        "{disk}"
    );
}

#[tokio::test]
async fn replication_worker_status_changes_only_local_memory() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.configured_peers = vec!["cedar".into()];
    let index = state.store.index().unwrap();
    let app = router(state.clone());
    let attempt = smallclaims::store::now_ms();
    let retry = attempt + 30_000;
    let (status, response) = json_request(app.clone(), "/v1/internal/replication/worker-status", json!({
        "peer": "cedar", "worker": {"phase": "backoff",
            "last_attempt_at_unix_ms": attempt, "next_retry_at_unix_ms": retry}
    })).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["changed"], false);
    let (status, response) = get_request(app, "/v1/replication/status").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["peers"][0]["worker"]["phase"], "backoff");
    assert_eq!(response["peers"][0]["worker"]["last_attempt_at_unix_ms"], json!(attempt));
    assert_eq!(response["peers"][0]["worker"]["next_retry_at_unix_ms"], json!(retry));
    assert!(response["peers"][0]["last_error"].is_null());
    assert_eq!(state.store.index().unwrap(), index);
}

#[tokio::test]
async fn idle_replication_receipts_do_not_wake_client_streams() {
    const FLEET: &str = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.configured_peers = vec!["cedar".into()];
    state.store.bind_fleet(FLEET).unwrap();
    state
        .store
        .record_transport_observation("cedar", "up", None, None)
        .unwrap();
    let source = Store::open_memory("cedar").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let exchange = source.export_replication_summary(FLEET).unwrap();
    let events = state.event_notify.subscribe();
    let app = router(state.clone());
    for carrier in ["fabric", "fabric", "loopback"] {
        let (status, response) = json_request(
            app.clone(),
            "/v1/internal/replication/receive",
            json!({
                "peer": "cedar", "fleet_id": FLEET, "exchange": exchange,
                "round_trip_ms": 10, "carrier": carrier
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["changed"], false, "{response}");
        assert!(
            !events.has_changed().unwrap(),
            "idle receipts have no visible client notice"
        );
    }
    // Leaving the notice set is visible even if no envelope or claim changed.
    state
        .store
        .replication_sync
        .lock()
        .unwrap()
        .get_mut("cedar")
        .unwrap()
        .observe(0, Some((1_000, 0)), smallclaims::store::now_ms());
    assert!(state.store.replication_peer_in_sync_notice("cedar"));
    let (status, response) = json_request(
        app,
        "/v1/internal/replication/receive",
        json!({
            "peer": "cedar", "fleet_id": FLEET, "exchange": exchange,
            "round_trip_ms": 10, "carrier": "loopback"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["changed"], false);
    assert!(
        events.has_changed().unwrap(),
        "a disappearing sync notice must notify clients"
    );
}

#[tokio::test]
async fn replication_exchange_carrier_status_notifies_clients_without_replicating_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.configured_peers = vec!["cedar".into()];
    // Existing reachability is already unknown, so this refusal writes only local evidence.
    state
        .store
        .record_transport_observation("cedar", "unknown", None, None)
        .unwrap();
    let index = state.store.index().unwrap();
    state
        .store
        .replication_sync
        .lock()
        .unwrap()
        .entry("cedar".into())
        .or_default()
        .observe(0, Some((1_000, 0)), smallclaims::store::now_ms());
    let mut events = state.event_notify.subscribe();
    let app = router(state.clone());
    let (status, response) = json_request(app.clone(), "/v1/internal/replication/peer-failure", json!({
        "peer": "cedar", "status": "refused", "error": "service grant refused", "carrier": "fabric"
    })).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["changed"], false);
    assert!(
        events.has_changed().unwrap(),
        "local evidence must notify client streams"
    );
    events.borrow_and_update();
    let (status, response) = get_request(app.clone(), "/v1/replication/status").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let failed = &response["peers"][0]["last_failed_exchange"];
    assert_eq!(failed["carrier"], "fabric");
    assert_eq!(failed["fabric_route"], "unknown");
    assert!(failed["at_unix_ms"].as_u64().is_some());
    assert!(
        response["peers"][0]
            .get("last_successful_exchange")
            .is_none()
    );
    assert_eq!(state.store.index().unwrap(), index);
    assert!(!state.state_dir.join("replication.wake").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), state.notify.notified())
            .await
            .is_err()
    );
    let (status, _) = json_request(app.clone(), "/v1/internal/replication/peer-failure", json!({
        "peer": "cedar", "status": "refused", "error": "service grant refused", "carrier": "fabric"
    })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !events.has_changed().unwrap(),
        "an unchanged-carrier repeat stays quiet"
    );
    state
        .store
        .replication_sync
        .lock()
        .unwrap()
        .get_mut("cedar")
        .unwrap()
        .observe(0, Some((0, 0)), smallclaims::store::now_ms());
    let (status, _) = json_request(app.clone(), "/v1/internal/replication/peer-failure", json!({
        "peer": "cedar", "status": "refused", "error": "service grant refused", "carrier": "loopback"
    })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !events.has_changed().unwrap(),
        "an idle peer's carrier is not in client notices"
    );
    // Old-worker/liveness reports have no dial evidence and leave the record intact.
    let (status, _) = json_request(
        app,
        "/v1/internal/replication/peer-failure",
        json!({
            "peer": "cedar", "status": "refused", "error": "service grant refused"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!events.has_changed().unwrap());
}

#[tokio::test]
async fn doctor_and_replication_status_explain_fabric_refusals_without_errors() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.fleet_id = Some("94cd11ba-c582-4558-9c84-c3bda922eb6d".into());
    state.configured_peers = vec!["cobalt".into()];
    let app = router(state.clone());
    let (status, recorded) = json_request(app.clone(), "/v1/internal/replication/peer-failure", json!({
        "peer": "cobalt", "status": "refused", "error": "refused by that member's Fabric grants (service st3-peer-v1); replication can continue through other members"
    })).await;
    assert_eq!(status, StatusCode::OK, "{recorded}");
    let (status, replication) = get_request(app.clone(), "/v1/replication/status").await;
    assert_eq!(status, StatusCode::OK, "{replication}");
    assert_eq!(replication["peers"][0]["status"], "refused");
    assert!(replication["peers"][0]["last_error"].is_null());
    assert!(
        replication["peers"][0]["refusal_reason"]
            .as_str()
            .unwrap()
            .contains("Fabric grants")
    );
    let (status, doctor) = get_request(app, "/v1/doctor").await;
    assert_eq!(status, StatusCode::OK, "{doctor}");
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "replication")
        .unwrap();
    assert_eq!(check["status"], "pass", "{check}");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("cobalt: refused by that member's Fabric grants")
    );
}

/// Two members apart, as during a partition, can each accept the same idempotency key for
/// a different request (#1026). Both claims stand once they meet; doctor says which, and a
/// retry with the key is refused instead of answering with either.
#[tokio::test]
async fn doctor_names_an_idempotency_key_two_members_used_for_different_requests() {
    const FLEET: &str = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state.store.bind_fleet(FLEET).unwrap();
    let note = |text: &str| ClaimInput {
        subject: "custom/partition/note".into(),
        kind: "custom.partition.note".into(),
        actor: Some("person/tester".into()),
        fields: BTreeMap::from([("text".into(), Value::String(text.into()))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("partition-key".into()),
    };
    let app = router(state.clone());
    let (_, doctor) = get_request(app.clone(), "/v1/doctor").await;
    let check = |doctor: &Value| {
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "idempotency-keys")
            .cloned()
            .unwrap()
    };
    assert_eq!(check(&doctor)["status"], "pass");

    state.store.append_claim(&note("written here")).unwrap();
    let other = Store::open_memory("birch").unwrap();
    other.bind_fleet(FLEET).unwrap();
    other.append_claim(&note("written there")).unwrap();
    let exchange = other
        .export_replication_exchange(FLEET, &state.store.replication_inventory().unwrap())
        .unwrap();
    state
        .store
        .receive_replication_exchange("birch", FLEET, &exchange)
        .unwrap();
    state.store.validate_replication_backlog().unwrap();
    state.store.project_replication_backlog().unwrap();

    let (_, doctor) = get_request(app, "/v1/doctor").await;
    let check = check(&doctor);
    assert_eq!(check["status"], "warn", "{check}");
    let message = check["message"].as_str().unwrap();
    assert!(message.starts_with("1 idempotency key was used"), "{check}");
    assert!(
        message.contains("custom/partition/note by birch"),
        "{check}"
    );
    assert!(
        message.contains(&format!(
            "custom/partition/note by {}",
            state.store.origin()
        )),
        "{check}"
    );
    let retry = state.store.append_claim(&note("written here")).unwrap_err();
    assert_eq!(retry.code, "idempotency-conflict", "{retry:?}");
}

#[tokio::test]
async fn first_readiness_fault_is_visible_in_doctor_and_mission_clients() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let intent = parse_intent(r#"version 2
mission "orchid" state="ready" { goal "Expose a scheduler wait."; step "work" { agentless } }
"#, "node").unwrap();
    state.store.apply_internal(&intent, "scheduler-visibility").unwrap();
    let run = state.store.create_mission_run(&MissionRunRequest {
        mission: "orchid".into(), revision: None, workspace: root.path().display().to_string(),
        requester: None, mode: None, inputs: BTreeMap::new(), idempotency_key: "orchid-run".into(),
    }).unwrap();
    let reason = format!("{} waited 180000ms with satisfied readiness predicates", run.subject);
    let fault = |status: &str| ClaimInput {
        subject: run.subject.clone(), kind: "reconcile.fault".into(), actor: None,
        fields: BTreeMap::from([
            ("scope".into(), json!(crate::reconcile::FIRST_READINESS_FAULT_SCOPE)),
            ("status".into(), json!(status)), ("reason".into(), json!(reason)),
        ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    };
    state.store.append_claim(&fault("faulted")).unwrap();
    let check = first_readiness_scheduler_check(&state.store, "node");
    assert_eq!(check.status, "warn");
    assert!(check.message.contains(&run.subject) && check.message.contains("180000ms"));
    let detail = client_v0::mission_resources(&state.store, state.store.index().unwrap(), false, Some("mission/orchid")).unwrap();
    assert_eq!(detail[0]["state"], "blocked");
    assert_eq!(detail[0]["run_details"][0]["blocker"]["reason"], reason);
    assert_eq!(detail[0]["run_details"][0]["blocker"]["scope"], crate::reconcile::FIRST_READINESS_FAULT_SCOPE);
    assert_eq!(detail[0]["run_details"][0]["steps"][0]["blocked_reason"], reason);
    let (_, cards) = get_request(router(state.clone()), "/v1/client/missions").await;
    assert_eq!(cards["items"][0]["state"], "blocked", "{cards}");
    assert_eq!(cards["items"][0]["run_details"][0]["blocker"]["reason"], reason);
    assert_eq!(cards["items"][0]["run_details"][0]["blocker"]["scope"], crate::reconcile::FIRST_READINESS_FAULT_SCOPE);
    assert_eq!(cards["items"][0]["run_details"][0]["steps"][0]["blocked_reason"], reason);
    state.store.append_claim(&fault("recovered")).unwrap();
    assert_eq!(first_readiness_scheduler_check(&state.store, "node").status, "pass");
    let detail = client_v0::mission_resources(&state.store, state.store.index().unwrap(), false, Some("mission/orchid")).unwrap();
    assert_ne!(detail[0]["state"], "blocked");
    assert!(detail[0]["run_details"][0]["blocker"].is_null());
}

#[test]
fn doctor_reports_missing_weekly_evidence_for_a_declared_account() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let intent = parse_intent(
        "version 2\naccount \"avery/one\" { provider \"anthropic\"; login \"/tmp/example-login\"; }\nagent \"worker\" { workspace \"/tmp\"; harness \"claude\" { account \"avery/one\"; }; }",
        state.store.origin(),
    ).unwrap();
    state.store.apply_internal(&intent, "missing-weekly-doctor").unwrap();
    let report = doctor_report(&state).unwrap().0;
    let check = report.checks.iter().find(|check| check.name == "account-limits").unwrap();
    assert_eq!(check.status, "warn");
    assert!(check.message.contains("account/avery/one: no weekly reading"));
    assert!(check.message.contains("not below the limit"));
}

#[test]
fn doctor_reports_waiting_claims_without_a_replication_fault() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.fleet_id = Some("fleet/waiting".into());
    state.configured_peers = vec!["alder".into()];
    let mut registry = st3_schema::registry().clone();
    registry.claims.remove("doc.bound").unwrap();
    state.store.set_claim_registry(registry);
    state.store.bind_fleet("fleet/waiting").unwrap();
    let source = Store::open_memory("alder").unwrap();
    source.bind_fleet("fleet/waiting").unwrap();
    source
        .put_document("doc/release", b"release", &None, "release")
        .unwrap();
    let exchange = source
        .export_replication_exchange(
            "fleet/waiting",
            &state.store.replication_inventory().unwrap(),
        )
        .unwrap();
    state
        .store
        .receive_replication_exchange("alder", "fleet/waiting", &exchange)
        .unwrap();
    assert_eq!(
        state.store.validate_replication_backlog().unwrap().unknown,
        1
    );
    assert!(state.store.project_replication_backlog().unwrap());
    let exchange = source.export_replication_summary("fleet/waiting").unwrap();
    state
        .store
        .receive_replication_exchange("alder", "fleet/waiting", &exchange)
        .unwrap();
    let report = doctor_report(&state).unwrap().0;
    let check = report
        .checks
        .iter()
        .find(|check| check.name == "replication")
        .unwrap();
    assert_eq!(check.status, "pass");
    assert!(check.message.contains("1 claims waiting for a newer build"));
    assert!(
        !report
            .checks
            .iter()
            .any(|check| check.name == "shared-projections")
    );
}

#[test]
fn doctor_names_shared_tables_that_differ_at_equal_inventory() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.fleet_id = Some("fleet/audit".into());
    state.configured_peers = vec!["alder".into()];
    state.store.bind_fleet("fleet/audit").unwrap();
    let source = Store::open_memory("alder").unwrap();
    source.bind_fleet("fleet/audit").unwrap();
    let original = source.export_replication_summary("fleet/audit").unwrap();
    let mut different = original.clone();
    different
        .projection_digests
        .insert("planning_sessions".into(), "different".into());
    state
        .store
        .receive_replication_exchange("alder", "fleet/audit", &different)
        .unwrap();
    let report = doctor_report(&state).unwrap().0;
    let check = report
        .checks
        .iter()
        .find(|check| check.name == "shared-projections")
        .unwrap();
    assert_eq!(check.status, "fail");
    assert!(check.message.contains("alder/planning_sessions"));
    state
        .store
        .receive_replication_exchange("alder", "fleet/audit", &original)
        .unwrap();
    assert!(
        !doctor_report(&state)
            .unwrap()
            .0
            .checks
            .iter()
            .any(|check| check.name == "shared-projections")
    );
}

#[test]
fn doctor_lists_attention_items_open_for_more_than_a_day() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for (key, title) in [
        ("first", "Renew the signing key"),
        ("second", "Pick a region"),
    ] {
        state
            .store
            .record_operational_failure(
                &format!("disk-{key}"),
                &AttentionRequest {
                    reviewer: "person/alex".into(),
                    title: title.into(),
                    reason: "A person needs to decide.".into(),
                    severity: "warning".into(),
                    targets: vec![format!("daemon/{key}")],
                    actor: "daemon/runtime".into(),
                    idempotency_key: key.into(),
                },
            )
            .unwrap();
    }
    assert!(state.store.attention_items(None).unwrap().is_empty());
    let items = doctor_attention_items(&state.store, client_now_ms()).unwrap();
    assert_eq!(items.len(), 2);
    let requested = items
        .iter()
        .map(|item| item.requested_at_unix_ms)
        .max()
        .unwrap();

    let fresh = stale_attention_check(&items, requested + 3_600_000);
    assert_eq!(fresh.status, "pass");
    let stale = stale_attention_check(&items, requested + 2 * 86_400_000 + 3 * 3_600_000);
    assert_eq!(
        (stale.name.as_str(), stale.status.as_str()),
        ("attention-age", "warn")
    );
    assert!(
        stale
            .message
            .starts_with("2 alerts and faults have been open for more than a day"),
        "{}",
        stale.message
    );
    assert!(
        stale
            .message
            .contains("daemon/first for no owning agent, open 2d 3h: Renew the signing key"),
        "{}",
        stale.message
    );
    let report = doctor_report(&state).unwrap().0;
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "attention-age" && check.status == "pass")
    );
}

#[test]
fn doctor_shows_what_spends_the_shared_github_budget() {
    use crate::resource::{
        GithubBudget, GithubBudgetReport, GithubSpenderReport, GithubUsageReport,
    };
    let now = 1_790_000_000_000_u128;
    let quiet = github_usage_checks(&GithubUsageReport::default(), now);
    assert_eq!(quiet.len(), 1);
    assert_eq!(
        (quiet[0].name.as_str(), quiet[0].status.as_str()),
        ("github-budget", "pass")
    );

    let spender = |name: &str, counted| GithubSpenderReport {
        spender: name.into(),
        sent: counted + 10,
        not_modified: 10,
        refused: 0,
        counted_last_hour: counted,
        last_sent_at_unix_ms: now - 5_000,
    };
    let usage = |remaining| GithubUsageReport {
        spenders: vec![
            spender("observer/orchid-listing", 40),
            spender("observer/lichen-ref", 2),
        ],
        budgets: vec![GithubBudgetReport {
            budget: GithubBudget {
                resource: "core".into(),
                limit: 5000,
                remaining,
                used: 5000 - remaining,
                reset_at_unix_ms: now + 600_000,
                reported_at_unix_ms: now - 5_000,
            },
            counted_here: 42,
        }],
    };
    let checks = github_usage_checks(&usage(4000), now);
    let names = checks
        .iter()
        .map(|check| (check.name.as_str(), check.status.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            ("github-budget/core", "pass"),
            ("github-requests/observer/orchid-listing", "pass"),
            ("github-requests/observer/lichen-ref", "pass"),
        ]
    );
    assert!(
        checks[0]
            .message
            .contains("observers on this host sent 42 of the 1000 counted in this window"),
        "{}",
        checks[0].message
    );
    assert!(
        checks[1]
            .message
            .starts_with("40 counted requests in the last hour; since the daemon started 50 sent, 10 not modified"),
        "{}",
        checks[1].message
    );
    // Under a tenth of the budget left warns until the window resets.
    assert_eq!(github_usage_checks(&usage(400), now)[0].status, "warn");
    assert_eq!(
        github_usage_checks(&usage(400), now + 600_000)[0].status,
        "pass"
    );
}

#[tokio::test]
async fn replication_receive_accepts_a_request_above_axums_default_body_limit() {
    let root = tempfile::tempdir().unwrap();
    let value = json!({
        "peer": "source",
        "fleet_id": "fleet",
        "exchange": {
            "peer": "source",
            "fleet_id": "fleet",
            "schema_digest": st3_schema::registry().digest(),
            "authority_digest": "",
            "graph_digest": "",
            "inventory": { "digest": "", "envelopes": [] },
            "envelopes": []
        }
    });
    let mut body = serde_json::to_vec(&value).unwrap();
    body.resize(2 * 1024 * 1024 + 1, b' ');
    let response = router(state(root.path()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/internal/replication/receive")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn replication_export_accepts_a_request_above_axums_default_body_limit() {
    let root = tempfile::tempdir().unwrap();
    let value = json!({
        "fleet_id": "fleet",
        "inventory": { "digest": "", "envelopes": [] },
        "summary_only": false
    });
    let mut body = serde_json::to_vec(&value).unwrap();
    body.resize(2 * 1024 * 1024 + 1, b' ');
    let response = router(state(root.path()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/internal/replication/export")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// The replication worker asks the daemon whether it needs a checkpoint's manifest, and
/// hands it a whole manifest to adopt, which can be far above axum's default body limit.
#[tokio::test]
async fn the_worker_asks_for_and_hands_over_checkpoint_manifests() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let post = |uri: &str, body: Vec<u8>| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(post(
            "/v1/internal/replication/checkpoint-need",
            b"{}".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let need: Value = serde_json::from_slice(&body).unwrap();
    assert!(need["value"].is_null(), "{need}");

    let manifest = crate::store::CheckpointManifest {
        checkpoint: "checkpoint/2026-09-27".into(),
        cut_unix_ms: crate::store::checkpoint_cut("checkpoint/2026-09-27").unwrap(),
        ..Default::default()
    };
    let mut body = serde_json::to_vec(&manifest).unwrap();
    body.resize(8 * 1024 * 1024, b' ');
    let response = app
        .oneshot(post("/v1/internal/replication/checkpoint-adopt", body))
        .await
        .unwrap();
    // Well past the default limit, and refused only because nothing certified it here.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        error.to_string().contains("checkpoint-not-stable"),
        "{error}"
    );
}

#[tokio::test]
async fn api_rejects_the_removed_plan_routes() {
    let root = tempfile::tempdir().unwrap();
    for path in ["/v1/plans/example", "/v1/plan-runs/example"] {
        let response = router(state(root.path()))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}
#[tokio::test]
async fn mission_resolves_a_bare_document_to_immutable_bytes() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let version = state
        .store
        .put_document("doc/task", b"hello", &None, "document")
        .unwrap();
    let app = router(state);
    let (status, body) = json_request(
        app.clone(),
        "/v1/intent/mission",
        serde_json::to_value(MissionRequest {
            intent: crate::model::IntentInput {
                kdl: r#"version 2
 message "task" { to "worker"; content "doc/task" } "#
                    .into(),
                source_name: None,
            },
            at_index: None,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.pointer("/resolved_intent/kdl")
            .and_then(Value::as_str)
            .unwrap()
            .contains(&format!("doc/task@{}", version.hash))
    );
}

async fn preview_and_apply(app: Router, kdl: &str) -> (Value, StatusCode, Value) {
    let (status, preview) = json_request(
        app.clone(),
        "/v1/intent/mission",
        serde_json::to_value(MissionRequest {
            intent: crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
            at_index: None,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let (status, applied) = json_request(
        app,
        "/v1/intent/apply",
        serde_json::to_value(ApplyRequest {
            intent: crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
            expected_subjects: serde_json::from_value(preview["subject_tokens"].clone())
                .unwrap(),
            idempotency_key: format!("apply-{}", hex::encode(Sha256::digest(kdl))),
            actor: Some("person/alex".into()),
        })
        .unwrap(),
    )
    .await;
    (preview, status, applied)
}

#[tokio::test]
async fn publication_accepts_a_step_assigned_to_an_undeclared_person() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let kdl = r#"version 2
mission "review" state="ready" {
  goal "Review the candidate."
  step "review" { assigned-to "person/reviewer" }
}"#;

    let (preview, status, applied) = preview_and_apply(app, kdl).await;
    assert_eq!(preview["blockers"], json!([]));
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(
        store.unresolved_graph_references().unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn publication_accepts_available_work_for_a_person_and_an_existing_agent() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let (_, status, applied) = preview_and_apply(
        app.clone(),
        r#"version 2
agent "example/worker" { workspace "/tmp"; command "true" }"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    let kdl = r#"version 2
mission "review" state="ready" {
  goal "Review the candidate."
  step "review" {
    available-to "person/reviewer"
    available-to "agent/example/worker"
  }
}"#;

    let (preview, status, applied) = preview_and_apply(app, kdl).await;
    assert_eq!(preview["blockers"], json!([]));
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(
        store.unresolved_graph_references().unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn publication_refuses_a_reference_that_does_not_resolve_without_a_write() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let before = store.index().unwrap();
    let kdl = r#"version 2
mission "work" state="ready" {
  goal "Complete the work."
  step "do-work" { assigned-to "agent/example/nobody" }
}"#;
    let refusal =
        "mission `mission/work` references missing eligible agent `agent/example/nobody`";

    let (preview, status, error) = preview_and_apply(app.clone(), kdl).await;
    assert_eq!(preview["blockers"], json!([refusal]));
    assert_eq!(preview["warnings"], json!([]));
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["code"], "unresolved-reference");
    assert_eq!(error["details"]["refusals"], json!([refusal]));
    assert_eq!(store.index().unwrap(), before);

    let available = kdl.replace(
        "assigned-to \"agent/example/nobody\"",
        "available-to \"person/reviewer\"; available-to \"agent/example/nobody\"",
    );
    let (preview, status, error) = preview_and_apply(app.clone(), &available).await;
    assert_eq!(preview["blockers"], json!([refusal]));
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["code"], "unresolved-reference");
    assert_eq!(error["details"]["refusals"], json!([refusal]));
    assert_eq!(store.index().unwrap(), before);

    let declared =
        format!("{kdl}\nagent \"example/nobody\" {{ workspace \"/tmp\"; command \"true\" }}");
    let (preview, status, applied) = preview_and_apply(app, &declared).await;
    assert_eq!(preview["blockers"], json!([]));
    assert_eq!(status, StatusCode::OK, "{applied}");
}

#[tokio::test]
async fn publication_refuses_a_render_that_would_fail_on_this_host() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().display().to_string();
    for args in [&["init", "-q"][..], &["add", "tracked"][..]] {
        if args[0] == "add" {
            std::fs::write(workspace.path().join("tracked"), "original\n").unwrap();
        }
        assert!(
            crate::test_support::git()
                .args(args)
                .current_dir(workspace.path())
                .status()
                .unwrap()
                .success()
        );
    }

    let (_, status, applied) = preview_and_apply(
        app.clone(),
        &format!(
            "version 2\nagent \"one\" {{ workspace {workspace_path:?}; command \"true\"; render {{ file \"shared\" \"one\" }} }}"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    let before = store.index().unwrap();

    let kdl = format!(
        r#"version 2
agent "two" {{ workspace {workspace_path:?}; command "true"; render {{ file "shared" "two" }} }}
agent "three" {{ workspace {workspace_path:?}; command "true"; render {{ file "tracked" "changed" }} }}
agent "four" {{ workspace {workspace_path:?}; command "true"; render {{ file "own" "a"; file "own" "b" }} }}
"#
    );
    let (preview, status, error) = preview_and_apply(app, &kdl).await;
    let shared = workspace.path().join("shared");
    let own = workspace.path().join("own");
    let tracked = workspace.path().join("tracked");
    let refusals = json!([
        format!(
            "prepare render for agent/node.four in {workspace_path}: render operations disagree about {}",
            own.display()
        ),
        format!(
            "prepare render for agent/node.three in {workspace_path}: render refuses to change tracked file {}",
            tracked.display()
        ),
        format!(
            "render owners agent/node.one and agent/node.two disagree about {}",
            shared.display()
        ),
    ]);
    assert_eq!(preview["blockers"], refusals);
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["code"], "render-refused");
    assert_eq!(error["details"]["refusals"], refusals);
    assert_eq!(store.index().unwrap(), before);
    assert!(!shared.exists());
}

#[tokio::test]
async fn st_doctor_reports_graph_references_that_no_longer_resolve() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state
        .store
        .apply_internal(
            &crate::graph::parse_intent(
                r#"version 2
mission "work" state="ready" {
  goal "Complete the work."
  step "do-work" { assigned-to "agent/example/nobody" }
}"#,
                "node",
            )
            .unwrap(),
            "published before the reference checks",
        )
        .unwrap();
    let (status, doctor) = get_request(router(state), "/v1/doctor").await;
    assert_eq!(status, StatusCode::OK, "{doctor}");
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "graph-references")
        .expect("doctor checks graph references");
    assert_eq!(check["status"], "warn");
    assert_eq!(
        check["message"],
        "1 reference no longer resolves: mission `mission/work` references missing eligible agent `agent/example/nobody`"
    );
}

#[tokio::test]
async fn preview_and_publish_reject_invalid_nested_declarations_without_a_write() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let before = store.index().unwrap();
    let kdl = r#"version 2
mission "invalid-message" state="ready" {
  goal "Reject this declaration before the mission runs."
  step "send" {
    agentless
    message "notice" {
      to "agent/worker"
      subject "This field is not valid."
      body "This field is not valid."
    }
  }
}"#;

    let (preview_status, preview_error) = json_request(
        app.clone(),
        "/v1/intent/mission",
        serde_json::to_value(MissionRequest {
            intent: crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: Some("invalid-message.kdl".into()),
            },
            at_index: None,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(preview_status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(preview_error["code"], "unknown-child");
    assert_eq!(store.index().unwrap(), before);

    let (publish_status, publish_error) = json_request(
        app,
        "/v1/intent/apply",
        serde_json::to_value(ApplyRequest {
            intent: crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: Some("invalid-message.kdl".into()),
            },
            expected_subjects: BTreeMap::new(),
            idempotency_key: "reject-invalid-nested-message".into(),
            actor: Some("person/alex".into()),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(publish_status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(publish_error["code"], "unknown-child");
    assert_eq!(store.index().unwrap(), before);
}

#[test]
fn launch_decision_responses_preserve_type_membership_uniqueness_and_rank_order() {
    let options = vec![
        LaunchDecisionOption {
            id: "a".into(),
            label: "A".into(),
            description: Some("first option".into()),
        },
        LaunchDecisionOption {
            id: "b".into(),
            label: "B".into(),
            description: None,
        },
    ];
    assert!(valid_launch_decision_response(
        &LaunchDecisionType::Boolean,
        &[],
        &LaunchDecisionResponse::Boolean(true)
    ));
    assert!(valid_launch_decision_response(
        &LaunchDecisionType::SingleChoice,
        &options,
        &LaunchDecisionResponse::SingleChoice("a".into())
    ));
    assert!(!valid_launch_decision_response(
        &LaunchDecisionType::SingleChoice,
        &options,
        &LaunchDecisionResponse::SingleChoice("missing".into())
    ));
    assert!(valid_launch_decision_response(
        &LaunchDecisionType::MultipleChoice,
        &options,
        &LaunchDecisionResponse::MultipleChoice(vec!["b".into(), "a".into()])
    ));
    assert!(!valid_launch_decision_response(
        &LaunchDecisionType::MultipleChoice,
        &options,
        &LaunchDecisionResponse::MultipleChoice(vec!["a".into(), "a".into()])
    ));
    assert!(valid_launch_decision_response(
        &LaunchDecisionType::Rank,
        &options,
        &LaunchDecisionResponse::Rank(vec!["b".into(), "a".into()])
    ));
    assert!(!valid_launch_decision_response(
        &LaunchDecisionType::Rank,
        &options,
        &LaunchDecisionResponse::Rank(vec!["a".into()])
    ));
    assert!(!valid_launch_decision_response(
        &LaunchDecisionType::Rank,
        &options,
        &LaunchDecisionResponse::MultipleChoice(vec!["a".into(), "b".into()])
    ));
}

#[test]
fn launch_candidates_accept_durable_seats_and_implicit_loop_missions() {
    let source = r#"
version 2
mission "planned/work" state="ready" {
  goal "Keep a worker available and improve the result."
  agent "worker" {
    workspace "."
    command "true"
    restart "always"
  }
  loop "improve" {
    max-rounds 2
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    assert_eq!(intent.missions.len(), 2);
    assert!(intent.subjects.is_empty());
    assert!(launch_candidate_mission(&intent, "planned/work").is_ok());

    let extra = parse_intent(
        r#"
version 2
mission "planned/work" state="ready" { goal "Do the requested work." }
mission "unrequested/work" state="ready" { goal "Do unrelated work." }
"#,
        "node",
    )
    .unwrap();
    let error = launch_candidate_mission(&extra, "planned/work").unwrap_err();
    assert_eq!(error.code, "wrong-launch-mission");
}

#[tokio::test]
async fn planner_choice_is_frozen_per_launch_and_survives_projection_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let database = root.path().join("graph.sqlite3");
    let mut first = state(root.path());
    first.store = Arc::new(Store::open(&database, "node").unwrap());
    let first_app = router(first.clone());
    let (status, first_launch) = json_request(
        first_app,
        "/v1/launches",
        serde_json::to_value(PlanningSessionStartRequest {
            mission: "first-planner".into(),
            run: None,
            request: b"Plan the first mission.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("person/alex".into()),
            provider: None,
            model: None,
            effort: None,
            idempotency_key: "first-planner-session".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first_launch}");
    assert_eq!(first_launch["planner_config"]["provider"], "codex");
    assert_eq!(first_launch["planner_config"]["model"], "gpt-6-sol");
    assert_eq!(first_launch["planner_config"]["effort"], "medium");

    let mut second = first.clone();
    second.planner_default = PlannerSpec {
        provider: "claude".into(),
        model: Some("opus".into()),
        effort: Some("high".into()),
    };
    let (status, second_launch) = json_request(
        router(second),
        "/v1/launches",
        serde_json::to_value(PlanningSessionStartRequest {
            mission: "second-planner".into(),
            run: None,
            request: b"Plan the second mission.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("person/alex".into()),
            provider: None,
            model: None,
            effort: None,
            idempotency_key: "second-planner-session".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_launch}");
    assert_eq!(second_launch["planner_config"]["provider"], "claude");
    assert_eq!(second_launch["planner_config"]["model"], "opus");
    let first_id = first_launch["id"].as_str().unwrap();
    let second_id = second_launch["id"].as_str().unwrap();
    drop(first);
    let rebuilt = Store::open(&database, "node").unwrap();
    assert_eq!(
        rebuilt
            .planning_session(first_id)
            .unwrap()
            .unwrap()
            .planner_config,
        PlannerSpec::default()
    );
    assert_eq!(
        rebuilt
            .planning_session(second_id)
            .unwrap()
            .unwrap()
            .planner_config
            .provider,
        "claude"
    );
}

/// Free mode (#799): an agent starts a planner-backed launch as its own requester.
#[tokio::test]
async fn an_agent_starts_a_launch_as_its_requester() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (status, started) = json_request(
        router(state(root.path())),
        "/v1/launches",
        serde_json::to_value(PlanningSessionStartRequest {
            mission: "example/planned".into(),
            run: None,
            request: b"Plan a harmless task.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("agent/example/chief".into()),
            provider: None,
            model: None,
            effort: None,
            idempotency_key: "agent-launch-session".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["requester"], "agent/example/chief");
}

#[tokio::test]
async fn planning_requires_an_exact_preview_and_publishes_without_a_run() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let marker = workspace.join("marker.txt");
    fs::write(&marker, "unchanged\n").unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);

    let (status, started) = json_request(
        app.clone(),
        "/v1/launches",
        serde_json::to_value(PlanningSessionStartRequest {
            mission: "planned/work".into(),
            run: None,
            request: b"Mission a two-step release without changing this workspace.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("alex".into()),
            provider: None,
            model: Some("gpt-5.6-sol".into()),
            effort: Some("medium".into()),
            idempotency_key: "planning-session-test".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let session = started["id"].as_str().unwrap();
    let planner = started["planner"].as_str().unwrap();
    assert!(planner.ends_with(&format!("/planner.{}", &session[..10])));
    assert_eq!(started["requester"], "person/alex");
    let request_reference = started["request"].as_str().unwrap();
    assert!(request_reference.starts_with("doc/planning/"));
    let (request_name, request_hash) = request_reference.rsplit_once('@').unwrap();
    assert_eq!(
        store
            .get_document(request_name, request_hash)
            .unwrap()
            .unwrap(),
        b"Mission a two-step release without changing this workspace."
    );
    let planner_seat = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject == planner)
        .unwrap();
    assert!(planner_seat.owner_run.is_none());
    let planner_launch = planner_seat.member.as_ref().unwrap().launch.clone();
    assert!(matches!(
        &planner_launch,
        crate::model::LaunchSpec::Argv(arguments)
            if !arguments.iter().any(|argument| argument == "--dangerously-bypass-approvals-and-sandbox")
                && !arguments.iter().any(|argument| argument == "--dangerously-bypass-hook-trust")
    ));
    assert!(store.mission_spec("planned/work", None).unwrap().is_none());
    assert!(store.active_mission_runs().unwrap().is_empty());

    let first = br#"
version 2

  mission "planned/work" state="ready" {
    goal "Publish the planned result."
    step "inspect" { goal "Inspect the source." }
    step "change" {
      goal "Make the approved change."
      depends-on { step "inspect" completed }
    }
  }

"#;
    let mut side_effect = br#"
version 2
  agent "side-effect" {
    workspace "/tmp"
    harness "codex" {}
  }
"#
    .to_vec();
    side_effect.extend_from_slice(&first[b"version 2\n".len()..]);
    let (status, rejected) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/submit"),
        serde_json::to_value(PlanningCandidateSubmitRequest {
            actor: planner.into(),
            markdown: b"# Mission with a side effect".to_vec(),
            kdl: side_effect,
            idempotency_key: "planning-candidate-side-effect".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    assert_eq!(rejected["code"], "wrong-launch-mission");
    assert!(
        store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .all(|desired| desired.subject != "agent/node.side-effect")
    );

    let (status, submitted) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/submit"),
        serde_json::to_value(PlanningCandidateSubmitRequest {
            actor: planner.into(),
            markdown: b"# Mission\n\n1. Inspect.\n2. Change.\n".to_vec(),
            kdl: first.to_vec(),
            idempotency_key: "planning-candidate-one".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    assert_eq!(submitted["candidate"]["revision"], 1);
    assert!(store.mission_spec("planned/work", None).unwrap().is_none());
    let (status, resubmitted) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/submit"),
        serde_json::to_value(PlanningCandidateSubmitRequest {
            actor: planner.into(),
            markdown: b"# Mission\n\n1. Inspect.\n2. Change.\n".to_vec(),
            kdl: first.to_vec(),
            idempotency_key: "planning-candidate-one-network-retry".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resubmitted}");
    assert_eq!(resubmitted["candidate"]["revision"], 1);
    assert_eq!(
        store
            .claims_for(
                &format!("planning-session/{session}"),
                Some("planning-session.candidate-submitted"),
            )
            .unwrap()
            .len(),
        1
    );

    let (status, previewed) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/preview"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{previewed}");
    let first_hash = previewed["preview"]["hash"].as_str().unwrap().to_owned();
    let graph = previewed["preview"]["graph"].as_str().unwrap();
    assert!(graph.contains("inspect [root]"), "{graph}");
    assert!(graph.contains("change [after inspect]"), "{graph}");
    assert!(
        previewed["preview"]["diff"]
            .as_str()
            .unwrap()
            .contains("mission/planned/work")
    );
    let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
    assert_eq!(attention.as_array().unwrap().len(), 1);
    assert_eq!(attention[0]["kind"], "launch-approval");
    assert_eq!(
        attention[0]["subject"],
        format!("planning-session/{session}")
    );
    assert_eq!(attention[0]["actions"][0]["argv"][1], "launch");
    assert_eq!(attention[0]["actions"][1]["argv"][1], "launch");
    assert_eq!(attention[0]["actions"][2]["argv"][1], "launch");

    let (status, revised) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/revise"),
        serde_json::to_value(PlanningRevisionRequest {
            actor: "person/alex".into(),
            feedback: b"Add a verification step.".to_vec(),
            idempotency_key: "planning-revision-one".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revised}");
    assert_eq!(revised["status"], "revision-requested");
    assert!(revised.get("preview").is_none());
    assert!(store.mission_spec("planned/work", None).unwrap().is_none());
    let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
    assert_eq!(attention, json!([]));

    let second = br#"
version 2

  mission "planned/work" state="ready" {
    goal "Publish the planned and verified result."
    step "inspect" { goal "Inspect the source." }
    step "change" {
      goal "Make the approved change."
      depends-on { step "inspect" completed }
    }
    step "verify" {
      goal "Verify the result."
      depends-on { step "change" completed }
    }
  }

"#;
    let (status, resubmitted) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/submit"),
        serde_json::to_value(PlanningCandidateSubmitRequest {
            actor: planner.into(),
            markdown: b"# Mission\n\n1. Inspect.\n2. Change.\n3. Verify.\n".to_vec(),
            kdl: second.to_vec(),
            idempotency_key: "planning-candidate-two".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resubmitted}");
    assert_eq!(resubmitted["candidate"]["revision"], 2);

    let (status, previewed) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/preview"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{previewed}");
    let current_hash = previewed["preview"]["hash"].as_str().unwrap().to_owned();
    assert_ne!(current_hash, first_hash);
    let (status, variants) = get_request(
        app.clone(),
        &format!("/v1/client/launches/{session}/variants"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{variants}");
    let variant = &variants["items"][0];
    let preview_token = variant["preview_token"].as_str().unwrap().to_owned();
    assert!(preview_token.starts_with("lpv0:"));
    assert_eq!(current_hash, preview_token);
    assert_eq!(variant["normalized_mission"]["id"], "planned/work");
    assert_eq!(
        variant["preview"]["goal"],
        "Publish the planned and verified result."
    );
    assert_eq!(variant["preview"]["steps"].as_array().unwrap().len(), 3);
    assert_eq!(variant["preview"]["steps"][2]["depends"], json!(["change"]));
    let (status, launch_page) = get_request(app.clone(), "/v1/client/launches").await;
    assert_eq!(status, StatusCode::OK, "{launch_page}");
    assert_eq!(launch_page["items"][0]["preview"], variant["preview"]);
    let (status, attention_page) =
        get_request(app.clone(), "/v1/client/attention?person=person%2Falex").await;
    assert_eq!(status, StatusCode::OK, "{attention_page}");
    assert_eq!(
        attention_page["items"][0]["launch_id"],
        format!("launch/{session}")
    );
    assert_eq!(attention_page["items"][0]["preview"], variant["preview"]);
    assert_eq!(attention_page["items"][0]["preview_token"], preview_token);
    let document_name = store.planning_session(session).unwrap().unwrap().request;
    let (status, document) = get_request(
        app.clone(),
        &format!(
            "/v1/client/documents/content?name={}",
            urlencoding::encode(&document_name)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["reference"], document_name);
    assert!(
        document["bytes"]
            .as_array()
            .is_some_and(|bytes| !bytes.is_empty())
    );
    assert!(
        !serde_json::to_string(&variant["normalized_mission"])
            .unwrap()
            .contains("declarations_kdl")
    );
    assert_eq!(
        variant["visualization"]["views"],
        json!([
            "graph",
            "timeline",
            "swimlane",
            "revision",
            "risk",
            "live-progress"
        ])
    );
    assert_eq!(
        variant["visualization"]["timeline"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        variant["visualization"]["swimlanes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        variant["structured_diff"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (status, decision) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/decisions"),
        serde_json::to_value(LaunchDecisionRequest {
            actor: planner.into(),
            question: "Which verification level?".into(),
            decision_type: LaunchDecisionType::SingleChoice,
            options: vec![
                LaunchDecisionOption {
                    id: "focused".into(),
                    label: "Focused".into(),
                    description: Some("Run the focused verification suite".into()),
                },
                LaunchDecisionOption {
                    id: "full".into(),
                    label: "Full".into(),
                    description: None,
                },
            ],
            idempotency_key: "launch-decision-verification".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decision}");
    let decision_id = decision["id"].as_str().unwrap();
    let decision_path = decision_id.trim_start_matches("launch-decision/");
    let (status, answered) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/decisions/{decision_path}/answer"),
        serde_json::to_value(LaunchDecisionAnswerRequest {
            actor: "person/alex".into(),
            response: LaunchDecisionResponse::SingleChoice("full".into()),
            explanation: Some("release candidate requires the full suite".into()),
            expected_revision: 1,
            idempotency_key: "launch-decision-verification-answer".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    assert_eq!(answered["state"], "answered");
    assert_eq!(
        answered["response"],
        json!({"type":"single-choice","value":"full"})
    );
    assert_eq!(
        answered["explanation"],
        "release candidate requires the full suite"
    );
    let (status, immutable) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/decisions/{decision_path}/answer"),
        serde_json::to_value(LaunchDecisionAnswerRequest {
            actor: "person/alex".into(),
            response: LaunchDecisionResponse::SingleChoice("focused".into()),
            explanation: None,
            expected_revision: 1,
            idempotency_key: "launch-decision-verification-conflict".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{immutable}");
    assert_eq!(immutable["code"], "launch-decision-immutable");
    let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
    assert_eq!(attention.as_array().unwrap().len(), 1);
    assert_eq!(attention[0]["kind"], "launch-approval");

    let (status, unauthorized) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/approve"),
        serde_json::to_value(PlanningApprovalRequest {
            actor: "person/intruder".into(),
            preview_hash: current_hash.clone(),
            idempotency_key: "planning-approve-unauthorized".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{unauthorized}");
    assert_eq!(unauthorized["code"], "launch-review-not-authorized");
    assert!(store.mission_spec("planned/work", None).unwrap().is_none());

    let (status, stale) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/approve"),
        serde_json::to_value(PlanningApprovalRequest {
            actor: "person/alex".into(),
            preview_hash: first_hash,
            idempotency_key: "planning-approve-stale".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["code"], "stale-launch-preview");
    assert!(store.mission_spec("planned/work", None).unwrap().is_none());

    let (status, approved) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/approve"),
        serde_json::to_value(PlanningApprovalRequest {
            actor: "person/alex".into(),
            preview_hash: preview_token.clone(),
            idempotency_key: "planning-approve-current".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["status"], "approved");
    assert_eq!(
        approved["published_revision"],
        approved["candidate"]["mission_revision"]
    );
    assert!(store.mission_spec("planned/work", None).unwrap().is_some());
    // The planner is a durable top-level seat, not a synthetic standing
    // mission. Approval removes that seat directly and leaves no planner
    // mission run behind to clean up.
    assert!(store.active_mission_runs().unwrap().is_empty());
    assert_eq!(fs::read_to_string(&marker).unwrap(), "unchanged\n");
    assert_eq!(fs::read_dir(&workspace).unwrap().count(), 1);
    let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
    assert_eq!(attention, json!([]));

    let documents = store
        .latest_claim(
            &format!("planning-session/{session}"),
            Some("planning-session.approved"),
        )
        .unwrap()
        .expect("the published mission does not link its documents");
    for field in ["markdown", "kdl"] {
        let reference = documents
            .body
            .pointer(&format!("/fields/{field}"))
            .and_then(Value::as_str)
            .unwrap();
        let (name, hash) = reference.rsplit_once('@').unwrap();
        assert!(store.get_document(name, hash).unwrap().is_some());
    }
    let stopped_planner = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject == planner)
        .expect("approval did not publish the planner stop tombstone");
    assert_eq!(stopped_planner.kind, "stop");

    let (status, approved_again) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/approve"),
        serde_json::to_value(PlanningApprovalRequest {
            actor: "person/alex".into(),
            preview_hash: approved["preview"]["hash"].as_str().unwrap().into(),
            idempotency_key: "planning-approve-retry".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved_again}");
    assert_eq!(approved_again["status"], "approved");
    let (status, started_run) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/approve-and-launch"),
        serde_json::to_value(LaunchApproveAndStartRequest {
            actor: "person/alex".into(),
            preview_hash: preview_token.clone(),
            workspace: workspace.display().to_string(),
            inputs: BTreeMap::new(),
            idempotency_key: "approved-launch-combined".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started_run}");
    assert_eq!(
        started_run["mission_run"]["revision"],
        approved["published_revision"]
    );
    assert_eq!(started_run["launch"]["status"], "approved");
    let (status, started_again) = json_request(
        app,
        &format!("/v1/launches/{session}/approve-and-launch"),
        serde_json::to_value(LaunchApproveAndStartRequest {
            actor: "person/alex".into(),
            preview_hash: preview_token,
            workspace: workspace.display().to_string(),
            inputs: BTreeMap::new(),
            idempotency_key: "approved-launch-combined".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started_again}");
    assert_eq!(
        started_again["mission_run"]["subject"],
        started_run["mission_run"]["subject"]
    );
    assert_eq!(
        store
            .claims_for("mission/planned/work", Some("mission.published"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .claims_for(
                &format!("planning-session/{session}"),
                Some("planning-session.approved"),
            )
            .unwrap()
            .len(),
        1
    );
    for kind in [
        "planning-session.started",
        "planning-session.candidate-submitted",
        "planning-session.previewed",
        "planning-session.revision-requested",
        "planning-session.approved",
    ] {
        assert!(
            !store
                .claims_for(&format!("planning-session/{session}"), Some(kind))
                .unwrap()
                .is_empty(),
            "missing {kind}"
        );
    }
    let started_event = store
        .claims_for(
            &format!("planning-session/{session}"),
            Some("planning-session.started"),
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        started_event
            .body
            .pointer("/fields/request")
            .and_then(Value::as_str),
        Some(request_reference)
    );
    let before = serde_json::to_value(store.planning_session(session).unwrap()).unwrap();
    store.rebuild_claim_projections().unwrap();
    let after = serde_json::to_value(store.planning_session(session).unwrap()).unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn declarative_planning_approval_stops_its_session_planner() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let request = store
        .put_document(
            "doc/planning/direct/request",
            b"Mission one inspection.",
            &None,
            "direct-planning-request",
        )
        .unwrap();
    let session = "planning/direct/one";
    let source = format!(
        r#"version 2
planning-session {session:?} {{
  mission "planned/direct"
  request {:?}
  workspace {:?}
  requester "person/operator"
  planner "codex" {{ model "gpt-5.6-sol"; effort "medium" }}
}}
"#,
        format!("{}@{}", request.name, request.hash),
        workspace.display().to_string(),
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "direct-planning-session",
            Some("person/operator"),
        )
        .unwrap();
    let planner =
        crate::graph::planning_planner_subject(&format!("planning-session/{session}"));
    assert_eq!(
        store.selected_desired_kind(&planner).unwrap().as_deref(),
        Some("agent")
    );

    let app = router(state);
    let encoded_session = urlencoding::encode(session);
    let candidate = br#"version 2
mission "planned/direct" state="ready" {
  goal "Inspect the release."
  step "inspect" { goal "Inspect the release input." }
}
"#;
    let (status, submitted) = json_request(
        app.clone(),
        &format!("/v1/launches/{encoded_session}/submit"),
        serde_json::to_value(PlanningCandidateSubmitRequest {
            actor: planner.clone(),
            markdown: b"# Mission\n\nInspect the release input.\n".to_vec(),
            kdl: candidate.to_vec(),
            idempotency_key: "direct-planning-candidate".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    let (status, previewed) = json_request(
        app.clone(),
        &format!("/v1/launches/{encoded_session}/preview"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{previewed}");
    let preview_hash = previewed["preview"]["hash"].as_str().unwrap();
    let (status, approved) = json_request(
        app,
        &format!("/v1/launches/{encoded_session}/approve"),
        serde_json::to_value(PlanningApprovalRequest {
            actor: "person/operator".into(),
            preview_hash: preview_hash.into(),
            idempotency_key: "direct-planning-approval".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["status"], "approved");
    assert_eq!(
        store.selected_desired_kind(&planner).unwrap().as_deref(),
        Some("stop")
    );
}

#[test]
fn an_agent_known_only_from_its_own_harness_observations_is_history() {
    let root = tempfile::tempdir().unwrap();
    let store = state(root.path()).store;
    let subject = "agent/diagnostic-run/sig.base";
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("idle".into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("sig-1".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("sig-base-observed".into()),
        })
        .unwrap();
    let current =
        client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert!(
        current.iter().all(|agent| agent["id"] != subject),
        "{current:?}"
    );
    let history =
        client_agent_resources(&store, true, "snapshot", store.index().unwrap()).unwrap();
    let agent = history.iter().find(|agent| agent["id"] == subject).unwrap();
    assert_eq!(agent["operational"]["layer"], "history");
    assert_eq!(agent["operational"]["reasons"], json!(["undeclared"]));
}

#[test]
fn client_work_projection_includes_agentless_mission_steps() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let kdl = r#"version 2
mission "visible-agentless" state="ready" {
  goal "Keep agentless work visible in Control."
  step "steward" { title "Keep watch"; agentless }
}"#;
    let intent = parse_intent(kdl, "node").unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &preview.subject_tokens, "visible-agentless")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "visible-agentless".into(),
            revision: None,
            workspace: workspace.display().to_string(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "visible-agentless-run".into(),
        })
        .unwrap();
    let resources = client_work_resources(
        &state.store,
        None,
        true,
        client_now_ms(),
        state.store.index().unwrap(),
    )
    .unwrap();
    let step = resources
        .iter()
        .find(|item| item["mission_run_id"] == run.subject)
        .unwrap();
    assert_eq!(step["path"], "steward");
    assert_eq!(step["title"], "Keep watch");
    assert_eq!(step["assigned_to"], Value::Null);
    assert_eq!(step["last_progress"], Value::Null);
    assert_eq!(step["progress_at"], Value::Null);
    assert_eq!(step["agentless"], true);
    assert!(
        client_work_resources(
            &state.store,
            Some("agent/other"),
            true,
            client_now_ms(),
            state.store.index().unwrap()
        )
        .unwrap()
        .is_empty()
    );
}

/// Mission rows mark st's keep-open steps and system missions from the declaration, and from
/// the names the fleet used for revisions published before those flags.
#[test]
fn mission_rows_carry_keep_open_and_system_flags() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let kdl = r#"version 2
mission "observer" state="ready" {
  system
  goal "Watch for work."
  step "hold" { agentless; keep-open }
  step "keep-watch" { agentless }
  step "work" { agentless }
}
mission "plain" state="ready" {
  goal "Do work."
  step "work" { agentless }
}"#;
    let intent = parse_intent(kdl, "node").unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &preview.subject_tokens, "flags")
        .unwrap();
    for mission in ["observer", "plain"] {
        state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: mission.into(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("flags-{mission}"),
            })
            .unwrap();
    }
    let index = state.store.index().unwrap();
    let detail = |id: &str| {
        client_v0::mission_resources(&state.store, index, false, Some(id))
            .unwrap()
            .remove(0)
    };
    let observer = detail("mission/observer");
    assert_eq!(observer["system"], true);
    let keep_open = observer["run_details"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| (step["path"].as_str().unwrap().to_owned(), step["keep_open"].clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(keep_open["hold"], true, "declared");
    assert_eq!(keep_open["keep-watch"], true, "a fleet name, before the flag");
    assert_eq!(keep_open["work"], false);
    assert_eq!(detail("mission/plain")["system"], false);
    let cards = client_v0::mission_list_cards_with(
        &state.store,
        &["mission/observer".into(), "mission/plain".into()],
        client_now_ms(),
        &BTreeSet::new(),
    )
    .unwrap();
    let system = cards
        .iter()
        .map(|card| (card["id"].as_str().unwrap().to_owned(), card["system"].clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(system["mission/observer"], true);
    assert_eq!(system["mission/plain"], false);
}

/// A work item is the resource the work history lists for it, read without enriching every
/// other step the store has run.
#[test]
fn a_work_item_reads_only_its_own_step() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let kdl = r#"version 2
mission "many-runs" state="ready" {
  goal "Run often."
  concurrent-runs max=100
  step "build" { assigned-to "agent/builder" }
  step "watch" { agentless }
}"#;
    let intent = parse_intent(kdl, "node").unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &preview.subject_tokens, "many-runs")
        .unwrap();
    for run in 0..8 {
        state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "many-runs".into(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("many-runs-{run}"),
            })
            .unwrap();
    }
    let (now, index) = (client_now_ms(), state.store.index().unwrap());
    let history = client_work_resources(&state.store, None, true, now, index).unwrap();
    assert_eq!(history.len(), 16);
    for item in &history {
        let id = item["id"].as_str().unwrap();
        let detail = client_work_item(&state.store, id, None, now, index)
            .unwrap()
            .unwrap();
        assert_eq!(&detail, item, "{id}");
    }
    let id = history[0]["id"].as_str().unwrap();
    crate::store::STEPS_ENRICHED.with(|enriched| enriched.set(0));
    client_work_item(&state.store, id, None, now, index).unwrap();
    assert_eq!(crate::store::STEPS_ENRICHED.with(std::cell::Cell::get), 1);
    let (assigned, assignee) = history
        .iter()
        .find_map(|item| Some((item["id"].as_str()?, item["assigned_to"].as_str()?)))
        .unwrap();
    assert!(
        client_work_item(&state.store, assigned, Some("agent/other"), now, index)
            .unwrap()
            .is_none()
    );
    assert!(
        client_work_item(&state.store, assigned, Some(assignee), now, index)
            .unwrap()
            .is_some()
    );
    assert!(
        client_work_item(&state.store, "step-run/none/build", None, now, index)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn steady_mission_and_work_pages_resume_after_unrelated_commits() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    for number in 0..7 {
        let name = format!("paged-{number}");
        let kdl = format!(
            r#"version 2
mission "{name}" state="ready" {{
  goal "Run a bounded page."
  step "build" {{ assigned-to "agent/builder" }}
}}"#
        );
        let intent = parse_intent(&kdl, "node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl,
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &preview.subject_tokens, &name)
            .unwrap();
        state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: name.clone(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("run-{name}"),
            })
            .unwrap();
    }
    for collection in ["missions", "work"] {
        let expected = if collection == "missions" {
            state.store.mission_collection_ids(true, 0, 100).unwrap()
        } else {
            client_work_resources(
                &state.store,
                None,
                true,
                client_now_ms(),
                state.store.index().unwrap(),
            )
            .unwrap()
            .into_iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect()
        };
        let app = router(state.clone());
        let mut cursor: Option<String> = None;
        let mut received = Vec::new();
        loop {
            state
                .store
                .append_claim(&probe_claim(
                    &format!("churn-{collection}-{}", received.len()),
                    "idle",
                ))
                .unwrap();
            let path = format!(
                "/v1/client/{collection}?limit=2&history=true{}",
                cursor
                    .as_deref()
                    .map(|cursor| format!("&cursor={}", urlencoding::encode(cursor)))
                    .unwrap_or_default()
            );
            let (status, response) = get_request(app.clone(), &path).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let page = &response;
            received.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["id"].as_str().unwrap().to_owned()),
            );
            cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(received, expected, "{collection}");
    }
}

/// Each page of the work history is that page of the whole history, for every reader, and
/// reading it enriches only the steps it shows.
#[test]
fn work_history_pages_read_only_what_they_show() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let kdl = r#"version 2
mission "paged" state="ready" {
  goal "Run often."
  concurrent-runs max=100
  step "build" { assigned-to "agent/builder" }
  step "watch" { agentless }
}"#;
    let intent = parse_intent(kdl, "node").unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &preview.subject_tokens, "paged")
        .unwrap();
    let (mut builds, mut builder) = (Vec::new(), String::new());
    for run in 0..6 {
        let view = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "paged".into(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("paged-{run}"),
            })
            .unwrap();
        let build = view.steps.iter().find(|step| step.step == "build").unwrap();
        builder = build
            .assigned_to
            .clone()
            .expect("the build step is assigned");
        builds.push(build.subject.clone());
    }
    // Two builds change after the rest, so the history's newest-first order differs
    // from the order the steps were made in; the builder claims the first.
    for build in [&builds[4], &builds[0]] {
        state.store.set_step_state(build, "ready", None).unwrap();
    }
    state
        .store
        .work_action(
            &builds[0],
            "claim",
            &crate::model::WorkRequest {
                wait_ms: None,
                actor: Some(builder.clone()),
                incarnation: Some("builder-1".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "claim-paged".into(),
            },
        )
        .unwrap();
    let (now, index) = (client_now_ms(), state.store.index().unwrap());
    let bare = builder.strip_prefix("agent/").unwrap();
    for actor in [
        None,
        Some(builder.as_str()),
        Some(bare),
        Some("agent/other"),
    ] {
        let whole = client_work_resources(&state.store, actor, true, now, index).unwrap();
        assert_eq!(
            whole.len(),
            match actor {
                None => 12,
                Some("agent/other") => 0,
                Some(_) => 6,
            },
            "{actor:?}"
        );
        for limit in [1, 4, 5, 12, 50] {
            let mut offset = 0;
            loop {
                let (page, has_more) =
                    client_work_history_page(&state.store, actor, now, index, offset, limit)
                        .unwrap();
                let end = (offset + limit).min(whole.len());
                assert_eq!(page, whole[offset..end], "{actor:?} {limit} {offset}");
                assert_eq!(has_more, end < whole.len(), "{actor:?} {limit} {offset}");
                offset = end;
                if !has_more {
                    break;
                }
            }
        }
    }
    crate::store::STEPS_ENRICHED.with(|enriched| enriched.set(0));
    let (page, has_more) =
        client_work_history_page(&state.store, None, now, index, 4, 3).unwrap();
    assert_eq!((page.len(), has_more), (3, true));
    assert_eq!(crate::store::STEPS_ENRICHED.with(std::cell::Cell::get), 3);
}

#[tokio::test]
async fn one_targeted_planning_approval_also_approves_the_run_revision() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let initial = r#"version 2
mission "targeted" state="ready" revisions="human-only" {
  goal "Use the initial mission."
  step "work" { agentless }
}
"#;
    let intent = parse_intent(initial, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: initial.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &preview.subject_tokens, "targeted-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "targeted".into(),
            revision: None,
            workspace: workspace.display().to_string(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "targeted-run".into(),
        })
        .unwrap();
    let app = router(state);
    let (status, session) = json_request(
        app.clone(),
        "/v1/launches",
        serde_json::to_value(PlanningSessionStartRequest {
            mission: "ignored".into(),
            run: Some(run.subject.clone()),
            request: b"Add the final verification.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("person/operator".into()),
            provider: None,
            model: None,
            effort: None,
            idempotency_key: "targeted-session".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["id"].as_str().unwrap();
    let planner = session["planner"].as_str().unwrap();
    let candidate = br#"version 2
mission "targeted" state="ready" revisions="human-only" {
  goal "Use the reviewed mission."
  step "work" { agentless }
  step "verify" {
    agentless
    depends-on { step "work" completed }
  }
}
"#;
    let (status, submitted) = json_request(
        app.clone(),
        &format!("/v1/launches/{session_id}/submit"),
        serde_json::to_value(PlanningCandidateSubmitRequest {
            actor: planner.into(),
            markdown: b"# Targeted mission\n".to_vec(),
            kdl: candidate.to_vec(),
            idempotency_key: "targeted-candidate".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    let preview_hash = submitted["preview"]["hash"].as_str().unwrap();
    let (status, approved) = json_request(
        app,
        &format!("/v1/launches/{session_id}/approve"),
        serde_json::to_value(PlanningApprovalRequest {
            actor: "person/operator".into(),
            preview_hash: preview_hash.into(),
            idempotency_key: "targeted-approval".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["status"], "approved");
    let revised = store.mission_run(&run.id).unwrap().unwrap();
    assert_ne!(revised.generation, run.generation);

    let approvals = store
        .events_after(0, None)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "revision-proposal.approved")
        .count();
    assert_eq!(approvals, 1);
}

#[tokio::test]
async fn planning_compares_named_variants_and_proposes_from_one_generation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let source = r#"
version 2

  mission "variants" state="ready" {
    goal "Use the initial mission."
    step "work" { goal "Use the initial goal." }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "variant-initial-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "variants".into(),
            revision: None,
            workspace: workspace.display().to_string(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "variant-run".into(),
        })
        .unwrap();
    let app = router(state);
    let (status, started) = json_request(
        app.clone(),
        "/v1/launches",
        serde_json::to_value(PlanningSessionStartRequest {
            mission: "ignored-when-run-is-present".into(),
            run: Some(run.subject.clone()),
            request: b"Compare a compact mission with an extended mission.".to_vec(),
            workspace: workspace.display().to_string(),
            requester: Some("person/alex".into()),
            provider: None,
            model: None,
            effort: None,
            idempotency_key: "variant-session".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["target_mission_run"], run.subject);
    assert_eq!(started["source_generation"], run.generation);
    let session = started["id"].as_str().unwrap();
    let planner = started["planner"].as_str().unwrap();
    let compact = br#"
version 2

  mission "variants" state="ready" {
    goal "Use the compact mission."
    step "work" { goal "Use the compact goal." }
  }

"#;
    let extended = br#"
version 2

  mission "variants" state="ready" {
    goal "Use the extended mission."
    step "work" { goal "Use the extended goal." }
    step "verify" {
      depends-on { step "work" completed }
      goal "Verify the extended result."
    }
  }

"#;
    for (index, (name, kdl)) in [
        ("compact", compact.as_slice()),
        ("extended", extended.as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, submitted) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/variants/{name}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.into(),
                markdown: format!("# {name} mission\n").into_bytes(),
                kdl: kdl.to_vec(),
                idempotency_key: format!("variant-submit-{name}"),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{submitted}");
        assert_eq!(submitted["variants"].as_array().unwrap().len(), index + 1);
        let (status, previewed) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/variants/{name}/preview"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{previewed}");
    }
    assert_eq!(
        store
            .claims_for(
                &format!("planning-session/{session}"),
                Some("planning-session.candidate-submitted"),
            )
            .unwrap()
            .len(),
        2
    );
    let (status, compared) = get_request(
        app.clone(),
        &format!("/v1/launches/{session}/variants/compact/compare/extended"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{compared}");
    assert_eq!(compared["left"]["name"], "compact");
    assert_eq!(compared["right"]["name"], "extended");

    let (status, proposed) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/variants/extended/propose"),
        serde_json::to_value(PlanningProposalRequest {
            actor: "person/alex".into(),
            reason: "the extended variant has the required check".into(),
            idempotency_key: "variant-propose-extended".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{proposed}");
    assert_eq!(proposed["status"], "applied");
    assert_ne!(proposed["mission_run"]["generation"], run.generation);

    let (status, retried) = json_request(
        app.clone(),
        &format!("/v1/launches/{session}/variants/extended/propose"),
        serde_json::to_value(PlanningProposalRequest {
            actor: "person/alex".into(),
            reason: "the extended variant has the required check".into(),
            idempotency_key: "variant-propose-extended".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(
        retried["mission_run"]["generation"],
        proposed["mission_run"]["generation"]
    );

    let (status, stale) = json_request(
        app,
        &format!("/v1/launches/{session}/variants/compact/propose"),
        serde_json::to_value(PlanningProposalRequest {
            actor: "person/alex".into(),
            reason: "try the stale compact variant".into(),
            idempotency_key: "variant-propose-compact".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{stale}");
    assert_eq!(stale["code"], "stale-launch-generation");
}

#[tokio::test]
async fn message_ingress_rejects_empty_content_before_it_enters_the_fifo() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let (status, body) = json_request(
        app,
        "/v1/messages",
        serde_json::to_value(MessageSendRequest {
            idempotency_key: "empty-message".into(),
            from: "agent/sender".into(),
            to: "agent/receiver".into(),
            content: " \n".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "empty-message");
}

#[tokio::test]
async fn a_runtime_work_message_uses_a_valid_daemon_sender() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let (status, body) = json_request(
        app,
        "/v1/messages",
        serde_json::to_value(MessageSendRequest {
            idempotency_key: "runtime-work-message".into(),
            from: "daemon/runtime".into(),
            to: "agent/worker".into(),
            content: "A durable mission step is ready.".into(),
            title: Some("Mission step ready".into()),
            in_reply_to: None,
            tags: vec!["st3-work:step-run/run/build@1@1@incarnation".into()],
            attachments: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["from"], "daemon/runtime");
    assert_eq!(body["to"], "agent/worker");
}

#[tokio::test]
async fn a_requester_message_uses_a_full_person_subject() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let (status, body) = json_request(
        app,
        "/v1/messages",
        serde_json::to_value(MessageSendRequest {
            idempotency_key: "requester-message".into(),
            from: "requester".into(),
            to: "agent/worker".into(),
            content: "Please do the work.".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["from"], "person/requester");
}

#[tokio::test]
async fn replying_settles_the_original_across_native_delivery_transports() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    for transport in [
        "omp-channel",
        "pi-channel",
        "claude-channel",
        "codex-app-server",
        "opencode-server",
    ] {
        let (status, original) = json_request(
            app.clone(),
            "/v1/messages",
            serde_json::to_value(MessageSendRequest {
                idempotency_key: format!("replay-original-{transport}"),
                from: "agent/sender".into(),
                to: "agent/receiver".into(),
                content: "Please answer once.".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{original}");
        let subject = original["subject"].as_str().unwrap();
        let path = format!(
            "/v1/messages/{}/claims",
            subject.trim_start_matches("message/")
        );
        for lifecycle in ["staged", "delivered"] {
            let (status, claim) = json_request(
                app.clone(),
                &path,
                serde_json::to_value(MessageLifecycleRequest {
                    delegation: None,
                    lifecycle: lifecycle.into(),
                    actor: Some("agent/receiver".into()),
                    transport: Some(transport.into()),
                    runtime_id: None,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: format!("replay-{transport}-{lifecycle}"),
                })
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{claim}");
        }
        let reply = serde_json::to_value(MessageSendRequest {
            idempotency_key: format!("replay-reply-{transport}"),
            from: "agent/receiver".into(),
            to: "agent/sender".into(),
            content: "Answered.".into(),
            title: None,
            in_reply_to: Some(subject.into()),
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap();
        let (status, sent) = json_request(app.clone(), "/v1/messages", reply.clone()).await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        let (status, repeated) = json_request(app.clone(), "/v1/messages", reply).await;
        assert_eq!(status, StatusCode::OK, "{repeated}");
        // The repeat is told it found the first reply, not that it sent one.
        assert_eq!(sent["already_sent"], false, "{sent}");
        assert_eq!(repeated["already_sent"], true, "{repeated}");
        assert_eq!(repeated["subject"], sent["subject"]);
        assert_eq!(repeated["sent_at"], sent["sent_at"]);
        let (_, settled) = get_request(
            app.clone(),
            &format!(
                "/v1/messages/read/{}",
                subject.trim_start_matches("message/")
            ),
        )
        .await;
        assert_eq!(settled["status"], "closed", "{transport}: {settled}");
    }
    let (_, mailbox) =
        get_request(app, "/v1/messages/page?to=agent%2Freceiver&limit=100").await;
    assert!(mailbox["items"].as_array().unwrap().is_empty(), "{mailbox}");
}

#[tokio::test]
async fn reading_a_persons_message_says_who_signed_it_and_an_agents_says_nothing() {
    // Nathan, 2026-10-06: signatures existed but nothing showed them.
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let send = |key: &str, from: &str| {
        serde_json::to_value(MessageSendRequest {
            idempotency_key: key.into(),
            from: from.into(),
            to: "agent/receiver".into(),
            content: "Hello.".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap()
    };
    let read = |subject: &str| {
        format!("/v1/messages/read/{}", subject.trim_start_matches("message/"))
    };
    let (_, person) = json_request(app.clone(), "/v1/messages", send("prov-person", "person/alex")).await;
    let (_, person) = get_request(app.clone(), &read(person["subject"].as_str().unwrap())).await;
    // The fixture seals nothing, so there is no signature to show, only the verdict.
    assert_eq!(person["provenance"]["verdict"], "unsigned", "{person}");
    assert!(person["provenance"].get("signer").is_none(), "{person}");
    let (_, agent) = json_request(app.clone(), "/v1/messages", send("prov-agent", "agent/sender")).await;
    let (_, agent) = get_request(app, &read(agent["subject"].as_str().unwrap())).await;
    assert!(agent.get("provenance").is_none(), "{agent}");
}

#[tokio::test]
async fn a_sender_followup_does_not_settle_the_recipient_message() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let send = |key: &str, from: &str, to: &str, parent: Option<String>| {
        serde_json::to_value(MessageSendRequest {
            idempotency_key: key.into(),
            from: from.into(),
            to: to.into(),
            content: "Please respond.".into(),
            title: None,
            in_reply_to: parent,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap()
    };
    let (_, first) = json_request(
        app.clone(),
        "/v1/messages",
        send("followup-original", "agent/sender", "agent/receiver", None),
    )
    .await;
    let subject = first["subject"].as_str().unwrap();
    let (status, _) = json_request(
        app.clone(),
        "/v1/messages",
        send(
            "followup-same-sender",
            "agent/sender",
            "agent/receiver",
            Some(subject.into()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, original) = get_request(
        app,
        &format!(
            "/v1/messages/read/{}",
            subject.trim_start_matches("message/")
        ),
    )
    .await;
    assert_eq!(original["status"], "sent");
}

#[tokio::test]
async fn a_send_or_reply_to_a_person_is_refused_and_an_agent_still_receives() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let message = |key: &str, from: &str, to: &str, in_reply_to: Option<&str>| {
        serde_json::to_value(MessageSendRequest {
            idempotency_key: key.into(),
            from: from.into(),
            to: to.into(),
            content: "Hello".into(),
            title: None,
            in_reply_to: in_reply_to.map(str::to_owned),
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap()
    };
    // An agent, and a person writing to an agent, are unchanged.
    let (status, asked) =
        json_request(app.clone(), "/v1/messages", message("ask", "person/ada", "agent/worker", None)).await;
    assert_eq!(status, StatusCode::OK, "{asked}");
    let asked = asked["subject"].as_str().unwrap().to_owned();
    let (status, relayed) =
        json_request(app.clone(), "/v1/messages", message("relay", "agent/worker", "agent/helper", None)).await;
    assert_eq!(status, StatusCode::OK, "{relayed}");
    // A send to a person, a reply to a person, a bare name that means the requester, and a
    // person writing to a person all fail with the one error, and write nothing.
    let before = state.store.claims_for_kind_at("message.sent", None, true, 100).unwrap().claims.len();
    for (key, from, to, parent) in [
        ("send", "agent/worker", "person/ada", None),
        ("reply", "agent/worker", "person/ada", Some(asked.as_str())),
        ("requester", "agent/worker", "requester", None),
        ("people", "person/ada", "person/robin", None),
    ] {
        let (status, body) =
            json_request(app.clone(), "/v1/messages", message(key, from, to, parent)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{key}: {body}");
        assert_eq!(body["code"], "person-has-no-inbox", "{key}");
        let text = body["message"].as_str().unwrap();
        assert!(text.starts_with("people do not have inboxes: print your answer in the chat"), "{text}");
        let chat = text.find("print in the chat").unwrap();
        assert!(chat < text.find("work ask").unwrap() && chat < text.find("work update").unwrap());
        assert!(text.contains("only if the person asked for it"), "{text}");
    }
    assert_eq!(
        state.store.claims_for_kind_at("message.sent", None, true, 100).unwrap().claims.len(),
        before,
        "a refused message is not written"
    );
    // What an older client already holds stays readable: a message to a person that is in the
    // graph still lists, reads and settles.
    state
        .store
        .append_claim(&ClaimInput {
            subject: "message/earlier".into(),
            kind: "message.sent".into(),
            actor: Some("agent/worker".into()),
            fields: BTreeMap::from([
                ("from".into(), json!("agent/worker")),
                ("to".into(), json!("person/ada")),
                ("content".into(), json!("Sent before people lost their inbox.")),
                ("status".into(), json!("sent")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("earlier".into()),
        })
        .unwrap();
    let (status, listed) = get_request(app.clone(), "/v1/messages?to=person/ada&include_closed=true").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed.as_array().unwrap().iter().any(|row| row["subject"] == "message/earlier"), "{listed}");
    let (status, read) = get_request(app, "/v1/messages/read/earlier").await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["content"], "Sent before people lost their inbox.");
}

#[tokio::test]
async fn message_lifecycle_requires_the_exact_recipient_actor() {
    let root = tempfile::tempdir().unwrap();
    let app = router(state(root.path()));
    let (status, message) = json_request(
        app.clone(),
        "/v1/messages",
        serde_json::to_value(MessageSendRequest {
            idempotency_key: "recipient-authority-message".into(),
            from: "agent/sender".into(),
            to: "agent/receiver".into(),
            content: "Please review this.".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{message}");
    let message_id = message["subject"]
        .as_str()
        .unwrap()
        .trim_start_matches("message/");
    let path = format!("/v1/messages/{message_id}/claims");

    let (status, missing) = json_request(
        app.clone(),
        &path,
        serde_json::to_value(MessageLifecycleRequest {
            delegation: None,
            lifecycle: "read".into(),
            actor: None,
            transport: None,
            runtime_id: None,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: "missing-message-actor".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{missing}");
    assert_eq!(missing["code"], "missing-message-actor");

    let (status, wrong) = json_request(
        app.clone(),
        &path,
        serde_json::to_value(MessageLifecycleRequest {
            delegation: None,
            lifecycle: "read".into(),
            actor: Some("person/intruder".into()),
            transport: None,
            runtime_id: None,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: "wrong-message-actor".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{wrong}");
    assert_eq!(wrong["code"], "wrong-message-recipient");

    let (status, staged) = json_request(
        app.clone(),
        &path,
        serde_json::to_value(MessageLifecycleRequest {
            delegation: None,
            lifecycle: "staged".into(),
            actor: Some("agent/receiver".into()),
            transport: Some("codex-app-server".into()),
            runtime_id: Some("runtime/receiver".into()),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: "staged-by-recipient-driver".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{staged}");
    assert_eq!(staged["body"]["fields"]["status"], "staged");
    assert_eq!(staged["body"]["fields"]["recipient"], "agent/receiver");
    assert_eq!(staged["body"]["fields"]["transport"], "codex-app-server");
    assert_eq!(staged["body"]["fields"]["runtime_id"], "runtime/receiver");

    let (status, read) = json_request(
        app.clone(),
        &path,
        serde_json::to_value(MessageLifecycleRequest {
            delegation: None,
            lifecycle: "delivered".into(),
            actor: Some("agent/receiver".into()),
            transport: None,
            runtime_id: None,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: "right-message-actor".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["actor"], "agent/receiver");

    let legacy = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/close/obsolete")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn eval_upload_posts_its_staged_documents_before_apply() {
    let root = tempfile::tempdir().unwrap();
    let eval_dir = tempfile::tempdir().unwrap();
    let bytes = b"hello from the eval";
    let hash = hex::encode(Sha256::digest(bytes));
    fs::create_dir_all(eval_dir.path().join(".st3-documents")).unwrap();
    fs::write(eval_dir.path().join(".st3-documents").join(&hash), bytes).unwrap();
    fs::write(
        eval_dir.path().join("eval.kdl"),
        format!(
            r#"
version 2

  agent "eval/demo/worker" {{
    workspace "${{EVAL_ROOT}}"
    command "true"
    restart "never"
  }}

  mission "eval/demo" state="ready" timeout="2m" {{
    goal "Complete mission eval/demo."
    baseline "document-content" {{ has "doc/evals/demo/task@{hash}" "hello" }}
    step "document" {{
      agentless
      title "The document exists"

        message "task" {{ to "person/worker"; content "doc/evals/demo/task@{hash}" }}

    }}
    completion {{ when "all-steps-exhausted" }}
  }}

"#
        ),
    )
    .unwrap();
    let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
    let bundle_hash = hex::encode(Sha256::digest(&bundle));
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    let (status, body) = json_request(
        app.clone(),
        "/v1/evals",
        serde_json::to_value(EvalStartRequest {
            name: "demo".into(),
            bundle_hash,
            bundle,
            inputs: BTreeMap::new(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["mission_run"]
            .as_str()
            .unwrap()
            .starts_with("mission-run/")
    );
    let eval_run = body["mission_run"].as_str().unwrap();
    let (status, eval_status) = get_request(
        app.clone(),
        &format!("/v1/evals/{}", urlencoding::encode(eval_run)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{eval_status}");
    assert_eq!(eval_status["mission_run"], body["mission_run"]);
    assert_eq!(eval_status["lifecycle"], "running");
    assert!(eval_status.get("active_checkpoint").is_none());
    let root = body["mission_run"].as_str().unwrap();
    let eval_seat = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|subject| subject.subject == "agent/eval/demo/worker")
        .expect("the eval did not apply its mission-less seat");
    assert_eq!(eval_seat.owner_run.as_deref(), Some(root));
    let (status, mission_runs) = get_request(
        app,
        &format!("/v1/mission-runs?root={}", urlencoding::encode(root)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mission_runs}");
    assert_eq!(mission_runs.as_array().unwrap().len(), 1);
    assert_eq!(mission_runs[0]["subject"], root);
    assert_eq!(
        store
            .get_document("doc/evals/demo/task", &hash)
            .unwrap()
            .unwrap(),
        bytes
    );
}

#[tokio::test]
async fn the_same_eval_bundle_can_run_again_with_a_new_workspace() {
    let root = tempfile::tempdir().unwrap();
    let eval_dir = tempfile::tempdir().unwrap();
    fs::write(
        eval_dir.path().join("eval.kdl"),
        r#"version 2
mission "eval/repeat" state="ready" timeout="2m" {
  concurrent-runs max=2
  goal "Run the same archived eval in a fresh workspace."
  completion { when "all-steps-exhausted" }
  step "done" {
    agentless
    gate "the current eval workspace exists" {
      exec "test -d '${EVAL_ROOT}'"
      host "local"
      workspace "${EVAL_ROOT}"
      time-limit "1m"
    }
  }
}
"#,
    )
    .unwrap();
    let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
    let bundle_hash = hex::encode(Sha256::digest(&bundle));
    let app = router(state(root.path()));

    let request = || {
        serde_json::to_value(EvalStartRequest {
            name: "repeat".into(),
            bundle_hash: bundle_hash.clone(),
            bundle: bundle.clone(),
            inputs: BTreeMap::new(),
        })
        .unwrap()
    };
    let (first_status, first) = json_request(app.clone(), "/v1/evals", request()).await;
    let (second_status, second) = json_request(app, "/v1/evals", request()).await;

    assert_eq!(first_status, StatusCode::OK, "{first}");
    assert_eq!(second_status, StatusCode::OK, "{second}");
    assert_ne!(first["mission_run"], second["mission_run"]);
}

#[tokio::test]
async fn eval_entries_require_a_bounded_twenty_minute_timeout() {
    for (timeout, code) in [
        ("", "missing-eval-timeout"),
        (" timeout=\"21m\"", "eval-timeout-too-large"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let eval_dir = tempfile::tempdir().unwrap();
        fs::write(
            eval_dir.path().join("eval.kdl"),
            format!(
                r#"version 2
mission "eval/deadline" state="ready"{timeout} {{
  goal "Prove the eval deadline is explicit and bounded."
  completion {{ when "all-steps-exhausted" }}
  step "done" {{ agentless }}
}}
"#
            ),
        )
        .unwrap();
        let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
        let bundle_hash = hex::encode(Sha256::digest(&bundle));
        let app = router(state(root.path()));

        let (status, body) = json_request(
            app,
            "/v1/evals",
            serde_json::to_value(EvalStartRequest {
                name: "deadline".into(),
                bundle_hash,
                bundle,
                inputs: BTreeMap::new(),
            })
            .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], code, "{body}");
    }
}

#[test]
fn eval_fixtures_are_owned_without_replacing_selected_desired_state() {
    let current = parse_intent(
        r#"version 2
host "node" { document "doc/hosts/production@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }
"#,
        "node",
    )
    .unwrap()
    .subjects
    .into_values()
    .collect::<Vec<_>>();
    let mut conflicting = parse_intent(
        r#"version 2
host "local" { document "doc/hosts/eval@bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" }
"#,
        "node",
    )
    .unwrap();

    let error = scope_eval_desired_subjects(&mut conflicting, &current, "mission-run/eval-run")
        .unwrap_err();
    assert_eq!(error.code, "eval-desired-conflict");
    assert_eq!(current[0].subject, "host/node");
    assert_eq!(
        current[0].desired["children"][0]["arguments"][0],
        "doc/hosts/production@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );

    let mut fixture = parse_intent(
        r#"version 2
resource "eval/fixture" { kind "filesystem.file" }
agent "eval/fixture/worker" { workspace "/tmp"; command "true"; restart "never" }
"#,
        "node",
    )
    .unwrap();
    scope_eval_desired_subjects(&mut fixture, &current, "mission-run/eval-run").unwrap();
    assert_eq!(
        fixture.subjects["resource/eval/fixture"]
            .owner_run
            .as_deref(),
        Some("mission-run/eval-run")
    );
    assert_eq!(
        fixture.subjects["agent/eval/fixture/worker"]
            .owner_run
            .as_deref(),
        Some("mission-run/eval-run")
    );
}

#[test]
fn eval_reuses_an_identical_selected_declaration_without_owning_it() {
    let mut intent = parse_intent(
        r#"version 2
resource "shared" { kind "filesystem.file" }
"#,
        "node",
    )
    .unwrap();
    let current = intent.subjects.values().cloned().collect::<Vec<_>>();

    scope_eval_desired_subjects(&mut intent, &current, "mission-run/eval-run").unwrap();

    assert!(intent.subjects.is_empty());
}

#[tokio::test]
async fn an_eval_cannot_replace_selected_host_metadata() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let production = state
        .store
        .put_document(
            "doc/hosts/node",
            b"Production host facts.\n",
            &None,
            "production-host",
        )
        .unwrap();
    let source = format!(
        "version 2\nhost \"node\" {{ document \"doc/hosts/node@{}\" }}\n",
        production.hash
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &preview.subject_tokens, "publish-production-host")
        .unwrap();

    let eval_dir = tempfile::tempdir().unwrap();
    let eval_host = b"Eval host facts.\n";
    let eval_hash = hex::encode(Sha256::digest(eval_host));
    fs::create_dir_all(eval_dir.path().join(".st3-documents")).unwrap();
    fs::write(
        eval_dir.path().join(".st3-documents").join(&eval_hash),
        eval_host,
    )
    .unwrap();
    fs::write(
        eval_dir.path().join("eval.kdl"),
        format!(
            r#"version 2
host "local" {{ document "doc/hosts/eval@{eval_hash}" }}
mission "eval/host-conflict" state="ready" timeout="1m" {{
  goal "Do not replace production host metadata."
  completion {{ when "all-steps-exhausted" }}
  step "done" {{ agentless }}
}}
"#
        ),
    )
    .unwrap();
    let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
    let bundle_hash = hex::encode(Sha256::digest(&bundle));
    let app = router(state.clone());

    let (status, body) = json_request(
        app,
        "/v1/evals",
        serde_json::to_value(EvalStartRequest {
            name: "host-conflict".into(),
            bundle_hash,
            bundle,
            inputs: BTreeMap::new(),
        })
        .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "eval-desired-conflict");
    let selected = state
        .store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject == "host/node")
        .unwrap();
    assert_eq!(
        selected.desired["children"][0]["arguments"][0],
        format!("doc/hosts/node@{}", production.hash)
    );
}

#[tokio::test]
async fn an_eval_can_publish_ready_helper_missions() {
    let root = tempfile::tempdir().unwrap();
    let eval_dir = tempfile::tempdir().unwrap();
    fs::write(
        eval_dir.path().join("eval.kdl"),
        r#"
version 2

mission "eval/demo/helper" state="ready" {
  goal "Supply one helper mission."
  completion { when "all-steps-exhausted" }
  step "done" { agentless }
}

mission "eval/demo" state="ready" timeout="2m" {
  goal "Run the eval entry mission."
  completion { when "all-steps-exhausted" }
  step "done" { agentless }
}
"#,
    )
    .unwrap();
    let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
    let bundle_hash = hex::encode(Sha256::digest(&bundle));
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);

    let (status, body) = json_request(
        app,
        "/v1/evals",
        serde_json::to_value(EvalStartRequest {
            name: "demo".into(),
            bundle_hash,
            bundle,
            inputs: BTreeMap::new(),
        })
        .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let run = store
        .mission_run(body["mission_run"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.mission, "mission/eval/demo");
}

#[tokio::test]
async fn manual_work_wake_is_idempotent_and_uses_the_durable_driver_inbox() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2

mission "wake" state="ready" {
  goal "Exercise manual work wake."
  agent "worker" { workspace "/tmp"; command "true" }
  step "work" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "manual-wake-source")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "wake".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "manual-wake-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let step = &run.steps[0];
    let agent = format!("agent/{}/worker", run.id);
    state
        .store
        .set_step_state(&step.subject, "ready", None)
        .unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: agent.clone(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String("wake-worker".into())),
                (
                    "incarnation_id".into(),
                    Value::String("wake-incarnation".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("manual-wake-runtime".into()),
        })
        .unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: agent.clone(),
            kind: "harness.observed".into(),
            actor: Some(agent.clone()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("idle".into())),
                ("driver".into(), Value::String("codex".into())),
                (
                    "incarnation_id".into(),
                    Value::String("wake-incarnation".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("manual-wake-harness".into()),
        })
        .unwrap();

    let app = router(state.clone());
    let request = serde_json::to_value(WorkWakeRequest {
        actor: "person/operator".into(),
        reason: "the operator requested another delivery".into(),
        idempotency_key: "manual-wake-request".into(),
    })
    .unwrap();
    let path = format!("/v1/work/wake/{}", urlencoding::encode(&step.subject));
    let (status, first) = json_request(app.clone(), &path, request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "sent");
    assert!(
        first["tags"]
            .as_array()
            .unwrap()
            .contains(&Value::String("st3-wake-source:manual".into()))
    );
    let (status, repeated) = json_request(app, &path, request).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(repeated["subject"], first["subject"]);
    assert_eq!(state.store.messages(Some(&agent), true).unwrap().len(), 1);
    let app = router(state.clone());
    let (status, denied) = json_request(
        app.clone(),
        &path,
        serde_json::to_value(WorkWakeRequest {
            actor: "daemon/runtime".into(),
            reason: "neither a person nor an agent attempted a wake".into(),
            idempotency_key: "manual-wake-foreign".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{denied}");
    assert_eq!(denied["code"], "invalid-wake-actor");
    // Free mode: any agent wakes work assigned to another seat.
    for (n, actor) in [(2, "agent/other"), (3, "person/operator")] {
        let (status, wake) = json_request(
            app.clone(),
            &path,
            serde_json::to_value(WorkWakeRequest {
                actor: actor.into(),
                reason: "retry the manual delivery".into(),
                idempotency_key: format!("manual-wake-request-{n}"),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{wake}");
    }
    let projected = state.store.step_run(&step.subject).unwrap().unwrap();
    let wake = projected.wake.expect("wake projection");
    assert_eq!(
        wake.attempts, 0,
        "manual wakes must not use automatic attempts"
    );
    assert_eq!(wake.assignee_state, "idle");
}

fn checked_agent_cache(store: &Store, history: bool, index: u64) -> Vec<Value> {
    let cached = client_agent_resources_cached(store, history, index).unwrap();
    let mut direct = client_agent_resources_uncached(store, history, index).unwrap();
    add_agent_todos(store, &mut direct, index).unwrap();
    assert_eq!(cached, direct);
    cached
}

struct RosterClaimsFixture {
    store: Store,
    intent: crate::model::NormalizedIntent,
    subjects: Vec<String>,
    draining_subjects: Vec<String>,
    runs: Vec<crate::model::MissionRunView>,
}

struct RosterFixtureClock;

impl RosterFixtureClock {
    fn at(time: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(time));
        Self
    }
}

impl Drop for RosterFixtureClock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

fn roster_fixture_expire_leases(fixture: &RosterClaimsFixture) {
    let now = smallclaims::store::now_ms();
    let expires = fixture.runs.iter().flat_map(|run| &run.steps)
        .filter_map(|step| fixture.store.step_run(&step.subject).unwrap().unwrap()
            .claim_expires_at_unix_ms)
        .max().unwrap_or(now);
    let after = expires.max(now) + 1;
    smallclaims::store::set_thread_clock(Some(after));
    fixture.store.set_write_clock_at(after).unwrap();
}

fn roster_fixture_has_live_claim(step: &crate::model::StepRunView) -> bool {
    step.claimant.is_some() && step.claim_expires_at_unix_ms
        .is_some_and(|expires| expires > smallclaims::store::now_ms())
}

fn roster_fixture_append(store: &Store, subject: &str, kind: &str, fields: Value) {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
        fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
        expected_subject: None, idempotency_key: None,
    }).unwrap();
}

fn roster_fixture_apply(store: &Store, intent: &crate::model::NormalizedIntent, key: &str) {
    let plan = store.mission(intent, IntentInput {
        kdl: String::new(), source_name: Some("roster claims fixture".into()),
    }).unwrap();
    store.apply(intent, &plan.subject_tokens, key).unwrap();
}

fn roster_fixture_work(fixture: &RosterClaimsFixture, agent: usize, action: &str, key: &str) {
    fixture.store.work_action(&fixture.runs[agent / 10].steps[agent % 10].subject,
        action, &WorkRequest {
            wait_ms: None,
            actor: Some(fixture.subjects[agent].clone()),
            incarnation: Some(format!("roster-session-{agent}")),
            summary: Some(format!("Fixture {action}.")), reason: None,
            evidence: Vec::new(), idempotency_key: key.into(),
        }).unwrap();
}

fn roster_fixture_message_claim(
    store: &Store, message: &str, actor: &str, lifecycle: &str, fields: Value,
) {
    store.append_claim(&ClaimInput {
        subject: message.into(), kind: format!("message.{lifecycle}"), actor: Some(actor.into()),
        fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
        expected_subject: None, idempotency_key: None,
    }).unwrap();
}

fn roster_fixture_message_receipt(store: &Store, message: &str, lifecycle: &str) {
    let recipient = store.message(message).unwrap().unwrap().to;
    let fields = if lifecycle == "staged" {
        json!({"status":lifecycle,"recipient":recipient})
    } else {
        json!({"status":lifecycle})
    };
    // Native receipts name the recipient actor, not from/to endpoint fields.
    roster_fixture_message_claim(store, message, &recipient, lifecycle, fields);
}

fn roster_fixture_draining_seats(fixture: &RosterClaimsFixture) -> Vec<String> {
    use crate::store::owned_sets::{Options, Source};
    let store = &fixture.store;
    let subjects = (0..3).map(|agent| format!("agent/roster-draining/seat-{agent}"))
        .collect::<Vec<_>>();
    for sequence in 1..=2 {
        let mut source = String::from("version 2\n");
        for agent in 0..3 {
            source.push_str(&format!(
                "agent \"roster-draining/seat-{agent}\" {{ rollout \"manual\"; host \"node\"; \
                    workspace \"/tmp\"; harness \"claude\" {{ model \"fixture-{sequence}\"; }} }}\n"
            ));
        }
        let input = crate::graph::parse_owned_set_intent(&source, "node").unwrap();
        let mut options = Options {
            set: "roster-draining".into(),
            source: Source {
                repository: "fixture/roster".into(), r#ref: "refs/heads/main".into(),
                sha: format!("{sequence:040x}"), sequence,
            },
            expected_set: store.owned_sets().unwrap().first()
                .map_or("absent".into(), |set| set.revision.clone()),
            rollout: None, adopt: Default::default(), allow_empty: false,
            confirm_retire: None, expected_subjects: Default::default(),
        };
        let preview = store.owned_set_preview(&input, &options).unwrap();
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        options.confirm_retire = Some(preview.digest);
        options.expected_subjects = preview.expected_subjects;
        store.apply_owned_set(&input, &options, &format!("roster-draining-publish-{sequence}"),
            "person/test").unwrap();
        if sequence == 1 {
            for (agent, subject) in subjects.iter().enumerate() {
                let incarnation = format!("roster-draining-incarnation-{agent}");
                let token = store.selected_desired_token(subject).unwrap().unwrap();
                for (kind, fields) in [
                    ("runtime.observed", json!({"status":"running","runtime_id":subject,
                        "host":"node","incarnation_id":incarnation})),
                    ("runtime.action.succeeded", json!({"action":"start",
                        "desired_token":token,"incarnation_id":incarnation})),
                    ("harness.observed", json!({"state":"idle","driver":"claude",
                        "incarnation_id":incarnation,"quiescent":true,"blocking":[]})),
                    ("harness.session-file", json!({"harness":"claude",
                        "session_id":format!("roster-draining-native-{agent}"),
                        "incarnation_id":incarnation,"path":"/tmp/roster-draining-session"})),
                    ("harness.todo.observed", {
                        let mut todo = roster_fixture_todo(agent, 0);
                        todo["harness"] = json!("claude");
                        todo["session_id"] = json!(format!("roster-draining-native-{agent}"));
                        todo["incarnation_id"] = json!(incarnation);
                        todo
                    }),
                    ("harness.usage", json!({"semantics":"response","driver":"claude",
                        "incarnation_id":incarnation,"model":"fixture-1",
                        "input_tokens":10,"output_tokens":5,"total_tokens":15})),
                ] {
                    roster_fixture_append(store, subject, kind, fields);
                }
                // Replies to a pre-drain outgoing message remain eligible during draining.
                roster_fixture_message_claim(store, &format!("message/roster-draining-parent-{agent}"),
                    subject, "sent", json!({"status":"sent","from":subject,"to":"person/test",
                        "content":"Fixture question."}));
            }
        }
    }
    for (agent, subject) in subjects.iter().enumerate() {
        let incarnation = format!("roster-draining-incarnation-{agent}");
        let selected = store.rollout_selection(subject).unwrap().unwrap();
        assert!(selected.manual);
        let (_, old) = crate::rollout::launched_member(store, subject, &incarnation)
            .unwrap().unwrap();
        store.request_rollout(subject, &selected.desired_token, &old, &incarnation,
            "person/test", &crate::rollout::Policy::when_idle(1_800_000, false),
            &format!("roster-draining-request-{agent}")).unwrap();
        assert_eq!(store.rollout(subject).unwrap().unwrap().phase, "draining");
    }
    subjects
}

fn roster_fixture_pending_delivery(cards: &[Value], subject: &str) -> bool {
    let card = cards.iter().find(|card| card["id"] == subject).unwrap();
    assert_eq!(card["rollout"]["phase"], "draining");
    assert!(!card["todo"].is_null());
    assert!(!card["usage"].is_null());
    card["rollout"]["blocking"].as_array().unwrap().iter()
        .any(|blocker| blocker == "pending-delivery")
}

fn roster_fixture_checked_messages(
    fixture: &RosterClaimsFixture, subjects: &[String], pending: bool,
) {
    let store = &fixture.store;
    let index = store.index().unwrap();
    for history in [false, true] {
        let mut cold = client_agent_resources_uncached(store, history, index).unwrap();
        add_agent_todos(store, &mut cold, index).unwrap();
        for subject in subjects {
            assert_eq!(roster_fixture_pending_delivery(&cold, subject), pending);
        }
        let selected = subjects.iter().cloned().collect::<BTreeSet<_>>();
        let cached_selected = store.cached_agent_resources_for(index, history, Some(&selected), |changed| {
            let mut cards = client_agent_resources_selected(store, history, index, changed)?;
            add_agent_todos(store, &mut cards, index)?;
            Ok(cards)
        }).unwrap();
        let expected = cold.iter().filter(|card|
            selected.contains(card["id"].as_str().unwrap())).cloned().collect::<Vec<_>>();
        assert_eq!(cached_selected, expected, "selected history={history}");
        let cached = client_agent_resources_cached(store, history, index).unwrap();
        assert_eq!(cached, cold, "full history={history}");
    }
}

#[test]
fn agent_roster_draining_message_read_and_closed_match_cold_json() {
    let _clock = RosterFixtureClock::at(1_900_000_000_000);
    for initial_status in ["sent", "staged", "delivered"] {
        for terminal_status in ["read", "closed"] {
            let fixture = roster_claims_fixture();
            let subjects = roster_fixture_draining_seats(&fixture);
            for (agent, subject) in subjects.iter().enumerate() {
                let message = format!("message/roster-draining-reply-{agent}");
                roster_fixture_message_claim(&fixture.store, &message, "person/test", "sent",
                    json!({"status":"sent","from":"person/test","to":subject,"content":"Fixture reply.",
                        "in_reply_to":format!("message/roster-draining-parent-{agent}")}));
                if initial_status != "sent" {
                    roster_fixture_message_receipt(&fixture.store, &message, "staged");
                }
                if initial_status == "delivered" {
                    roster_fixture_message_receipt(&fixture.store, &message, "delivered");
                }
            }
            // Warm selected and full caches with the actual pending-delivery blocker.
            roster_fixture_checked_messages(&fixture, &subjects, true);
            for agent in 0..subjects.len() {
                let message = format!("message/roster-draining-reply-{agent}");
                if initial_status != "delivered" {
                    roster_fixture_message_receipt(&fixture.store, &message, "delivered");
                }
                roster_fixture_message_receipt(&fixture.store, &message, "read");
                if terminal_status == "closed" {
                    roster_fixture_message_receipt(&fixture.store, &message, "closed");
                }
                assert_eq!(fixture.store.message(&message).unwrap().unwrap().status, terminal_status);
            }
            roster_fixture_checked_messages(&fixture, &subjects, false);
        }
    }
}

#[test]
fn agent_roster_draining_message_staged_and_delivered_match_cold_json() {
    let _clock = RosterFixtureClock::at(1_900_000_000_000);
    for receipt in ["staged", "delivered"] {
        let fixture = roster_claims_fixture();
        let subjects = roster_fixture_draining_seats(&fixture);
        for (agent, subject) in subjects.iter().enumerate() {
            roster_fixture_message_claim(&fixture.store, &format!("message/roster-held-{agent}"),
                "person/test", "sent", json!({"status":"sent","from":"person/test","to":subject,
                    "content":"Independent held message."}));
        }
        // Independent sent messages are held; a receipt makes them drain blockers.
        roster_fixture_checked_messages(&fixture, &subjects, false);
        for agent in 0..subjects.len() {
            roster_fixture_message_receipt(&fixture.store,
                &format!("message/roster-held-{agent}"), receipt);
        }
        roster_fixture_checked_messages(&fixture, &subjects, true);
    }
}

fn roster_fixture_todo(agent: usize, revision: usize) -> Value {
    json!({
        "harness":"omp", "session_id":format!("roster-native-{agent}"),
        "incarnation_id":format!("roster-session-{agent}"),
        "observed_at":"2026-10-03T09:00:00Z", "source_op":"update",
        "phases":[{"name":"Work","tasks":[{"content":format!("Task {revision}"),
            "status":"in_progress"}]}],
        "totals":{"pending":0,"in_progress":1,"completed":0,"blocked":0,"abandoned":0},
        "truncated":false,
    })
}

fn roster_claims_fixture() -> RosterClaimsFixture {
    let store = Store::open_memory("node").unwrap();
    let subjects = (0..60).map(|n| format!("agent/node.roster-{n:02}")).collect::<Vec<_>>();
    store.set_write_clock_at(smallclaims::store::now_ms()).unwrap();
    let mut source = String::from("version 2\n");
    for n in 0..60 {
        source.push_str(&format!("agent \"roster-{n:02}\" {{ command \"true\" }}\n"));
    }
    for mission in 0..6 {
        source.push_str(&format!("mission \"roster-mission-{mission}\" state=\"ready\" {{\n\
                goal \"Exercise roster claim dependencies.\"\n"));
        for step in 0..10 {
            source.push_str(&format!("step \"work-{step:02}\" {{ assigned-to \"{}\" }}\n",
                subjects[mission * 10 + step]));
        }
        source.push_str("}\n");
    }
    let mut intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    roster_fixture_apply(&store, &intent, "roster-claims-initial");
    let runs = (0..6).map(|mission| {
        store.create_mission_run(&MissionRunRequest {
            mission: format!("roster-mission-{mission}"), revision: None, workspace: "/tmp".into(),
            requester: Some("person/test".into()), mode: Some("run".into()),
            inputs: BTreeMap::new(), idempotency_key: format!("roster-claims-run-{mission}"),
        }).unwrap()
    }).collect::<Vec<_>>();
    for (agent, subject) in subjects.iter().enumerate() {
        let run = &runs[agent / 10];
        let step = &run.steps[agent % 10];
        let desired = intent.subjects.get_mut(subject).unwrap();
        desired.owner_run = Some(run.subject.clone());
        desired.owner_generation = Some(run.generation.clone());
        desired.owner_step = Some(step.subject.clone());
        store.set_step_state(&step.subject, "ready", None).unwrap();
        for (kind, fields) in [
            ("runtime.observed", json!({"status":"running","runtime_id":subject,
                "incarnation_id":format!("roster-session-{agent}")})),
            ("harness.observed", json!({"state":"idle","driver":"omp",
                "incarnation_id":format!("roster-session-{agent}")})),
            ("harness.session-file", json!({"harness":"omp","agent":subject,
                "session_id":format!("roster-native-{agent}"),"path":"/tmp/roster-session"})),
            ("harness.todo.observed", roster_fixture_todo(agent, 0)),
            ("harness.usage", json!({"semantics":"response","driver":"omp",
                "incarnation_id":format!("roster-session-{agent}"),"model":"fixture",
                "input_tokens":10,"output_tokens":5,"total_tokens":15})),
        ] {
            roster_fixture_append(&store, subject, kind, fields);
        }
    }
    roster_fixture_apply(&store, &intent, "roster-claims-owners");
    let fixture = RosterClaimsFixture { store, intent, subjects, draining_subjects: Vec::new(), runs };
    for agent in (0..60).step_by(2) {
        roster_fixture_work(&fixture, agent, "claim", &format!("roster-initial-claim-{agent}"));
    }
    fixture
}

// Freeze the old policy here so the same measurement remains meaningful after cutover.
fn roster_old_cold_fallback(subject: &str, kind: &str) -> bool {
    kind != "daemon.diagnostic" && !(subject.starts_with("agent/") && matches!(kind,
        "runtime.observed" | "harness.observed" | "harness.diagnostic" |
        "harness.timeline" | "harness.todo.observed" | "harness.session-file" | "harness.usage"))
}

#[test]
fn agent_roster_typical_claim_kind_baseline() {
    let fixture = roster_claims_fixture();
    let store = &fixture.store;
    let before = store.index().unwrap();
    let local_before = store.connection.lock().expect("fixture writer").query_row(
        "SELECT COALESCE(MAX(id),0) FROM local_observations", [],
        |row| row.get::<_, u64>(0),
    ).unwrap();
    checked_agent_cache(store, false, before);
    let mut actual_cold_builds = 0_usize;
    let mut actual_selected_builds = 0_usize;
    for agent in 0..12 {
        roster_fixture_append(store, &fixture.subjects[agent], "harness.todo.observed",
            roster_fixture_todo(agent, 1));
        roster_fixture_append(store, &fixture.subjects[agent], "harness.usage",
            json!({"semantics":"response","driver":"omp",
                "incarnation_id":format!("roster-session-{agent}"),"model":"fixture",
                "input_tokens":20,"output_tokens":10,"total_tokens":30}));
        roster_fixture_work(&fixture, agent, if agent % 2 == 0 { "progress" } else { "claim" },
            &format!("roster-baseline-work-{agent}"));
        roster_fixture_append(store, "daemon/fixture", "daemon.diagnostic",
            json!({"code":"fixture","severity":"warning","reason":"Roster measurement."}));
        store.read_snapshot(|index| {
            let cached = store.cached_agent_resources(index, false, |changed| {
                if changed.is_some() { actual_selected_builds += 1; }
                else { actual_cold_builds += 1; }
                let mut rows = client_agent_resources_selected(store, false, index, changed)?;
                add_agent_todos(store, &mut rows, index)?;
                Ok(rows)
            })?;
            let mut cold = client_agent_resources_uncached(store, false, index)?;
            add_agent_todos(store, &mut cold, index)?;
            assert_eq!(cached, cold);
            Ok(())
        }).unwrap();
    }
    let connection = store.connection.lock().expect("fixture writer");
    let mut statement = connection.prepare(
        "SELECT subject,kind,COUNT(*) FROM claims WHERE store_index>?1 GROUP BY subject,kind",
    ).unwrap();
    let mut breakdown = BTreeMap::<String, (usize, usize)>::new();
    for row in statement.query_map([before], |row| Ok((
        row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, usize>(2)?,
    ))).unwrap() {
        let (subject, kind, count) = row.unwrap();
        let old_cold = roster_old_cold_fallback(&subject, &kind);
        let entry = breakdown.entry(kind).or_default();
        entry.0 += count;
        entry.1 += if old_cold { count } else { 0 };
    }
    // Latest-retention samples are real local observations even when the publisher
    // coalesces them instead of emitting another durable claim inside its interval.
    let mut local_statement = connection.prepare(
        "SELECT kind,COUNT(*) FROM local_observations WHERE id>?1 GROUP BY kind",
    ).unwrap();
    let local_breakdown = local_statement.query_map([local_before], |row| Ok((
        row.get::<_, String>(0)?, row.get::<_, usize>(1)?,
    ))).unwrap().collect::<rusqlite::Result<BTreeMap<_, _>>>().unwrap();
    let claims = breakdown.values().map(|(count, _)| count).sum::<usize>();
    let old_cold = breakdown.values().map(|(_, count)| count).sum::<usize>();
    println!("roster claim baseline: agents=60 missions=6 steps=60 initial_leases=30 \
            claims={claims} old_cold_fallback_claims={old_cold} \
            actual_cold_builds={actual_cold_builds} actual_selected_builds={actual_selected_builds}");
    for (kind, (count, cold)) in &breakdown {
        println!("roster claim kind: kind={kind} claims={count} old_cold_fallback={cold}");
    }
    for (kind, count) in &local_breakdown {
        println!("roster local observation kind: kind={kind} samples={count} old_cold_fallback=0");
    }
    assert_eq!(breakdown["work.claimed"], (6, 6));
    assert_eq!(breakdown["work.progress"], (6, 6));
    assert_eq!(breakdown["harness.todo.observed"], (12, 0));
    assert_eq!(local_breakdown["harness.usage"], 12);
    assert_eq!(old_cold, 12);
}

#[test]
fn faults_and_nudges_on_runs_and_steps_refold_only_the_cards_they_reach() {
    let fixture = roster_claims_fixture();
    let store = &fixture.store;
    for history in [false, true] {
        checked_agent_cache(store, history, store.index().unwrap());
    }
    let (run, step) = (&fixture.runs[1], &fixture.runs[2].steps[3]);
    roster_fixture_work(&fixture, 23, "claim", "fault-fixture-claim");
    for history in [false, true] {
        checked_agent_cache(store, history, store.index().unwrap());
    }
    let append = |subject: &str, kind: &str, actor: &str, fields: Value| {
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: Some(actor.into()),
            fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
            expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    let failure = |subject: &str, episode: &str| json!({
        "condition": "observed", "episode": episode, "incarnation": null,
        "reason": "Fixture fault.", "reviewer": "person/test", "severity": "warning",
        "source_revision": null, "targets": [subject], "title": "A fixture fault",
    });
    for (label, subject, kind, actor, fields) in [
        ("run fault", run.subject.as_str(), "operational.failure", "agent/st3/reconciler",
            failure(&run.subject, "fixture-run-fault")),
        ("step fault", step.subject.as_str(), "operational.failure", "agent/st3/reconciler",
            failure(&step.subject, "fixture-step-fault")),
        ("nudge", step.subject.as_str(), "work.nudged", "daemon/runtime", json!({
            "agent": fixture.subjects[23], "attempt": 1, "idle_since_unix_ms": "1",
            "message": "message/fixture", "reason": "quiet", "waits": [],
        })),
        ("report-to", run.subject.as_str(), "mission-run.report-to", "person/test",
            json!({"report_to": "agent/node.roster-00"})),
        ("eval verdict", run.subject.as_str(), "eval.verdict", "agent/st3/reconciler",
            json!({"verdict": "pass", "reason": "Fixture verdict."})),
    ] {
        append(subject, kind, actor, fields);
        let refolded = store.agent_resources_refolded_cards_for_test();
        for history in [false, true] {
            checked_agent_cache(store, history, store.index().unwrap());
        }
        let refolded = store.agent_resources_refolded_cards_for_test() - refolded;
        // Run 1 owns ten agents; step 2/3's holder is one. Never all sixty, in either view.
        assert!(refolded <= 2 * 11, "{label} refolded {refolded} cards");
    }
}

fn roster_fixture_person_claim(store: &Store, subject: &str, kind: &str, fields: Value) {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: kind.into(), actor: Some("person/test".into()),
        fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
        expected_subject: None, idempotency_key: None,
    }).unwrap();
}

fn roster_fixture_checked_cut(fixture: &RosterClaimsFixture, index: u64, seed: u64, turn: usize) {
    let store = &fixture.store;
    store.read_snapshot(|_| {
        for history in [false, true] {
            let mut cold = client_agent_resources_uncached(store, history, index)?;
            add_agent_todos(store, &mut cold, index)?;
            // Probe two disjoint pages before the complete roster: selected cache coverage
            // must grow without leaking other rows or seeding frozen pagination metadata.
            for remainder in [turn % 3, (turn + 1) % 3] {
                let selected = fixture.subjects.iter().chain(&fixture.draining_subjects).enumerate()
                    .filter(|(agent, _)| agent % 3 == remainder)
                    .map(|(_, subject)| subject.clone()).collect::<BTreeSet<_>>();
                let cached = store.cached_agent_resources_for(index, history, Some(&selected), |changed| {
                    let mut rows = client_agent_resources_selected(store, history, index, changed)?;
                    add_agent_todos(store, &mut rows, index)?;
                    Ok(rows)
                })?;
                let expected = cold.iter().filter(|row|
                    selected.contains(row["id"].as_str().unwrap())).cloned().collect::<Vec<_>>();
                assert_eq!(cached, expected, "selected seed={seed:#x} turn={turn} history={history}");
            }
            let cached = client_agent_resources_cached(store, history, index)?;
            assert_eq!(cached, cold, "full seed={seed:#x} turn={turn} history={history}");
        }
        Ok(())
    }).unwrap();
}

fn roster_fixture_random(state: &mut u64) -> u64 {
    // A fixed PRNG makes every failing interleaving reproducible without a new dependency.
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 4,
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x51a7_2026),
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn agent_roster_seeded_claim_sequences_match_cold_json(seed in 1u64..u64::MAX) {
        let _clock = RosterFixtureClock::at(1_900_000_000_000);
        let mut fixture = roster_claims_fixture();
        fixture.draining_subjects = roster_fixture_draining_seats(&fixture);
        let initial = fixture.store.index().unwrap();
        roster_fixture_checked_cut(&fixture, initial, seed, 0);
        let mut random = seed;
        let mut turn = 0;
        let mut exercised = BTreeSet::new();
        for round in 0..2 {
            // Shuffle a complete operation deck, rather than relying on chance to cover
            // a dependency. The seed chooses order, subjects and selected-page coverage.
            let mut operations = (0..27).collect::<Vec<_>>();
            for position in (1..operations.len()).rev() {
                let swap = (roster_fixture_random(&mut random) % (position + 1) as u64) as usize;
                operations.swap(position, swap);
            }
            for operation in operations {
                let agent = (roster_fixture_random(&mut random) % 60) as usize;
                let subject = fixture.subjects[agent].clone();
                let store = &fixture.store;
                let key = format!("roster-seed-{seed}-{turn}");
                let step = fixture.runs[agent / 10].steps[agent % 10].subject.clone();
                let incarnation = format!("roster-session-{agent}");
                exercised.insert(operation);
                match operation {
                    0 => roster_fixture_append(store, &subject, "harness.todo.observed",
                        roster_fixture_todo(agent, turn + 1)),
                    1 => roster_fixture_append(store, &subject, "harness.usage", json!({
                        "semantics":"response","driver":"omp","incarnation_id":incarnation,
                        "model":"fixture","input_tokens":turn + 1,"output_tokens":2,
                        "total_tokens":turn + 3})),
                    2 => roster_fixture_append(store, &subject, "harness.observed", json!({
                        "state":if round == 0 { "working" } else { "idle" },
                        "driver":"omp","incarnation_id":incarnation,"observed_at_ms":turn + 1})),
                    3 => roster_fixture_append(store, &subject, "runtime.observed", json!({
                        "status":"running","runtime_id":subject,"incarnation_id":incarnation})),
                    4 => {
                        let current = store.step_run(&step).unwrap().unwrap();
                        roster_fixture_work(&fixture, agent,
                            if roster_fixture_has_live_claim(&current) { "progress" } else { "claim" }, &key);
                    }
                    5 => {
                        let current = store.step_run(&step).unwrap().unwrap();
                        roster_fixture_work(&fixture, agent,
                            if roster_fixture_has_live_claim(&current) { "release" } else { "claim" }, &key);
                    }
                    6 => {
                        let desired = fixture.intent.subjects.get_mut(&subject).unwrap();
                        desired.set_display_name(Some(&format!("Declared {turn:03}"))).unwrap();
                        roster_fixture_apply(store, &fixture.intent, &key);
                    }
                    7 => {
                        store.rename_agent(&subject, Some(&format!("Seat {turn:03}")), &key).unwrap();
                    }
                    8 => {
                        // An undeclared agent crosses current/history membership boundaries.
                        roster_fixture_append(store, "agent/roster-extra", "runtime.observed",
                            json!({"status":if round == 0 { "running" } else { "stopped" },
                                "runtime_id":"roster-extra","incarnation_id":"extra-session"}));
                    }
                    9 => {
                        let run = &fixture.runs[agent / 10];
                        store.set_mission_run_state(&run.id, "running",
                            if round == 0 { "active" } else { "running" },
                            Some("Fixture owner state transition.")).unwrap();
                    }
                    10 => {
                        let run = &fixture.runs[agent / 10];
                        let generation = if run.generation.starts_with("run-generation/") {
                            run.generation.clone()
                        } else { format!("run-generation/{}", run.generation) };
                        roster_fixture_append(store, &generation, "run-generation.state",
                            json!({"status":"active","reason":format!("Fixture generation {turn}")}));
                    }
                    11 => {
                        let current = store.step_run(&step).unwrap().unwrap();
                        if roster_fixture_has_live_claim(&current) {
                            roster_fixture_work(&fixture, agent, "release", &format!("{key}-release"));
                        }
                        store.set_step_state(&step, "blocked", Some("Fixture readiness change.")).unwrap();
                        store.set_step_state(&step, "ready", None).unwrap();
                    }
                    12 => roster_fixture_append(store, "daemon/fixture", "daemon.diagnostic",
                        json!({"code":"fixture","severity":"warning","reason":format!("Diagnostic {turn}")})),
                    13 => roster_fixture_person_claim(store, &format!("message/roster-{seed}-{turn}"),
                        "message.sent", json!({"from":"person/test","to":subject,"status":"sent",
                            "content":format!("Fixture message {turn}")})),
                    14 => roster_fixture_person_claim(store,
                        &format!("glass/person/test/019a0000-0000-7000-8000-{turn:012x}"),
                        "glass.upserted", json!({"body":{"name":format!("Glass {turn}"),
                            "layout":{"tabs":[{"pane":"opaque:fixture"}]}},"base_revision":null})),
                    15 => roster_fixture_person_claim(store,
                        &format!("arrangement/person/test/019a0000-0000-7000-8000-{turn:012x}"),
                        "arrangement.edited", json!({"owner":"person/test",
                            "operations":[{"op":"create","name":format!("Arrangement {turn}")}]})),
                    16 => roster_fixture_append(store, "host/roster-peer", "fleet.member-endpoints",
                        json!({"member_key":"fixture-member","mode":"dial-out","endpoints":[]})),
                    17 => roster_fixture_append(store, &format!("custom/roster/{turn}"),
                        "custom.roster.note", json!({"value":turn})),
                    18 => {
                        let mut timeline = roster_local_timeline();
                        timeline.subject = subject.clone();
                        timeline.actor = Some(subject);
                        timeline.fields.insert("entry_id".into(), json!(key));
                        timeline.fields.insert("sequence".into(), json!(turn + 1));
                        timeline.fields.insert("observed_at_unix_ms".into(),
                            json!(1_900_000_000_000_u64 + turn as u64));
                        timeline.fields.insert("incarnation_id".into(), json!(incarnation));
                        let before = store.index().unwrap();
                        store.append_local_observations_for_test(&[timeline]);
                        assert_eq!(store.index().unwrap(), before);
                    }
                    19 => roster_fixture_append(store, &subject, "harness.diagnostic",
                        json!({"code":"fixture","severity":"warning","reason":format!("Harness diagnostic {turn}"),
                            "driver":"omp","incarnation_id":incarnation})),
                    20 => {
                        store.move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
                            agent: subject, run: fixture.runs[agent / 10].id.clone(),
                            placement: if round == 0 { "top" } else { "bottom" }.into(),
                            anchor: None, reason: Some("Fixture queue move.".into()),
                            actor: "person/test".into(), idempotency_key: key,
                        }).unwrap();
                    }
                    21 => roster_fixture_expire_leases(&fixture),
                    22 => {
                        let mut timeline = roster_local_timeline();
                        timeline.subject = subject.clone();
                        timeline.actor = Some(subject);
                        timeline.fields.insert("entry_id".into(), json!(key));
                        timeline.fields.insert("incarnation_id".into(), json!(incarnation));
                        timeline.fields.insert("sequence".into(), json!(turn + 1));
                        timeline.fields.insert("observed_at_unix_ms".into(),
                            json!(smallclaims::store::now_ms() as u64));
                        store.append_claim(&timeline).unwrap();
                    }
                    23..=26 => {
                        let draining_agent = (roster_fixture_random(&mut random)
                            % fixture.draining_subjects.len() as u64) as usize;
                        let recipient = &fixture.draining_subjects[draining_agent];
                        let message = format!("message/{key}-lifecycle");
                        roster_fixture_message_claim(store, &message, "person/test", "sent",
                            json!({"status":"sent","from":"person/test","to":recipient,
                                "content":format!("Fixture lifecycle {turn}"),
                                "in_reply_to":format!("message/roster-draining-parent-{draining_agent}")}));
                        let receipts = ["staged", "delivered", "read", "closed"];
                        let target = operation - 23;
                        // Establish a schema-valid predecessor for this deck operation.
                        for receipt in &receipts[..target] {
                            roster_fixture_message_receipt(store, &message, receipt);
                        }
                        roster_fixture_checked_cut(&fixture, store.index().unwrap(), seed, turn);
                        // Compare the target transition and its remaining lifecycle separately:
                        // no later message can mask an earlier stale pending-delivery card.
                        for receipt in &receipts[target..] {
                            roster_fixture_message_receipt(store, &message, receipt);
                            roster_fixture_checked_cut(&fixture, store.index().unwrap(), seed, turn);
                        }
                    }
                    _ => unreachable!(),
                }
                turn += 1;
                roster_fixture_checked_cut(&fixture, fixture.store.index().unwrap(), seed, turn);
                if turn % 10 == 0 {
                    // The oracle and cache read the identical historical graph cut and
                    // eligible local frontier, not an oracle silently advanced to "now".
                    roster_fixture_checked_cut(&fixture, initial, seed, turn);
                }
            }
        }
        assert_eq!(exercised.len(), 27);
        let cards = checked_agent_cache(&fixture.store, true, fixture.store.index().unwrap());
        assert!(cards.iter().any(|card| !card["todo"].is_null()),
            "fixture must compare actual todo snapshots, not absent fields");
        assert!(cards.iter().any(|card| !card["usage"].is_null()),
            "fixture must compare actual usage summaries, not absent fields");
        // End owners only after the shuffled work operations. The two history modes
        // must agree with their own oracle when declared seats leave the current layer.
        fixture.store.set_mission_run_state(&fixture.runs[0].id, "cancelled", "terminal",
            Some("Fixture run retirement.")).unwrap();
        roster_fixture_checked_cut(&fixture, fixture.store.index().unwrap(), seed, turn + 1);
        let generation = &fixture.runs[1].generation;
        let generation = if generation.starts_with("run-generation/") {
            generation.clone()
        } else { format!("run-generation/{generation}") };
        roster_fixture_append(&fixture.store, &generation, "run-generation.state",
            json!({"status":"superseded","reason":"Fixture generation retirement."}));
        roster_fixture_checked_cut(&fixture, fixture.store.index().unwrap(), seed, turn + 2);
        roster_fixture_checked_cut(&fixture, initial, seed, turn + 3);
    }
}

#[test]
#[ignore = "focused step-claim roster timing; run explicitly with --ignored --nocapture"]
fn agent_roster_one_step_claim_fixture_timing() {
    for history in [false, true] {
        let fixture = roster_claims_fixture();
        let store = &fixture.store;
        let before = store.index().unwrap();
        checked_agent_cache(store, history, before);
        roster_fixture_work(&fixture, 1, "claim", "roster-timing-one-step");
        store.read_snapshot(|index| {
            let before_refolds = store.agent_resources_refolded_cards_for_test();
            let start = Instant::now();
            let incremental = client_agent_resources_cached(store, history, index)?;
            let incremental_time = start.elapsed();
            let refolded_cards = store.agent_resources_refolded_cards_for_test() - before_refolds;
            let start = Instant::now();
            let mut cold = client_agent_resources_uncached(store, history, index)?;
            add_agent_todos(store, &mut cold, index)?;
            let cold_time = start.elapsed();
            assert_eq!(incremental, cold);
            println!("roster step-claim timing: agents=60 missions=6 steps=60 history={history} \
                    cold_ms={:.3} incremental_ms={:.3} refolded_cards={refolded_cards}",
                cold_time.as_secs_f64() * 1000.0, incremental_time.as_secs_f64() * 1000.0);
            assert_eq!(refolded_cards, 1, "one step claim must refold only its assigned agent");
            Ok(())
        }).unwrap();
    }
}

#[test]
fn agent_roster_claim_fixture_refreshes_at_lease_expiry_without_new_claims() {
    let _clock = RosterFixtureClock::at(1_900_000_000_000);
    let fixture = roster_claims_fixture();
    let store = &fixture.store;
    let step = &fixture.runs[0].steps[0].subject;
    let expires = store.step_run(step).unwrap().unwrap().claim_expires_at_unix_ms.unwrap();
    let index = store.index().unwrap();
    for history in [false, true] {
        let cards = checked_agent_cache(store, history, index);
        let card = cards.iter().find(|card| card["id"] == fixture.subjects[0]).unwrap();
        assert_eq!(card["current_work_ids"], json!([step]));
    }
    assert_eq!(store.agent_roster_valid_until(index), Some(expires));
    roster_fixture_expire_leases(&fixture);
    assert_eq!(store.index().unwrap(), index);
    for history in [false, true] {
        assert!(store.agent_resources_cached_at(index, history, None).unwrap().is_none(),
            "the expired queue deadline must invalidate a warm roster at the same cut");
        let refolds = store.agent_resources_refolded_cards_for_test();
        let cards = checked_agent_cache(store, history, index);
        assert_eq!(store.agent_resources_refolded_cards_for_test() - refolds, 30,
            "expiry must refold the 30 leased cards, not all 60 agents");
        let card = cards.iter().find(|card| card["id"] == fixture.subjects[0]).unwrap();
        assert_eq!(card["current_work_ids"], json!([]));
        assert_eq!(card["next_work_id"], step.as_str());
    }
}

fn roster_followup_store() -> Store {
    let store = Store::open_memory("node").unwrap();
    let source =
        "version 2\nagent \"amber\" { command \"true\" }\nagent \"cobalt\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &plan.subject_tokens, "roster-followup").unwrap();
    store.append_claim(&roster_local_observation(
        "runtime.observed",
        json!({"status":"running", "runtime_id":"node.amber",
            "incarnation_id":"amber-1"}),
    )).unwrap();
    store.append_claim(&roster_local_observation(
        "harness.observed",
        json!({"state":"idle", "driver":"codex", "incarnation_id":"amber-1",
            "observed_at_ms":1}),
    )).unwrap();
    store
}

fn roster_local_observation(kind: &str, fields: Value) -> ClaimInput {
    ClaimInput {
        subject: "agent/node.amber".into(),
        kind: kind.into(),
        actor: Some("agent/node.amber".into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }
}

fn roster_local_timeline() -> ClaimInput {
    roster_local_observation("harness.timeline", json!({
        "operation":"append", "entry_id":"local-activity", "source_id":"fixture/local-activity",
        "sequence":1, "revision":1, "role":"assistant", "entry_type":"content",
        "final":true, "driver":"codex", "incarnation_id":"amber-1",
        "observed_at_unix_ms":1_900_000_000_000_u64, "body":{"text":"local activity"},
    }))
}

#[test]
fn agent_roster_cached_projection_matches_uncached_after_local_timeline() {
    let store = roster_followup_store();
    let index = store.index().unwrap();
    let before = [false, true].map(|history| checked_agent_cache(&store, history, index));
    store.append_local_observations_for_test(&[roster_local_timeline()]);
    assert_eq!(store.index().unwrap(), index, "local activity must preserve the graph cut");
    for (history, original) in [false, true].into_iter().zip(before) {
        let updated = checked_agent_cache(&store, history, index);
        let card = |cards: &[Value], subject: &str| {
            cards.iter().find(|card| card["id"] == subject).unwrap().clone()
        };
        assert_ne!(
            card(&updated, "agent/node.amber")["last_activity_at"],
            card(&original, "agent/node.amber")["last_activity_at"],
        );
        assert_eq!(
            card(&updated, "agent/node.cobalt"),
            card(&original, "agent/node.cobalt"),
        );
    }
}

#[tokio::test]
async fn warm_agent_http_page_bypasses_unrelated_cold_roster_admission() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = "version 2\nagent \"warm\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = state.store.mission(&intent, crate::model::IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    state.store.apply(&intent, &plan.subject_tokens, "warm-admission").unwrap();
    let (_, Json(cold)) = client_agents(
        State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery::default()),
    ).await.unwrap();
    assert_eq!(cold.items.len(), 1);
    let admission = state.store.admit_agent_resources("other").await;
    let started = Instant::now();
    let (_, Json(warm)) = tokio::time::timeout(
        Duration::from_secs(2),
        client_agents(
            State(state.clone()), Extension(new_client_snapshot(&state)),
            Query(ClientListQuery::default()),
        ),
    ).await.expect("warm HTTP page must not wait for an unrelated cold builder").unwrap();
    assert_eq!(warm.items, cold.items);
    println!("roster warm HTTP fixture: held_cold_admission=true elapsed_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0);
    drop(admission);
}

#[test]
fn agent_roster_heartbeat_only_local_stream_hit_rate() {
    let store = roster_followup_store();
    let index = store.index().unwrap();
    let selected = BTreeSet::from(["agent/node.amber".to_string()]);
    let pinned = [false, true].map(|history| {
        checked_agent_cache(&store, history, index);
        store.read_snapshot(|_| {
            Ok(store.agent_resources_cached_at(index, history, None)?.unwrap())
        }).unwrap()
    });
    const APPENDS: usize = 64;
    let started = Instant::now();
    let mut hits = 0;
    for n in 0..APPENDS {
        let response = store.append_claim(&roster_local_observation(
            "harness.observed",
            json!({"state":"idle", "driver":"codex", "incarnation_id":"amber-1",
                "observed_at_ms":300_001 + n as u64}),
        )).unwrap();
        assert!(crate::store::local_observation_position(&response).is_some());
        assert_eq!(store.index().unwrap(), index);
        for (history, original) in [false, true].into_iter().zip(&pinned) {
            store.read_snapshot(|_| {
                let hit = store.agent_resources_cached_at(index, history, Some(&selected))?;
                let hit = hit.expect("heartbeat-only append must reuse the roster");
                assert!(Arc::ptr_eq(&hit, original), "read-only hits must pin the same rows");
                assert_eq!(hit.len(), 2, "selected coverage must not filter the pinned Arc");
                hits += 1;
                Ok(())
            }).unwrap();
        }
    }
    let probes = APPENDS * pinned.len();
    println!(
        "roster heartbeat fixture: appends={APPENDS} probes={probes} hits={hits} hit_rate={:.1}% elapsed_ms={:.3}",
        100.0 * hits as f64 / probes as f64,
        started.elapsed().as_secs_f64() * 1000.0,
    );
    assert_eq!(hits, probes);
    store.append_local_observations_for_test(&[roster_local_timeline()]);
    assert_eq!(store.index().unwrap(), index);
    for history in [false, true] {
        store.read_snapshot(|_| {
            assert!(store.agent_resources_cached_at(index, history, None)?.is_none(),
                "timeline activity must miss even at the same graph cut");
            Ok(())
        }).unwrap();
        checked_agent_cache(&store, history, index);
        store.read_snapshot(|_| {
            assert!(store.agent_resources_cached_at(index, history, None)?.is_some(),
                "the rebuilt timeline cut must become reusable");
            Ok(())
        }).unwrap();
    }
}

#[tokio::test]
async fn agent_roster_cache_reuse_preserves_changed_for_existing_subscribers() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.store = Arc::new(roster_followup_store());
    let index = state.store.index().unwrap();
    checked_agent_cache(&state.store, false, index);
    let original = state.store.agent_resources_cached_at(index, false, None)
        .unwrap().unwrap();
    let mut subscribers = [
        state.event_notify.subscribe(),
        state.event_notify.subscribe(),
    ];
    for request in [
        roster_local_observation("harness.observed", json!({
            "state":"idle", "driver":"codex", "incarnation_id":"amber-1",
            "observed_at_ms":300_001,
        })),
        roster_local_timeline(),
    ] {
        let (response, appended) = state.store.append_claim_outcome(&request).unwrap();
        assert!(appended);
        assert!(crate::store::local_observation_position(&response).is_some());
        assert_eq!(state.store.index().unwrap(), index);
        let _ = finish_claim_publication(&state, &request.kind, response, appended, None)
            .await.unwrap();
        for subscriber in &mut subscribers {
            tokio::time::timeout(Duration::from_secs(1), subscriber.changed())
                .await.expect("cache reuse must not suppress a subscriber wake")
                .unwrap();
        }
        let cached = state.store.agent_resources_cached_at(index, false, None).unwrap();
        if request.kind == "harness.observed" {
            assert!(Arc::ptr_eq(&cached.unwrap(), &original));
        } else {
            assert!(cached.is_none(), "timeline publication must invalidate the roster");
            checked_agent_cache(&state.store, false, index);
        }
    }
}

#[test]
fn agent_roster_refresh_publishes_an_empty_or_small_roster_at_once() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    for history in [false, true] {
        refresh_agent_roster(store, history).unwrap();
        let (cut, cards, _) = store.published_agent_roster(store.index().unwrap(), history).unwrap();
        assert_eq!((cut, cards.len()), (store.index().unwrap(), 0), "history {history}");
    }
    store.append_claim(&ClaimInput {
        subject: "agent/small-roster".into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(json!({"status":"running", "runtime_id":"small",
            "incarnation_id":"one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    for history in [false, true] {
        refresh_agent_roster(store, history).unwrap();
        let (_, cards, _) = store.published_agent_roster(store.index().unwrap(), history).unwrap();
        assert_eq!(cards.len(), 1, "history {history}");
    }
}
fn roster_resilience_runtime(store: &Store, subject: &str, runtime: &str) {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(json!({"status":"running", "runtime_id":runtime,
            "incarnation_id":"one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
}

fn roster_resilience_ended(store: &Store) {
    store.append_claim(&ClaimInput {
        subject: "agent/ended".into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(json!({"status":"stopped", "terminal":true,
            "runtime_id":"ended", "incarnation_id":"ended"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
}

#[tokio::test]
async fn roster_resilience_fault_deadline_publishes_current_fields_and_retries() {
    for history in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        roster_resilience_runtime(store, "agent/retained-fault", "old");
        roster_resilience_runtime(store, "agent/known-clear", "old");
        let failure = store.append_claim(&ClaimInput {
            subject: "agent/retained-fault".into(), kind: "operational.failure".into(),
            actor: Some("daemon/runtime".into()),
            fields: serde_json::from_value(json!({"condition":"mailbox-channel-lost",
                "incarnation":"one", "reason":"previous failure", "reviewer":"agent/retained-fault",
                "title":"Mailbox lost", "severity":"error", "targets":["agent/retained-fault"]})).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        refresh_agent_roster(store, history).unwrap();
        let (_, before, _) = store.published_agent_roster(store.index().unwrap(), history).unwrap();
        let card = |cards: &[Value], id: &str| cards.iter().find(|card| card["id"] == id).unwrap().clone();
        assert_eq!(card(&before, "agent/retained-fault")["fault"], "previous failure");
        assert!(card(&before, "agent/known-clear")["fault"].is_null());
        store.append_claim(&ClaimInput {
            subject: "agent/retained-fault".into(), kind: "operational.recovered".into(),
            actor: Some("daemon/runtime".into()),
            fields: serde_json::from_value(json!({"failure":failure.id, "reason":"recovered"})).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        for subject in ["agent/retained-fault", "agent/known-clear", "agent/new-unknown"] {
            roster_resilience_runtime(store, subject, "new");
        }
        Store::expire_next_card_fault_read_for_test();
        refresh_agent_roster(store, history).expect("an optional deadline must still publish");
        let cut = store.index().unwrap();
        let (published, after, _) = store.published_agent_roster(cut, history).unwrap();
        assert_eq!(published, cut);
        for subject in ["agent/retained-fault", "agent/known-clear", "agent/new-unknown"] {
            assert_eq!(card(&after, subject)["runtime_ids"], json!(["runtime/new"]));
        }
        assert_eq!(card(&after, "agent/retained-fault")["fault"], "previous failure");
        assert!(card(&after, "agent/known-clear")["fault"].is_null());
        assert_eq!(card(&after, "agent/new-unknown")["fault"], "fault status unknown");
        let (Extension(snapshot), Json(page)) = client_agents(State(state.clone()),
            Extension(new_client_snapshot(&state)), Query(ClientListQuery { history, fresh: true, ..ClientListQuery::default() }))
            .await.expect("a deadline-degraded publication must answer, not return 503");
        assert_eq!(snapshot.store_index, cut);
        assert_eq!(card(&page.items, "agent/retained-fault")["fault"], "previous failure");
        // No new claim is needed: the next refresh retries exactly the degraded subjects.
        refresh_agent_roster(store, history).unwrap();
        let (_, retried, _) = store.published_agent_roster(cut, history).unwrap();
        for subject in ["agent/retained-fault", "agent/known-clear", "agent/new-unknown"] {
            assert!(card(&retried, subject)["fault"].is_null());
        }
    }
}

#[tokio::test]
async fn roster_resilience_always_expired_fault_reads_publish_from_the_first_attempt_per_mode() {
    for history in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        assert!(store.published_agent_roster(store.index().unwrap(), history).is_none());
        Store::expire_card_fault_reads_for_test(usize::MAX);
        for round in 0..3 {
            if round == 1 {
                store.forget_current_views();
                assert!(store.published_agent_roster(store.index().unwrap(), history).is_none(),
                    "a full reset must keep its existing invalidation policy");
            }
            let runtime = format!("round-{round}");
            roster_resilience_runtime(store, "agent/first-publication", &runtime);
            refresh_agent_roster(store, history)
                .expect("a fault deadline from the first attempt must not prevent publication");
            let cut = store.index().unwrap();
            let (published, _, _) = store.published_agent_roster(cut, history).unwrap();
            assert_eq!(published, cut);
            let (status, page) = get_request(router(state.clone()),
                &format!("/v1/client/agents?history={history}&fresh=true")).await;
            assert_eq!(status, StatusCode::OK, "never-published fault degradation must answer 200");
            assert_eq!(page["items"][0]["runtime_ids"], json!([format!("runtime/{runtime}")]));
            assert_eq!(page["items"][0]["fault"], "fault status unknown");
        }
        Store::expire_card_fault_reads_for_test(0);
    }
}

#[tokio::test(start_paused = true)]
async fn roster_resilience_unpublished_mode_keeps_the_bounded_wait_and_503() {
    for history in [false, true] {
        for forget in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = state(root.path());
            let store = &state.store;
            let _wake = store.start_agent_roster_refresher().unwrap();
            roster_resilience_runtime(store, "agent/current-only", "current");
            if history || forget {
                refresh_agent_roster(store, false).unwrap();
            }
            if forget {
                store.forget_current_views();
            }
            assert!(store.published_agent_roster(store.index().unwrap(), history).is_none());
            assert!(store.answer_agent_roster_requests::<()>(||
                anyhow::bail!("injected refresh failure")).is_err());
            let started = tokio::time::Instant::now();
            let request = client_agents(
                State(state.clone()), Extension(new_client_snapshot(&state)), Query(ClientListQuery {
                    history, fresh: true, ..ClientListQuery::default()
                }));
            tokio::pin!(request);
            tokio::select! {
                _ = &mut request => panic!("an unpublished mode answered before its bounded wait"),
                _ = tokio::time::sleep(AGENT_ROSTER_READ_WAIT - Duration::from_millis(1)) => {}
            }
            let error = match request.await {
                Ok(_) => panic!("an unpublished mode must return 503"),
                Err(error) => error,
            };
            assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(error.code, "agent-roster-not-ready");
            assert_eq!(started.elapsed(), AGENT_ROSTER_READ_WAIT);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn named_roster_future_cut_is_rejected_without_waiting_or_refreshing() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let wake = store.start_agent_roster_refresher().unwrap();
    let head = store.index().unwrap();
    for history in [false, true] {
        for fresh in [false, true] {
            let started = tokio::time::Instant::now();
            let error = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
                Query(ClientListQuery { history, fresh, min_cut: Some(u64::MAX),
                    ..ClientListQuery::default() })).await.unwrap_err();
            assert_eq!(started.elapsed(), Duration::ZERO);
            assert_eq!(error.code, "agent-roster-cut-not-ready");
            assert_eq!(error.details["min_cut"], u64::MAX);
            assert_eq!(error.details["head"], head);
        }
    }
    assert!(tokio::time::timeout(Duration::from_millis(1), wake.notified()).await.is_err(),
        "an impossible cut must not request a refresh");
}

#[tokio::test(start_paused = true)]
async fn named_roster_cut_timeout_has_one_timer_even_with_legacy_fresh_and_failure() {
    for history in [false, true] {
        for fresh in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = state(root.path());
            let store = &state.store;
            let _wake = store.start_agent_roster_refresher().unwrap();
            roster_resilience_runtime(store, "agent/cut-timeout", "old");
            refresh_agent_roster(store, history).unwrap();
            roster_resilience_runtime(store, "agent/cut-timeout", "new");
            let wanted = store.index().unwrap();
            assert!(store.answer_agent_roster_requests::<()>(||
                anyhow::bail!("previous failure cannot satisfy a named cut")).is_err());
            let started = tokio::time::Instant::now();
            let error = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
                Query(ClientListQuery { history, fresh, min_cut: Some(wanted),
                    ..ClientListQuery::default() })).await.unwrap_err();
            // This timeout returns before blocking_roster: paused time is exact here.
            assert_eq!(started.elapsed(), AGENT_ROSTER_READ_WAIT, "one fixed timer");
            assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(error.code, "agent-roster-cut-not-ready");
            assert_eq!(error.details["min_cut"], wanted);
            let (status, _) = get_request(router(state), "/v1/health").await;
            assert_eq!(status, StatusCode::OK, "a refused cut must leave the daemon healthy");
        }
    }
}

#[tokio::test]
async fn named_roster_cut_uses_its_mutation_receipt_not_later_head_or_legacy_wait() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    roster_resilience_runtime(store, "agent/receipt-cut", "old");
    refresh_agent_roster(store, false).unwrap();
    let (status, receipt) = json_request(router(state.clone()), "/v1/claims", json!({
        "subject":"agent/receipt-cut", "actor":"agent/receipt-cut", "kind":"runtime.observed", "fields":{
            "status":"running", "runtime_id":"receipt-new", "incarnation_id":"one"}
    })).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let wanted = receipt["store_index"].as_u64().expect("the same host's acceptance receipt");
    let read = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery { min_cut: Some(wanted), ..ClientListQuery::default() }));
    tokio::pin!(read);
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut read).await.is_err(),
        "the mutation receipt must not be satisfied by the old publication");
    refresh_agent_roster(store, false).unwrap();
    let (Extension(snapshot), Json(page)) = read.await.unwrap();
    assert_eq!(snapshot.store_index, wanted);
    assert_eq!(page.items[0]["runtime_ids"], json!(["runtime/receipt-new"]));
    for n in 0..3 {
        store.append_claim(&ClaimInput {
            subject: format!("custom/test/after-cut-{n}"), kind: "custom.test.marker".into(),
            actor: None, fields: BTreeMap::new(), evidence: Vec::new(),
            expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    assert!(store.index().unwrap() > wanted);
    let (Extension(snapshot), Json(page)) = tokio::time::timeout(Duration::from_millis(500),
        client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
            Query(ClientListQuery { fresh: true, min_cut: Some(wanted),
                ..ClientListQuery::default() }))).await
        .expect("an already satisfied named cut must add no legacy wait").unwrap();
    assert_eq!(snapshot.store_index, wanted);
    assert_eq!(page.items[0]["runtime_ids"], json!(["runtime/receipt-new"]));
}

#[tokio::test]
async fn named_roster_cut_rechecks_the_served_page_after_wait_and_publication_reset() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    roster_resilience_runtime(store, "agent/reset-cut", "old");
    refresh_agent_roster(store, false).unwrap();
    let older = client_agents_published_page(&state, &ClientListQuery::default(), 0, None).unwrap();
    roster_resilience_runtime(store, "agent/reset-cut", "new");
    refresh_agent_roster(store, false).unwrap();
    let wanted = store.index().unwrap();
    assert!(wait_for_agent_roster_cut(store, false, wanted).await);
    let error = checked_agent_roster_page(older, Some(wanted)).unwrap_err();
    assert_eq!(error.code, "agent-roster-cut-not-ready");
    // Exercise the exact post-wait reset seam without adding a production test hook.
    store.forget_current_views();
    let page = client_agents_published_page(&state, &ClientListQuery::default(), wanted, None).unwrap();
    assert!(page.is_none());
    let error = checked_agent_roster_page(page, Some(wanted)).unwrap_err();
    assert_eq!(error.code, "agent-roster-cut-not-ready");
    assert!(checked_agent_roster_page(None, None).unwrap().is_none());
}

#[tokio::test]
async fn named_roster_cut_rejects_a_continuation_before_decoding_its_cursor() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (status, error) = get_request(router(state),
        "/v1/client/agents?min_cut=0&fresh=true&cursor=not-a-cursor").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["code"], "validation-failed");
    assert!(error["message"].as_str().unwrap().contains("first agents page"));
}

#[test]
fn roster_resilience_chunk_deadlines_publish_without_retrying_in_the_same_attempt() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    let count = AGENT_ROSTER_WARM_CHUNK * 2 + 1;
    for n in 0..count {
        roster_resilience_runtime(store, &format!("agent/chunk-{n:04}"), "old");
    }
    refresh_agent_roster(store, false).unwrap();
    for n in 0..count {
        roster_resilience_runtime(store, &format!("agent/chunk-{n:04}"), "new");
    }
    let last = format!("agent/chunk-{:04}", count - 1);
    store.append_claim(&ClaimInput {
        subject: last.clone(), kind: "operational.failure".into(), actor: Some("daemon/runtime".into()),
        fields: serde_json::from_value(json!({"condition":"mailbox-channel-lost",
            "incarnation":"one", "reason":"new failure", "reviewer":last,
            "title":"Mailbox lost", "severity":"error", "targets":[last]})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let before = store.agent_resources_refolded_cards_for_test();
    Store::expire_card_fault_reads_for_test(3);
    refresh_agent_roster(store, false).expect("degrading every chunk must still complete this attempt");
    let cut = store.index().unwrap();
    let (published, cards, _) = store.published_agent_roster(cut, false).unwrap();
    assert_eq!(published, cut);
    assert_eq!(cards.len(), count);
    assert!(cards.iter().all(|card| card["runtime_ids"] == json!(["runtime/new"]) && card["fault"].is_null()));
    assert_eq!(store.agent_resources_refolded_cards_for_test() - before, count,
        "a deadline must not trigger another fold inside this refresh attempt");
    let before = store.agent_resources_refolded_cards_for_test();
    refresh_agent_roster(store, false).unwrap();
    assert_eq!(store.agent_resources_refolded_cards_for_test() - before, count,
        "the next attempt must retry the faults even at the same graph cut");
    let (_, cards, _) = store.published_agent_roster(cut, false).unwrap();
    assert_eq!(cards.iter().find(|card| card["id"] == last).unwrap()["fault"], "new failure",
        "a same-cut complete publication must replace the old degraded fault values");
}

async fn assert_roster_resilience_failed_attempt(history: bool) {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    roster_resilience_runtime(store, "agent/retained", "old");
    roster_resilience_ended(store);
    refresh_agent_roster(store, history).unwrap();
    let (cut, cards, published_at) = store.published_agent_roster(store.index().unwrap(), history).unwrap();
    assert_eq!(cards.iter().any(|card| card["id"] == "agent/ended"), history);
    let (_, Json(before)) = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery { history, ..ClientListQuery::default() })).await.unwrap();
    roster_resilience_runtime(store, "agent/retained", "new");
    assert!(store.answer_agent_roster_requests::<()>(|| anyhow::bail!("injected refresh failure")).is_err());
    for status in [None, Some("running".to_owned()), Some("stopped".to_owned())] {
        let expected = before.items.iter().filter(|card| status.as_deref().is_none_or(|status| card["state"] == status))
            .cloned().collect::<Vec<_>>();
        let read = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
            Query(ClientListQuery { history, status, fresh: true, ..ClientListQuery::default() }));
        let (Extension(snapshot), Json(page)) = tokio::time::timeout(Duration::from_millis(200), read).await
            .expect("a failed refresher must not impose the two-second wait").unwrap();
        assert_eq!(snapshot.store_index, cut);
        assert_eq!(snapshot.published_at, Some(client_timestamp(published_at)));
        assert_eq!(page.items, expected);
    }
    store.answer_agent_roster_requests(|| Ok(())).unwrap();
    let reader = Arc::clone(store);
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, proceed) = std::sync::mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || reader.answer_agent_roster_requests::<()>(|| {
        started.send(()).unwrap();
        proceed.recv().unwrap();
        anyhow::bail!("injected in-flight failure")
    }));
    ready.await.unwrap();
    let read = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery { history, fresh: true, ..ClientListQuery::default() }));
    tokio::pin!(read);
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut read).await.is_err());
    release.send(()).unwrap();
    let (Extension(snapshot), Json(page)) = tokio::time::timeout(Duration::from_millis(200), read)
        .await.expect("a failed attempt must wake its waiting readers").unwrap();
    assert!(worker.await.unwrap().is_err());
    assert_eq!(snapshot.store_index, cut);
    assert_eq!(page.items, before.items);
}

#[tokio::test]
async fn roster_resilience_fresh_failed_attempt_returns_last_publication_promptly() {
    assert_roster_resilience_failed_attempt(false).await;
}

#[tokio::test]
async fn roster_resilience_fresh_failed_history_attempt_returns_last_publication_promptly() {
    assert_roster_resilience_failed_attempt(true).await;
}

#[tokio::test]
async fn roster_resilience_fresh_idle_and_new_attempt_after_failure_wait_for_written_cards() {
    for history in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        roster_resilience_runtime(store, "agent/retained", "old");
        refresh_agent_roster(store, history).unwrap();
        for failed in [false, true] {
            let old_cut = store.index().unwrap();
            let runtime = if failed { "after-failure" } else { "after-idle" };
            roster_resilience_runtime(store, "agent/retained", runtime);
            if failed {
                assert!(store.answer_agent_roster_requests::<()>(||
                    anyhow::bail!("injected previous refresh failure")).is_err());
            }
            let read = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
                Query(ClientListQuery { history, fresh: true, ..ClientListQuery::default() }));
            tokio::pin!(read);
            if !failed {
                assert!(tokio::time::timeout(Duration::from_millis(30), &mut read).await.is_err(),
                    "an idle refresher must not answer a post-write fresh read with old cards");
            }
            let reader = Arc::clone(store);
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, proceed) = std::sync::mpsc::channel();
            let worker = tokio::task::spawn_blocking(move || reader.answer_agent_roster_requests(|| {
                started.send(()).unwrap();
                proceed.recv().unwrap();
                refresh_agent_roster(&reader, history)
            }));
            ready.await.unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(30), &mut read).await.is_err(),
                "a newer in-flight attempt must be awaited even after a previous failure");
            release.send(()).unwrap();
            let (Extension(snapshot), Json(page)) = tokio::time::timeout(Duration::from_millis(200), read)
                .await.expect("successful publication must wake the fresh reader").unwrap();
            worker.await.unwrap().unwrap();
            assert_eq!(snapshot.store_index, store.index().unwrap());
            assert!(snapshot.store_index > old_cut);
            assert_eq!(page.items[0]["runtime_ids"], json!([format!("runtime/{runtime}")]));
        }
    }
}


async fn assert_agent_roster_survives_other_mode_publications(history: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.store = Arc::new(roster_followup_store());
    let store = &state.store;
    let wake = store.start_agent_roster_refresher().unwrap();
    let advance = |marker| {
        store.append_claim(&ClaimInput {
            subject: format!("custom/test/roster-retention-{marker}"),
            kind: "custom.test.marker".into(), actor: None, fields: BTreeMap::new(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    refresh_agent_roster(store, history).unwrap();
    advance(0);
    refresh_agent_roster(store, history).unwrap();
    let (cut, _, published_at) = store.published_agent_roster(store.index().unwrap(), history).unwrap();
    let (_, Json(before)) = client_agents(
        State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery { history, ..ClientListQuery::default() }),
    ).await.unwrap();
    let mut first_other_cut = None;
    for marker in 1..=12 {
        advance(marker);
        refresh_agent_roster(store, !history).unwrap();
        let other_cut = store.published_agent_roster(store.index().unwrap(), !history).unwrap().0;
        assert_eq!(other_cut, store.index().unwrap(), "each refresh must publish a distinct cut");
        first_other_cut.get_or_insert(other_cut);
    }
    assert!(store.index().unwrap() > cut);
    assert!(store.published_agent_roster_at(first_other_cut.unwrap(), !history).is_none(),
        "old publications of the busy mode must still be evicted");
    assert!(!store.take_agent_roster_history_request());
    let (Extension(snapshot), Json(after)) = client_agents(
        State(state.clone()), Extension(new_client_snapshot(&state)),
        Query(ClientListQuery { history, fresh: true, ..ClientListQuery::default() }),
    ).await.expect("the retained publication must answer, not return agent-roster-not-ready");
    assert_eq!(snapshot.store_index, cut, "serve the newest retained publication's own cut");
    assert_eq!(snapshot.published_at, Some(client_timestamp(published_at)));
    assert_eq!(after.items, before.items);
    assert_eq!(store.take_agent_roster_history_request(), history);
    tokio::time::timeout(Duration::from_secs(1), wake.notified()).await
        .expect("the stale first page must request a refresh");
}

#[tokio::test]
async fn client_agents_agent_roster_retains_history_during_current_publications() {
    assert_agent_roster_survives_other_mode_publications(true).await;
}

#[tokio::test]
async fn client_agents_agent_roster_retains_current_during_history_publications() {
    assert_agent_roster_survives_other_mode_publications(false).await;
}

#[test]
fn agent_roster_refresh_never_folds_more_than_a_chunk_in_one_call() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let add = |agents: std::ops::Range<usize>| for n in agents {
        store.append_claim(&ClaimInput {
            subject: format!("agent/bound-{n:04}"), kind: "runtime.observed".into(),
            actor: None, fields: serde_json::from_value(json!({"status":"running",
                "runtime_id":format!("bound-{n}"), "incarnation_id":"one"})).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    let _wake = store.start_agent_roster_refresher().unwrap();
    // Cold: nothing to start from.
    add(0..600);
    refresh_agent_roster(store, false).unwrap();
    let (_, cards, _) = store.published_agent_roster(store.index().unwrap(), false).unwrap();
    assert_eq!(cards.len(), 600);
    // More changed and missing cards than one fold may take.
    add(600..900);
    refresh_agent_roster(store, false).unwrap();
    let (cut, cards, _) = store.published_agent_roster(store.index().unwrap(), false).unwrap();
    assert_eq!((cut, cards.len()), (store.index().unwrap(), 900));
    assert!(store.agent_resources_largest_fold_for_test() <= AGENT_ROSTER_WARM_CHUNK,
        "one call folded {} cards", store.agent_resources_largest_fold_for_test());
    // A few changes complete in one short fold.
    add(900..910);
    refresh_agent_roster(store, false).unwrap();
    assert_eq!(store.published_agent_roster(store.index().unwrap(), false).unwrap().1.len(), 910);
    assert!(store.agent_resources_largest_fold_for_test() <= AGENT_ROSTER_WARM_CHUNK);
}

/// What one roster refresh costs on a copy of a real or generated store: the first full
/// fold, a fold after a claim that changes one card, and one after a claim no card reads.
/// `ST_ROSTER_FOLD_STORE` names the store; the copy stays in `TMPDIR` and only timings print.
#[test]
#[ignore = "roster fold timing on a store copy; set ST_ROSTER_FOLD_STORE and run with --ignored --nocapture"]
fn agent_roster_fold_timing_on_a_store_copy() {
    let Some(source) = std::env::var_os("ST_ROSTER_FOLD_STORE").map(PathBuf::from) else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("claims.sqlite3");
    for suffix in ["", "-wal"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            fs::copy(&from, format!("{}{suffix}", database.display())).unwrap();
        }
    }
    // With ST3_PROFILE_DIR set, each fold's statements land in its slow.jsonl.
    smallclaims::profile::init_from_env();
    let store = Store::open(&database, "bench-host").unwrap();
    let timed = |label: &str, samples: &mut Vec<f64>, work: &dyn Fn()| {
        let before = smallclaims::sqlite::work::total();
        let started = Instant::now();
        smallclaims::profile::task("roster-fold-timing", work);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        samples.push(ms);
        println!("roster fold {label}: {ms:.1} ms; sqlite_work={:?}",
            smallclaims::sqlite::work::total() - before);
    };
    let mut cold = Vec::new();
    timed("cold", &mut cold, &|| refresh_agent_roster(&store, false).unwrap());
    let agents = store.read_snapshot(|index| Ok(client_agent_page_refs(&store, false, index)?
        .iter().filter_map(|card| card["id"].as_str().map(str::to_owned)).collect::<Vec<_>>()))
        .unwrap();
    println!("roster fold agents={}", agents.len());
    assert!(!agents.is_empty());
    let append = |subject: &str, kind: &str, fields: Value| {
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    let mut changed = Vec::new();
    let mut unrelated = Vec::new();
    for turn in 0..30 {
        let agent = &agents[turn % agents.len()];
        append(agent, "harness.observed", json!({"state": if turn % 2 == 0 { "working" } else { "idle" },
            "driver":"codex", "incarnation_id": format!("fold-timing-{turn}")}));
        timed(&format!("one card changed (agent #{})", turn % agents.len()), &mut changed,
            &|| refresh_agent_roster(&store, false).unwrap());
        append(&format!("custom/fold-timing/{turn}"), "custom.test.marker", json!({}));
        timed("no card changed", &mut unrelated, &|| refresh_agent_roster(&store, false).unwrap());
    }
    let summary = |label: &str, samples: &mut Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        let at = |q: f64| samples[((samples.len() as f64 * q).ceil() as usize).saturating_sub(1)];
        println!("roster fold summary {label}: n={} p50={:.1} p90={:.1} max={:.1} ms",
            samples.len(), at(0.5), at(0.9), samples[samples.len() - 1]);
    };
    summary("cold", &mut cold);
    summary("one card changed", &mut changed);
    summary("no card changed", &mut unrelated);
    let performance = smallclaims::performance::snapshot();
    for row in performance["requests"].as_array().into_iter().flatten()
        .filter(|row| row["kind"].as_str().is_some_and(|kind| {
            kind.starts_with("roster/") || matches!(kind, "published_list.agents" | "startup.warmup")
        }))
    {
        println!("roster fold task {}: n={} total_ms={} max_ms={}",
            row["kind"], row["count"], row["total_ms"], row["max_ms"]);
    }
}

/// What a refresh costs after a backlog of real claims: the roster folded at a cut that many
/// claims back, then refreshed at the newest, as after an idle refresher or a slow one.
/// `ST_ROSTER_BACKLOG_STORE` names a disposable store copy, opened in place; and
/// `ST_ROSTER_BACKLOGS` the backlogs in claims, comma separated.
#[test]
#[ignore = "roster refresh timing after a backlog; set ST_ROSTER_BACKLOG_STORE and run with --ignored --nocapture"]
fn agent_roster_backlog_refresh_timing_on_a_store_copy() {
    let Some(database) = std::env::var_os("ST_ROSTER_BACKLOG_STORE").map(PathBuf::from) else {
        return;
    };
    let backlogs = std::env::var("ST_ROSTER_BACKLOGS").unwrap_or_else(|_| "20,200,2000".into());
    smallclaims::profile::init_from_env();
    for backlog in backlogs.split(',').map(|n| n.trim().parse::<u64>().unwrap()) {
        // A fresh store each time: no fold kept from the previous backlog.
        let store = Store::open(&database, "bench-host").unwrap();
        let newest = store.index().unwrap();
        let timed = |label: &str, work: &dyn Fn()| {
            let before = smallclaims::sqlite::work::total();
            let started = Instant::now();
            smallclaims::profile::task("roster-backlog-timing", work);
            println!("roster backlog {backlog}: {label} {:.1} ms; sqlite_work={:?}",
                started.elapsed().as_secs_f64() * 1000.0,
                smallclaims::sqlite::work::total() - before);
        };
        timed("cold", &|| { client_agent_resources_cached(&store, false, newest - backlog).unwrap(); });
        let reason = store.agent_roster_unbounded_because(newest, false, AGENT_ROSTER_WARM_CHUNK)
            .unwrap();
        let refolded = store.agent_resources_refolded_cards_for_test();
        timed("refresh", &|| refresh_agent_roster(&store, false).unwrap());
        println!("roster backlog {backlog}: chunked because {reason:?}; cards refolded {}",
            store.agent_resources_refolded_cards_for_test() - refolded);
    }
}

/// What one refresh costs after one claim of each kind that moves cards, on a disposable
/// store copy opened in place: claims copied from `ST_ROSTER_KIND_CLAIMS` (a JSON array of
/// `{label, subject, kind, actor, fields}`), and bursts of `harness.observed` on that many
/// agents at once. Each prints the fold time, cards refolded and where the time went.
#[test]
#[ignore = "roster refresh timing by claim kind; set ST_ROSTER_BACKLOG_STORE and ST_ROSTER_KIND_CLAIMS"]
fn agent_roster_refresh_by_claim_kind_timing_on_a_store_copy() {
    let (Some(database), Some(claims)) = (
        std::env::var_os("ST_ROSTER_BACKLOG_STORE").map(PathBuf::from),
        std::env::var_os("ST_ROSTER_KIND_CLAIMS").map(PathBuf::from),
    ) else {
        return;
    };
    let claims: Vec<Value> = serde_json::from_slice(&fs::read(claims).unwrap()).unwrap();
    let rounds = std::env::var("ST_ROSTER_KIND_ROUNDS").map_or(5, |n| n.parse().unwrap());
    smallclaims::profile::init_from_env();
    let store = Store::open(&database, "bench-host").unwrap();
    refresh_agent_roster(&store, false).unwrap();
    let agents = store.read_snapshot(|index| Ok(client_agent_page_refs(&store, false, index)?
        .iter().filter_map(|card| card["id"].as_str().map(str::to_owned)).collect::<Vec<_>>()))
        .unwrap();
    println!("roster kinds agents={}", agents.len());
    let task_ms = || {
        let performance = smallclaims::performance::snapshot();
        performance["requests"].as_array().into_iter().flatten()
            .filter_map(|row| Some((row["kind"].as_str()?.to_owned(), row["total_ms"].as_f64()?)))
            .filter(|(kind, _)| kind.starts_with("roster/")
                || matches!(kind.as_str(), "published_list.agents" | "startup.warmup"))
            .collect::<BTreeMap<_, _>>()
    };
    let append = |subject: &str, kind: &str, actor: Option<&str>, fields: &Value| {
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: actor.map(str::to_owned),
            fields: serde_json::from_value(fields.clone()).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).map(drop)
    };
    let timed = |label: &str| {
        let before = (smallclaims::sqlite::work::total(), task_ms(),
            store.agent_resources_refolded_cards_for_test());
        let started = Instant::now();
        smallclaims::profile::task("roster-kind-timing", || refresh_agent_roster(&store, false).unwrap());
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let after = task_ms();
        let tasks = after.iter().filter_map(|(kind, total)| {
            let spent = total - before.1.get(kind).copied().unwrap_or(0.0);
            (spent >= 1.0).then(|| format!("{kind}={spent:.0}"))
        }).collect::<Vec<_>>().join(" ");
        println!("roster kind {label}: {ms:.1} ms; cards refolded {}; {tasks}; vm_steps={}",
            store.agent_resources_refolded_cards_for_test() - before.2,
            (smallclaims::sqlite::work::total() - before.0).vm_steps);
    };
    for round in 0..rounds {
        append(&format!("custom/kind-timing/{round}"), "custom.test.marker", None, &json!({})).unwrap();
        timed("no card");
        for burst in [1, 10, 25] {
            for (n, agent) in agents.iter().cycle().skip(round * 7).take(burst.min(agents.len())).enumerate() {
                append(agent, "harness.observed", None, &json!({
                    "state": if (round + n) % 2 == 0 { "working" } else { "idle" },
                    "driver": "codex", "incarnation_id": format!("kind-timing-{round}-{n}"),
                })).unwrap();
            }
            timed(&format!("harness.observed x{burst}"));
        }
        for claim in &claims {
            let label = claim["label"].as_str().unwrap();
            match append(claim["subject"].as_str().unwrap(), claim["kind"].as_str().unwrap(),
                claim["actor"].as_str(), &claim["fields"])
            {
                Ok(()) => timed(label),
                Err(error) => println!("roster kind {label}: append refused: {error:#}"),
            }
        }
    }
}

#[test]
#[ignore = "focused roster timing fixture; run explicitly with --ignored --nocapture"]
fn agent_roster_snapshot_fixture_timing() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = format!("version 2\n{}", (0..70)
        .map(|n| format!("agent \"roster-{n:02}\" {{ command \"true\" }}\n"))
        .collect::<String>());
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let plan = store.mission(&intent, crate::model::IntentInput {
        kdl: source, source_name: None,
    }).unwrap();
    store.apply(&intent, &plan.subject_tokens, "roster-fixture").unwrap();
    let subjects = (0..70).map(|n| format!("agent/node.roster-{n:02}")).collect::<Vec<_>>();
    for subject in &subjects {
        for session in 0..20 {
            let incarnation = format!("session-{session}");
            for (kind, fields) in [
                ("runtime.observed", json!({"status":"running", "runtime_id":subject,
                    "incarnation_id":incarnation})),
                ("harness.observed", json!({"state":"idle", "driver":"codex",
                    "incarnation_id":incarnation})),
                ("harness.usage", json!({"semantics":"response", "driver":"codex",
                    "incarnation_id":incarnation, "model":"fixture",
                    "input_tokens":10, "output_tokens":5, "total_tokens":15})),
            ] {
                store.append_claim(&ClaimInput {
                    subject: subject.clone(), kind: kind.into(), actor: Some(subject.clone()),
                    fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
                    expected_subject: None, idempotency_key: None,
                }).unwrap();
            }
        }
    }
    let index = store.index().unwrap();
    let measure = |label: &str, read: &dyn Fn()| {
        let before = smallclaims::sqlite::work::total();
        let started = Instant::now();
        read();
        println!("roster fixture {label}: {:.3} ms; sqlite_work={:?}",
            started.elapsed().as_secs_f64() * 1000.0,
            smallclaims::sqlite::work::total() - before);
    };
    store.forget_current_views();
    measure("main cold card status", &|| {
        store.agent_card_status_at(None, index, false).unwrap();
    });
    measure("main usage fold", &|| {
        store.usage_summaries_at(&subjects, Some(index)).unwrap();
    });
    let oracle = client_agent_resources_uncached(store, false, index).unwrap();
    measure("main repeated projections x22", &|| {
        for _ in 0..22 {
            let mut rows = client_agent_resources_selected(store, false, index, None).unwrap();
            add_agent_todos(store, &mut rows, index).unwrap();
        }
    });
    store.forget_current_views();
    measure("shared cold projection", &|| {
        let mut rows = client_agent_resources_cached(store, false, index).unwrap();
        for row in &mut rows { row.as_object_mut().unwrap().remove("todo"); }
        assert_eq!(rows, oracle);
    });
    measure("shared warm projections x22", &|| {
        for _ in 0..22 { client_agent_resources_cached(store, false, index).unwrap(); }
    });
    store.append_claim(&ClaimInput {
        subject: subjects[0].clone(), kind: "harness.observed".into(),
        actor: Some(subjects[0].clone()),
        fields: serde_json::from_value(json!({"state":"working", "driver":"codex",
            "incarnation_id":"session-19"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let index = store.index().unwrap();
    let mut advanced = Vec::new();
    let started = Instant::now();
    advanced.extend(client_agent_resources_cached(store, false, index).unwrap());
    println!("roster fixture shared one-card advance: {:.3} ms",
        started.elapsed().as_secs_f64() * 1000.0);
    for row in &mut advanced { row.as_object_mut().unwrap().remove("todo"); }
    assert_eq!(advanced, client_agent_resources_uncached(store, false, index).unwrap());
}

#[test]
fn agent_card_status_preserves_declared_and_canonical_fallback_revisions() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = "version 2\nagent \"declared\" { name \"Same\"; command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store.mission(&intent, IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "card-status-fixture").unwrap();
    let append = |subject: &str, kind: &str, fields: Value| {
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap()
    };
    append("agent/node.declared", "runtime.observed", json!({
        "status":"running", "runtime_id":"declared", "incarnation_id":"one"
    }));
    let runtime = append("agent/undeclared", "runtime.observed", json!({
        "status":"running", "runtime_id":"undeclared", "incarnation_id":"one"
    }));
    append("agent/undeclared", "harness.observed", json!({
        "state":"working", "driver":"omp", "incarnation_id":"one"
    }));
    let first = store.index().unwrap();
    let last = append("agent/undeclared", "harness.observed", json!({
        "state":"idle", "driver":"omp", "incarnation_id":"one"
    }));
    for index in [first, last.store_index] {
        store.read_snapshot(|_| {
            let full = client_agent_resources_uncached(store, false, index)?;
            let cards = client_agent_resources_selected(store, false, index, None)?;
            assert_eq!(Sha256::digest(serde_json::to_vec(&cards)?),
                Sha256::digest(serde_json::to_vec(&full)?));
            let status = store.status_for_subject_prefix_at(
                "agent/",
                Some(index),
                false,
                crate::store::ClaimsReduction::Full,
            )?;
            let declared = status.subjects.iter().find(|s| s.subject == "agent/node.declared").unwrap();
            assert_eq!(cards.iter().find(|c| c["id"] == declared.subject).unwrap()["revision"],
                declared.desired_revision.as_ref().unwrap().as_str());
            let undeclared = status.subjects.iter().find(|s| s.subject == "agent/undeclared").unwrap();
            assert_eq!(cards.iter().find(|c| c["id"] == undeclared.subject).unwrap()["revision"],
                undeclared.claims.last().unwrap().as_str());
            Ok(())
        }).unwrap();
    }
    // Equal accepted times must use canonical writer order, not arrival or claim ID.
    // Construct that metadata tie on this isolated fixture, with the first claim last.
    {
        let connection = store.connection.write();
        connection.execute(
            "UPDATE claims SET accepted_at_unix_ms='1000' WHERE subject='agent/undeclared'", [],
        ).unwrap();
        connection.execute("UPDATE batches SET origin='zz-card-tie' WHERE id=?1", [&runtime.batch_id]).unwrap();
        connection.execute("UPDATE claims SET origin='zz-card-tie' WHERE batch_id=?1", [&runtime.batch_id]).unwrap();
    }
    store.forget_current_views();
    let index = store.index().unwrap();
    let full = client_agent_resources_uncached(store, false, index).unwrap();
    let cards = client_agent_resources_selected(store, false, index, None).unwrap();
    assert_eq!(Sha256::digest(serde_json::to_vec(&cards).unwrap()),
        Sha256::digest(serde_json::to_vec(&full).unwrap()), "cards={cards:?}\nfull={full:?}");
    assert_eq!(cards.iter().find(|c| c["id"] == "agent/undeclared").unwrap()["revision"],
        runtime.id);
}

#[test]
#[ignore = "requires ST3_AGENT_CORPUS naming an owned disposable corpus copy"]
fn agent_card_status_copied_corpus_parity() {
    let path = std::env::var_os("ST3_AGENT_CORPUS").expect("owned corpus copy required");
    for history in [false, true] {
        let oracle = Store::open(Path::new(&path), "card-status-benchmark").unwrap();
        let store = Store::open(Path::new(&path), "card-status-benchmark").unwrap();
        let read = |store: &Store, full: bool| {
            let before = smallclaims::sqlite::work::total();
            let start = Instant::now();
            let cards = store.read_snapshot(|index| {
                store.with_owned_set_snapshot_reads(|| {
                    if full { client_agent_resources_uncached(store, history, index) }
                    else { client_agent_resources_selected(store, history, index, None) }
                })
            }).unwrap();
            let work = smallclaims::sqlite::work::total() - before;
            eprintln!("agent-card-status full={full} history={history} elapsed_ms={} sql={} vm_steps={} cards={}",
                start.elapsed().as_millis(), work.statements, work.vm_steps, cards.len());
            cards
        };
        let full = read(&oracle, true);
        let cards = read(&store, false);
        let full_hash = hex::encode(Sha256::digest(serde_json::to_vec(&full).unwrap()));
        let card_hash = hex::encode(Sha256::digest(serde_json::to_vec(&cards).unwrap()));
        assert_eq!(card_hash, full_hash);
        eprintln!("agent-card-status history={history} sha256={card_hash}");
    }
}

#[test]
fn agent_cards_advance_locally_and_keep_historical_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source =
        "version 2\nagent \"amber\" { command \"true\" }\nagent \"cobalt\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "incremental-cards")
        .unwrap();
    let append = |subject: &str, kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    append(
        "agent/node.amber",
        "runtime.observed",
        json!({
            "status":"running", "runtime_id":"node.amber", "incarnation_id":"amber-1",
        }),
    );
    let before = store.index().unwrap();
    let original = checked_agent_cache(store, false, before);
    checked_agent_cache(store, true, before);
    for n in 0..12 {
        append(
            "agent/node.amber",
            "harness.observed",
            json!({
                "state":if n % 2 == 0 { "idle" } else { "working" },
                "driver":"codex", "incarnation_id":"amber-1",
            }),
        );
        let at = store.index().unwrap();
        for history in [false, true] {
            checked_agent_cache(store, history, at);
        }
        // The unchanged card survives every local update.
        let cards = checked_agent_cache(store, false, at);
        assert_eq!(
            cards
                .iter()
                .find(|card| card["id"] == "agent/node.cobalt")
                .unwrap(),
            original
                .iter()
                .find(|card| card["id"] == "agent/node.cobalt")
                .unwrap()
        );
    }
    // Native credentials can change while activity stays working. Both cached
    // history modes must carry the refusal/recovery, preserving the legacy wire state.
    for provider_auth in [false, true, false, true] {
        append(
            "agent/node.amber",
            "harness.observed",
            json!({
                "state":"working", "driver":"codex", "incarnation_id":"amber-1",
                "provider_auth":provider_auth,
            }),
        );
        let at = store.index().unwrap();
        for history in [false, true] {
            let cards = checked_agent_cache(store, history, at);
            let amber = cards
                .iter()
                .find(|card| card["id"] == "agent/node.amber")
                .unwrap();
            assert_eq!(
                amber["harness_state"],
                if provider_auth {
                    "working"
                } else {
                    "unauthenticated"
                }
            );
            assert_eq!(
                amber["harness_error_state"],
                if provider_auth {
                    Value::Null
                } else {
                    json!("needs-login")
                }
            );
        }
    }
    // Evicted old snapshots rebuild independently of the newest cache.
    assert_eq!(checked_agent_cache(store, false, before), original);
    append(
        "agent/node.cobalt",
        "runtime.observed",
        json!({
            "status":"running", "runtime_id":"node.cobalt", "incarnation_id":"cobalt-1",
        }),
    );
    checked_agent_cache(store, false, store.index().unwrap());
    // A declaration changes membership and falls back to the complete projection.
    let source = "version 2\nagent \"birch\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "incremental-new-card")
        .unwrap();
    let cards = checked_agent_cache(store, false, store.index().unwrap());
    assert_eq!(
        cards
            .iter()
            .map(|card| card["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["node.amber", "node.birch", "node.cobalt"]
    );
}

#[test]
fn agents_name_their_queued_steps_and_missions_carry_open_run_steps() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let source = r#"
version 2
mission "labelled" state="ready" {
  goal "Name steps where clients read them."
  agent "worker" { workspace "/tmp"; harness "codex" {} }
  step "first" { title "Say hello"; goal "Greet the fleet."; assigned-to "agent/${ST_MISSION_RUN}/worker" }
  step "second" { goal "Wave goodbye."; depends-on "first"; assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "labelled-source")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "labelled".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "labelled-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let first = run
        .steps
        .iter()
        .find(|step| step.step == "first")
        .unwrap()
        .subject
        .clone();
    store.set_step_state(&first, "ready", None).unwrap();
    let index = store.index().unwrap();

    let agents = client_agent_resources(&store, false, "snapshot", index).unwrap();
    let next = &agents[0]["next_work"];
    assert_eq!(next["id"], first);
    assert_eq!(next["mission_id"], "mission/labelled");
    assert_eq!(next["mission_run_id"], run.subject);
    assert_eq!(next["path"], "first");
    assert_eq!(next["title"], "Say hello");
    assert_eq!(next["goal"], "Greet the fleet.");
    assert_eq!(next["state"], "ready");
    assert_eq!(agents[0]["upcoming_work"], json!([next]));
    assert_eq!(agents[0]["current_work"], json!([]));

    // The list carries the open run's steps, so a client never joins work to missions.
    let missions = client_v0::mission_resources(&store, index, false, None).unwrap();
    let steps = missions[0]["run_details"][0]["steps"].as_array().unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|step| (
                step["path"].as_str().unwrap(),
                step["state"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [("first", "ready"), ("second", "waiting")]
    );
    assert_eq!(steps[0]["goals"], json!(["Greet the fleet."]));
    assert_eq!(steps[0]["assignee"], format!("agent/{}/worker", run.id));

    store.append_claim(&ClaimInput {
        subject: format!("agent/{}/worker", run.id), kind: "harness.observed".into(),
        actor: None, fields: serde_json::from_value(json!({
            "state":"idle", "driver":"codex", "incarnation_id":"labelled-1",
        })).unwrap(), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let cards = checked_agent_cache(&store, false, store.index().unwrap());
    assert_eq!(cards[0]["next_work"], *next);
    store.set_step_state(&first, "working", None).unwrap();
    checked_agent_cache(&store, false, store.index().unwrap());
}

#[test]
fn agent_state_requires_current_harness_evidence_for_native_drivers() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let source = r#"
version 2
mission "agent-health" state="ready" {
  goal "Exercise agent health projection."
  agent "worker" { workspace "/tmp"; harness "codex" {} }
  step "queued" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "agent-health-source")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "agent-health".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "agent-health-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let subject = format!("agent/{}/worker", run.id);
    // This test isolates harness evidence. Supply the independent local delivery proof:
    // absent polls become stale after the process-wide startup grace, so relying on that
    // grace makes the running assertions depend on which tests ran before this one.
    delivery_presence::record_legacy(&subject, "app-server", std::process::id());
    let queued = run.steps[0].subject.clone();
    store.set_step_state(&queued, "ready", None).unwrap();
    store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String("node.worker".into())),
                (
                    "incarnation_id".into(),
                    Value::String("incarnation-1".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("agent-health-runtime".into()),
        })
        .unwrap();
    let resources =
        client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["state"], "starting");
    assert_eq!(resources[0]["host_id"], "host/node");
    assert!(resources[0]["last_activity_at"].is_null());
    assert_eq!(resources[0]["next_work_id"], queued);
    assert_eq!(resources[0]["upcoming_work_ids"], json!([queued]));
    assert_eq!(resources[0]["queued_work_count"], 1);

    store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "harness.observed".into(),
            actor: Some(subject.clone()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("idle".into())),
                ("driver".into(), Value::String("codex".into())),
                (
                    "incarnation_id".into(),
                    Value::String("incarnation-1".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("agent-health-ready".into()),
        })
        .unwrap();
    let resources =
        client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["delivery"]["state"], "legacy", "{}", resources[0]);
    assert_eq!(resources[0]["state"], "running", "{}", resources[0]);

    let healthy_cut = store.index().unwrap();
    let harness = |driver: &str, incarnation: &str, key: &str| {
        store.append_claim(&ClaimInput {
            subject: subject.clone(), kind: "harness.observed".into(), actor: Some(subject.clone()),
            fields: serde_json::from_value(json!({"state":"ready","driver":driver,"incarnation_id":incarnation})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: Some(key.into()),
        }).unwrap();
    };
    harness("claude", "incarnation-1", "agent-health-foreign-provider");
    let resources = client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["driver"], "codex");
    assert_eq!(resources[0]["state"], "starting");
    assert!(resources[0]["harness_state"].is_null(), "wrong-provider readiness must not survive the cached card");
    assert_eq!(resources[0]["queued_work_count"], 1);
    harness("codex", "incarnation-1", "agent-health-native-provider-restored");
    let fault = crate::model::AttentionRequest {
        reviewer: "person/test".into(), title: "Mailbox unavailable".into(), reason: "The current mailbox lost delivery custody.".into(),
        severity: "error".into(), targets: vec![subject.clone()], actor: "daemon/runtime".into(), idempotency_key: "agent-health-mailbox-loss".into(),
    };
    let episode = "attention/agent-health-mailbox";
    let recorded = store.record_runtime_failure(episode, &fault, "mailbox-channel-lost").unwrap();
    let repeated = store.record_runtime_failure(episode, &fault, "mailbox-channel-lost").unwrap();
    assert_eq!(recorded.request, repeated.request);
    let resources = client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["state"], "waiting");
    assert_eq!(resources[0]["harness_state"], "indeterminate");
    assert_eq!(resources[0]["blocked_on"], "channel");
    assert_eq!(resources[0]["fault"], fault.reason);
    harness("codex", "incarnation-1", "agent-health-superficial-ready");
    let resources = client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["state"], "waiting", "ready alone cannot clear a mailbox fault");
    let historic = client_agent_resources(&store, false, "snapshot", healthy_cut).unwrap();
    assert_eq!(historic[0]["state"], "running");
    assert!(historic[0]["fault"].is_null(), "the historical cut must not observe a later fault");
    store.recover_operational_failure(episode, "authenticated replay control", "agent-health-repaired").unwrap();
    let resources = client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["state"], "running");
    assert!(resources[0]["fault"].is_null());
    assert_eq!(resources[0]["next_work_id"], queued);

    store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "harness.observed".into(),
            actor: Some(subject.clone()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("ended".into())),
                ("driver".into(), Value::String("codex".into())),
                (
                    "incarnation_id".into(),
                    Value::String("incarnation-1".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("agent-health-ended".into()),
        })
        .unwrap();
    let resources =
        client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["state"], "failed");
    store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("failed".into())),
                ("runtime_id".into(), Value::String("node.worker".into())),
                (
                    "incarnation_id".into(),
                    Value::String("incarnation-1".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("agent-health-crashed".into()),
        })
        .unwrap();
    let resources =
        client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(
        resources.len(),
        1,
        "current unhealthy agents must remain visible"
    );
    assert_eq!(resources[0]["state"], "failed");
    assert_eq!(resources[0]["operational"]["layer"], "current");
}

#[test]
fn human_blocking_overrides_activity_but_not_runtime_or_harness_fences() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    // A run-owned seat carries its declared harness driver, so a new incarnation without
    // a harness observation is `starting` rather than a driverless wrapper's `running`.
    let source = r#"
version 2
mission "agent-human" state="ready" {
  goal "Exercise human blocking projection."
  agent "worker" { workspace "/tmp"; harness "omp" {} }
  step "queued" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "agent-human-source")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "agent-human".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "agent-human-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let subject = format!("agent/{}/worker", run.id);
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: kind.into(),
                actor: None,
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    let observe_runtime = |status: &str| {
        append(
            "runtime.observed",
            json!({
                "status": status, "runtime_id": "node.worker", "incarnation_id": "human-1",
            }),
        );
    };
    let observe_harness = |activity: &str| {
        append(
            "harness.observed",
            json!({
                "state": activity, "driver": "omp", "incarnation_id": "human-1",
                "blocked_on": "human", "ask": "permission", "reason": "Deploy production?",
            }),
        );
    };
    // Exercise the canonical graph projection independently of process-local delivery health.
    let agent = || {
        checked_agent_cache(&store, true, store.index().unwrap())
            .into_iter()
            .find(|agent| agent["id"] == subject.as_str())
            .unwrap()
    };
    observe_runtime("running");
    for activity in ["working", "idle", "ready"] {
        observe_harness(activity);
        assert_eq!(agent()["state"], "waiting", "{activity}");
    }
    append("harness.observed", json!({
        "state": "working", "driver": "omp", "incarnation_id": "human-1",
        "blocked_on": null, "ask": null, "reason": null, "input_buffer": null, "exit": null,
    }));
    let answered: st3_client::Agent = serde_json::from_value(agent()).unwrap();
    assert_eq!(answered.state, "running");
    assert_eq!(answered.harness_state.as_deref(), Some("working"));
    assert!(answered.blocked_on.is_none());
    assert!(answered.ask.is_none());
    assert!(answered.reason.is_none());
    assert!(answered.activity.is_none());
    // Compacting is the seat's status: it runs, it waits on nobody.
    append("harness.observed", json!({
        "state": "working", "driver": "omp", "incarnation_id": "human-1",
        "blocked_on": null, "ask": null, "reason": "compaction", "input_buffer": null, "exit": null,
    }));
    let compacting: st3_client::Agent = serde_json::from_value(agent()).unwrap();
    assert_eq!(compacting.state, "running");
    assert_eq!(compacting.activity.as_deref(), Some("compacting"));
    // The harness schema's terminal activity keeps precedence over a stale ask.
    observe_harness("ended");
    assert_eq!(agent()["state"], "failed");
    assert!(agent()["blocked_on"].is_null());
    assert!(agent()["ask"].is_null());
    // Indeterminate activity keeps its existing waiting verdict; clients must not
    // present it as an answerable human ask.
    observe_harness("indeterminate");
    assert_eq!(agent()["state"], "waiting");
    assert_eq!(agent()["harness_state"], "indeterminate");
    assert!(agent()["blocked_on"].is_null());
    assert!(agent()["ask"].is_null());
    // A sparse working successor in this incarnation must not revive the old ask.
    append("harness.observed", json!({
        "state": "working", "driver": "omp", "incarnation_id": "human-1",
    }));
    assert_eq!(agent()["state"], "running");
    assert!(agent()["blocked_on"].is_null());
    assert!(agent()["ask"].is_null());
    // A fresh permission observation may block again.
    observe_harness("working");
    assert_eq!(agent()["state"], "waiting");
    observe_runtime("stopped");
    assert_eq!(agent()["state"], "stopped");
    observe_runtime("starting");
    assert_eq!(agent()["state"], "starting");
    observe_runtime("running");
    append("runtime.reconcile-decision", json!({
        "key": "member-reconcile", "decision": "member-fault", "reason": "Cannot reconcile seat",
    }));
    assert_eq!(agent()["state"], "failed");
    append("runtime.reconcile-decision", json!({
        "key": "member-reconcile", "decision": "member-started",
    }));
    append("runtime.observed", json!({
        "status": "running", "runtime_id": "node.worker", "incarnation_id": "human-2",
    }));
    // Before a new incarnation's first observation, the previous ask is fenced out.
    assert_eq!(agent()["state"], "starting", "{}", agent());
    assert!(agent()["blocked_on"].is_null());
    append("harness.observed", json!({
        "state": "idle", "driver": "omp", "incarnation_id": "human-2",
        "blocked_on": null, "ask": null, "reason": null, "input_buffer": null, "exit": null,
    }));
    observe_harness("working");
    let resumed: st3_client::Agent = serde_json::from_value(agent()).unwrap();
    assert_eq!(resumed.state, "running");
    assert_eq!(resumed.harness_state.as_deref(), Some("idle"));
    assert_eq!(resumed.incarnation_id.as_deref(), Some("human-2"));
    assert!(resumed.blocked_on.is_none());
    assert!(resumed.ask.is_none());
    assert!(resumed.reason.is_none());
}

#[test]
fn an_unauthenticated_harness_is_waiting_not_starting() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let source = r#"
version 2
mission "agent-auth" state="ready" {
  goal "Exercise an unauthenticated harness."
  agent "worker" { workspace "/tmp"; harness "claude" {} }
  step "queued" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "agent-auth-source")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "agent-auth".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "agent-auth-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let subject = format!("agent/{}/worker", run.id);
    for (kind, fields, key) in [
        (
            "runtime.observed",
            BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String("node.worker".into())),
                ("incarnation_id".into(), Value::String("worker-1".into())),
            ]),
            "agent-auth-runtime",
        ),
        (
            "harness.diagnostic",
            BTreeMap::from([
                ("severity".into(), Value::String("error".into())),
                ("status".into(), Value::String("unauthenticated".into())),
                ("code".into(), Value::String("provider-auth-expired".into())),
                (
                    "reason".into(),
                    Value::String("Claude reports an expired login".into()),
                ),
                ("incarnation_id".into(), Value::String("worker-1".into())),
            ]),
            "agent-auth-expired",
        ),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: kind.into(),
                actor: (kind == "harness.diagnostic").then(|| subject.clone()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    }
    let resources =
        client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
    assert_eq!(resources[0]["harness_state"], "unauthenticated");
    assert_eq!(resources[0]["harness_error_state"], "needs-login");
    assert_eq!(resources[0]["state"], "waiting");
    assert_eq!(resources[0]["observation"], "current");
    assert!(resources[0]["since"].is_string());
    assert!(resources[0].get("_status_source").is_none());
    // st serves its presentation of the seat, read from the facts settled above.
    assert_eq!(resources[0]["presentation"]["state"], "needs_login");
    assert_eq!(resources[0]["presentation"]["group"], "needs_login");
    assert_eq!(
        resources[0]["presentation"]["because"],
        "Its harness is not logged in to its provider"
    );
}

#[tokio::test]
async fn a_run_agent_revises_its_mission() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2

  mission "revision" state="ready" {
    goal "Complete mission revision."
     agent "sup" {
       workspace "."
       command "true"
       mission-authority { revise "revision" }
     }
    step "work" { goal "First goal." }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "revision-mission-one")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "revision".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "revision-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let actor = format!("agent/{}/sup", run.id);
    let app = router(state.clone());
    let replacement = r#"
version 2

  mission "revision" state="ready" {
    goal "Complete mission revision."
     agent "sup" {
       workspace "."
       command "true"
       mission-authority { revise "revision" }
     }
    step "work" { goal "Corrected goal." }
  }

"#;
    let (status, revised) = json_request(
        app,
        &format!("/v1/mission-runs/{}/revision", run.id),
        serde_json::to_value(MissionRevisionRequest {
            intent: crate::model::IntentInput {
                kdl: replacement.into(),
                source_name: None,
            },
            actor,
            reason: "the first goal was incomplete".into(),
            idempotency_key: "revision-two".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revised}");
    assert_eq!(revised["status"], "applied");
    assert_ne!(revised["mission_run"]["revision"], run.revision);
    assert_eq!(revised["mission_run"]["root_revision"], run.root_revision);
    assert_eq!(revised["mission_run"]["steps"][0]["status"], "pending");
    assert_eq!(state.store.desired_subjects().unwrap().len(), 1);
}

#[tokio::test]
async fn a_mission_revision_refuses_a_reference_that_does_not_resolve() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"version 2
mission "revision" state="ready" {
  goal "Complete mission revision."
  step "work" { agentless }
}"#;
    state
        .store
        .apply_internal(&parse_intent(source, "node").unwrap(), "revision-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "revision".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "revision-run".into(),
        })
        .unwrap();
    let before = state.store.index().unwrap();
    let (status, error) = json_request(
        router(state.clone()),
        &format!("/v1/mission-runs/{}/revision", run.id),
        serde_json::to_value(MissionRevisionRequest {
            intent: crate::model::IntentInput {
                kdl: source.replace("agentless", r#"assigned-to "agent/example/nobody""#),
                source_name: None,
            },
            actor: "person/test".into(),
            reason: "hand the work to an agent".into(),
            idempotency_key: "revision-missing-agent".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["code"], "mission-revision-blocked");
    assert!(
        error["message"].as_str().unwrap().contains(
            "mission `mission/revision` references missing eligible agent `agent/example/nobody`"
        ),
        "{error}"
    );
    assert_eq!(state.store.index().unwrap(), before);
}

#[tokio::test]
async fn any_agent_retries_failed_work_as_itself() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2

  mission "retry" state="ready" {
    goal "Retry one failed check."
    agent "worker" { workspace "."; command "true" }
    step "check" { goal "Run the check." }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "retry-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "retry".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "retry-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let check = run.steps[0].subject.clone();
    state
        .store
        .set_step_state(&check, "failed", Some("the check failed"))
        .unwrap();
    let retry = |actor: String, key: &str| {
        serde_json::to_value(WorkRetryRequest {
            actor,
            reason: "the check host is back".into(),
            idempotency_key: key.into(),
        })
        .unwrap()
    };
    let path = format!("/v1/work/retry/{}", urlencoding::encode(&check));

    let (status, denied) = json_request(
        router(state.clone()),
        &path,
        retry("daemon/runtime".into(), "retry-unknown"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{denied}");
    assert_eq!(denied["code"], "retry-authority-denied");

    let worker = format!("agent/{}/worker", run.id);
    let (status, retried) = json_request(
        router(state.clone()),
        &path,
        retry(worker.clone(), "retry-worker"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["steps"][0]["status"], "pending");
    assert_eq!(retried["steps"][0]["attempt"], 2);
    assert!(
        state
            .store
            .claims_for(&check, None)
            .unwrap()
            .iter()
            .any(|claim| claim.actor.as_deref() == Some(worker.as_str())),
        "the retry records the agent as its actor"
    );
}

#[tokio::test]
async fn any_agent_sets_a_run_outcome_and_retires_its_mission() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2

  mission "shipped" state="ready" {
    goal "Ship one change."
    agent "worker" { workspace "."; command "true" }
    step "ship" { goal "Ship the change." }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "shipped-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "shipped".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "shipped-run".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    state
        .store
        .set_mission_run_state(&run.id, "failed", "terminal", Some("a gate failed"))
        .unwrap();
    let outcome = |actor: String, key: &str| {
        serde_json::to_value(MissionRunOutcomeRequest {
            actor,
            status: "completed".into(),
            reason: "the change shipped after the gate was fixed".into(),
            idempotency_key: key.into(),
        })
        .unwrap()
    };
    let path = format!(
        "/v1/mission-runs/{}/outcome",
        urlencoding::encode(&run.subject)
    );

    let worker = format!("agent/{}/worker", run.id);
    let (status, completed) = json_request(
        router(state.clone()),
        &path,
        outcome(worker.clone(), "outcome-worker"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["outcome"]["previous_status"], "failed");
    assert_eq!(completed["outcome"]["actor"], worker);

    let retire = |actor: String, key: &str| {
        serde_json::to_value(MissionRetireRequest {
            actor,
            idempotency_key: key.into(),
        })
        .unwrap()
    };
    let path = format!("/v1/missions/{}/retire", urlencoding::encode("shipped"));
    let (status, retired) = json_request(
        router(state.clone()),
        &path,
        retire(worker, "retire-worker"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    assert_eq!(retired["state"], "retired");
}

#[tokio::test]
async fn a_run_revision_accepts_a_loop_round_embedded_mission() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2
mission "loop-review" state="ready" revision-cutover="restart-active" {
  goal "Review the source."
  loop "review" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      step "write" { goal "Write the review." }
    }
  }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "loop-review-initial")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "loop-review".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "loop-review-run".into(),
        })
        .unwrap();
    let replacement = source.replace("Write the review.", "Write a corrected review.");
    let (status, revised) = json_request(
        router(state),
        &format!("/v1/mission-runs/{}/revision", run.id),
        serde_json::to_value(MissionRevisionRequest {
            intent: crate::model::IntentInput {
                kdl: replacement,
                source_name: None,
            },
            actor: "person/test".into(),
            reason: "Move the review to its corrected definition".into(),
            idempotency_key: "loop-review-revision".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revised}");
    assert_eq!(revised["status"], "applied");
    assert_ne!(revised["mission_run"]["revision"], run.revision);
}

#[tokio::test]
async fn seat_rename_follows_free_mode_and_preserves_other_fields() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"version 2
agent "test/target" { workspace "."; command "true"; name "Initial seat" }
"#;
    let request = apply_request(&state, source, "person/test", "rename-fixture");
    let _ = apply(State(state.clone()), Json(request)).await.unwrap();
    let original = state.store.desired_subject_with_writer("agent/test/target").unwrap().unwrap();
    let initial = client_agent_resources(&state.store, false, "before", state.store.index().unwrap()).unwrap();
    assert_eq!(initial.iter().find(|agent| agent["id"] == "agent/test/target").unwrap()["name"], "Initial seat");
    let app = router(state.clone());
    let (status, _) = json_request(app.clone(), "/v1/agents/rename", json!({
        "subject": "test/target", "name": "Denied", "actor": "daemon/test",
        "idempotency_key": "invalid-actor",
    })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(state.store.desired_subject_with_writer("agent/test/target").unwrap().unwrap(), original);
    for (name, key) in [(Some("Renamed seat"), "rename"), (None, "clear")] {
        let (status, body) = json_request(app.clone(), "/v1/agents/rename", json!({
            "subject": "test/target", "name": name, "actor": "agent/test/ungranted",
            "idempotency_key": key,
        })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (mut desired, writer) = state.store.desired_subject_with_writer("agent/test/target").unwrap().unwrap();
        let agents = client_agent_resources(&state.store, false, "after", state.store.index().unwrap()).unwrap();
        assert_eq!(agents.iter().find(|agent| agent["id"] == "agent/test/target").unwrap()["name"],
            name.unwrap_or("test/target"));
        assert_eq!(writer, original.1);
        desired.set_display_name(original.0.member.as_ref().unwrap().display_name.as_deref()).unwrap();
        assert_eq!(desired, original.0);
    }
}

/// Free mode (2026-10-01): within a fleet an agent may do what its person may do. A seat
/// with no declaration and no grant publishes, starts, revises, retires and declares
/// anywhere, stops another seat and itself, and every write records the agent as actor.
#[tokio::test]
async fn an_ungranted_agent_publishes_starts_revises_and_stops_anywhere_as_itself() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let agent = "agent/example/anyone";
    let apply = |source: String, key: &'static str| {
        let app = app.clone();
        let request = serde_json::to_value(apply_request(&state, &source, agent, key)).unwrap();
        async move {
            let (status, body) = json_request(app, "/v1/intent/apply", request).await;
            assert_eq!(status, StatusCode::OK, "{key}: {body}");
            body
        }
    };
    let writer = |subject: &str| {
        state
            .store
            .latest_claim(subject, None)
            .unwrap()
            .and_then(|claim| claim.actor)
    };

    let mission = r#"version 2
mission "elsewhere/jobs/one" state="ready" {
  concurrent-runs
  goal "Run in a namespace no grant names."
  step "work" { goal "First goal." }
}
"#;
    apply(mission.into(), "free-publish").await;
    assert_eq!(writer("mission/elsewhere/jobs/one").as_deref(), Some(agent));

    let revision = state
        .store
        .mission_spec("elsewhere/jobs/one", None)
        .unwrap()
        .unwrap()
        .revision;
    apply(
        format!(
            "version 2\nmission-run \"elsewhere/jobs/one/run\" {{\n  mission {:?}\n  workspace {:?}\n  requester {agent:?}\n}}\n",
            format!("mission/elsewhere/jobs/one@{revision}"),
            root.path().display().to_string(),
        ),
        "free-start",
    )
    .await;
    let run = state
        .store
        .mission_run("elsewhere/jobs/one/run")
        .unwrap()
        .unwrap();
    assert_eq!(run.requester, agent);

    let (status, revised) = json_request(
        app.clone(),
        &format!("/v1/mission-runs/{}/revision", urlencoding::encode(&run.id)),
        serde_json::to_value(MissionRevisionRequest {
            intent: crate::model::IntentInput {
                kdl: mission.replace("First goal.", "Corrected goal."),
                source_name: None,
            },
            actor: agent.into(),
            reason: "the first goal was incomplete".into(),
            idempotency_key: "free-revise".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revised}");
    assert_eq!(revised["status"], "applied");

    // Declaring a seat with authority blocks succeeds; the preview says they are ignored.
    let seat = r#"version 2
agent "example/other" {
  workspace "."
  command "true"
  mission-authority { publish "example/*" }
  seat-authority { stop "example/*" }
}
"#;
    let (status, previewed) = json_request(
        app.clone(),
        "/v1/intent/mission",
        serde_json::to_value(crate::model::MissionRequest {
            intent: crate::model::IntentInput {
                kdl: seat.into(),
                source_name: None,
            },
            at_index: None,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{previewed}");
    assert!(
        previewed["warnings"].as_array().unwrap().iter().any(|warning| {
            warning.as_str().unwrap()
                == "free-mode: `agent/example/other` declares `mission-authority`, `seat-authority`, which st ignores; within a fleet every agent may do what its person may do"
        }),
        "{previewed}"
    );
    apply(seat.into(), "free-declare").await;
    assert_eq!(writer("agent/example/other").as_deref(), Some(agent));

    // It stops another seat, then itself.
    apply(
        "version 2\nstop \"agent/example/other\"\n".into(),
        "free-stop-other",
    )
    .await;
    apply(format!("version 2\nstop {agent:?}\n"), "free-stop-self").await;
    for subject in ["agent/example/other", agent] {
        let desired = state
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == subject)
            .unwrap();
        assert_eq!(desired.kind, "stop", "{subject}");
        assert_eq!(writer(subject).as_deref(), Some(agent), "{subject}");
    }

    // It cancels the run it started, then retires the mission once cleanup ends.
    apply(
        format!(
            "version 2\nmission-run {:?} {{ cancellation \"free-cancel\" {{ reason \"the work was superseded\" }} }}\n",
            run.subject
        ),
        "free-cancel",
    )
    .await;
    assert_eq!(
        state.store.mission_run(&run.id).unwrap().unwrap().phase,
        "cleanup-cancelled"
    );
    state
        .store
        .set_mission_run_state(&run.id, "cancelled", "terminal", Some("cleanup completed"))
        .unwrap();
    let (status, retired) = json_request(
        app,
        &format!(
            "/v1/missions/{}/retire",
            urlencoding::encode("elsewhere/jobs/one")
        ),
        serde_json::to_value(MissionRetireRequest {
            actor: agent.into(),
            idempotency_key: "free-retire".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    assert_eq!(retired["state"], "retired");
}

#[tokio::test]
async fn a_claimed_step_publishes_one_attempt_bound_ready_mission() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2

  mission "bootstrap" state="ready" {
    goal "Complete mission bootstrap."
    agent "planner" {
      workspace "."
      command "true"
      mission-authority { publish "project/*" }
    }
    step "compile" {
      assigned-to "agent/${ST_MISSION_RUN}/planner"
      produces-mission "project/work"
    }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "publish-bootstrap-output")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "bootstrap".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "run-bootstrap-output".into(),
        })
        .unwrap();
    materialize_run_agents(&state, &run);
    let actor = format!("agent/{}/planner", run.id);
    let step = run.steps[0].clone();
    state
        .store
        .set_step_state(&step.subject, "ready", None)
        .unwrap();
    let produced = r#"
version 2

  mission "project/work" state="ready" {
    goal "Complete mission project/work."
    step "inspect" { title "Inspect the project" }
    step "implement" { depends-on { step "inspect" completed } }
  }

"#;
    let app = router(state.clone());
    let (status, output) = json_request(
        app.clone(),
        &format!("/v1/work/mission/{}", urlencoding::encode(&step.subject)),
        serde_json::to_value(MissionProductionRequest {
            intent: crate::model::IntentInput {
                kdl: produced.into(),
                source_name: Some("generated.kdl".into()),
            },
            actor: actor.clone(),
            incarnation: Some("test".into()),
            idempotency_key: "reject-unclaimed-output".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{output}");
    assert_eq!(output["code"], "work-not-claimed");
    state
        .store
        .work_action(
            &step.subject,
            "claim",
            &WorkRequest {
                wait_ms: None,
                actor: Some(actor.clone()),
                incarnation: Some("test".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "claim-bootstrap-output".into(),
            },
        )
        .unwrap();
    let wrong = r#"
version 2

  mission "project/other" state="ready" { goal "Complete mission project/other."; goal "Complete mission project/other."; step "inspect" { } }

"#;
    let (status, output) = json_request(
        app.clone(),
        &format!("/v1/work/mission/{}", urlencoding::encode(&step.subject)),
        serde_json::to_value(MissionProductionRequest {
            intent: crate::model::IntentInput {
                kdl: wrong.into(),
                source_name: Some("wrong.kdl".into()),
            },
            actor: actor.clone(),
            incarnation: Some("test".into()),
            idempotency_key: "reject-wrong-output".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{output}");
    assert_eq!(output["code"], "wrong-mission-output");
    let (status, output) = json_request(
        app,
        &format!("/v1/work/mission/{}", urlencoding::encode(&step.subject)),
        serde_json::to_value(MissionProductionRequest {
            intent: crate::model::IntentInput {
                kdl: produced.into(),
                source_name: Some("generated.kdl".into()),
            },
            actor,
            incarnation: Some("test".into()),
            idempotency_key: "publish-produced-work".into(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{output}");
    assert_eq!(output["mission"], "mission/project/work");
    let revision = output["revision"].as_str().unwrap();
    assert_eq!(revision.len(), 64);
    assert!(
        state
            .store
            .mission_spec("project/work", Some(revision))
            .unwrap()
            .is_some()
    );
    let bound = state
        .store
        .mission_output(&step.subject, 1, &step.definition_hash)
        .unwrap()
        .expect("attempt-bound mission output");
    assert_eq!(bound.revision, revision);
    assert_eq!(bound.claim_id, output["claim_id"]);
}

#[test]
fn silent_send_retry_keeps_original_kind_and_rejects_changed_input() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let kdl = "version 2\nagent \"example/reader\" { command \"true\"; }\n";
    let intent = crate::graph::parse_test_intent(kdl, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "seat-retry")
        .unwrap();
    let request = || MessageSendRequest {
        idempotency_key: "silent-retry".into(),
        from: "agent/example/writer".into(),
        to: "agent/example/reader".into(),
        content: "An update".into(),
        title: None,
        in_reply_to: None,
        tags: vec![crate::silent::SILENT_TAG.into()],
        attachments: vec![],
    };
    let first = accept_message(&state, request(), None, None).unwrap().0;
    assert!(crate::silent::is_held(&first));
    let retry = accept_message(&state, request(), None, None).unwrap().0;
    assert_eq!(first.tags, retry.tags);
    assert_eq!(first.subject, retry.subject);
    for tags in [vec!["different".into()], vec![], vec!["launch".into()]] {
        let mut changed = request();
        changed.tags = tags;
        assert!(accept_message(&state, changed, None, None).is_err());
    }
    let mut changed = request();
    changed.content = "A different update".into();
    assert!(accept_message(&state, changed, None, None).is_err());
    assert_eq!(
        state
            .store
            .claims_for(&first.subject, Some("message.sent"))
            .unwrap()
            .len(),
        1
    );
    let mut forged = request();
    forged.idempotency_key = "forged-tag".into();
    forged.tags = vec!["st3-fault:fake".into()];
    assert!(accept_message(&state, forged, None, None).is_err());
    let mut launch = request();
    launch.idempotency_key = "ordinary-launch-tag".into();
    launch.tags = vec!["launch".into(), crate::silent::SILENT_TAG.into()];
    assert!(crate::silent::is_held(
        &accept_message(&state, launch, None, None).unwrap().0
    ));
    let mut marker = request();
    marker.from = "person/example".into();
    marker.idempotency_key = "signed-marker".into();
    marker.tags = vec![format!("{}{}", crate::silent::REMAINING_PREFIX, usize::MAX)];
    let (key, _) = smallclaims::fleet::MemberKey::generate().unwrap();
    let signature = smallclaims::principal::ClaimSignature::sign(
        &key,
        "",
        "person/example",
        None,
        vec![],
        client_now_ms() as u64,
    );
    let rejected = accept_message(&state, marker, None, Some(signature)).unwrap_err();
    assert_eq!(rejected.code, "reserved-message-tag");
}

#[tokio::test]
async fn claims_endpoint_returns_bounded_cursor_pages() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for (subject, key) in [("host/one", "one"), ("host/two", "two")] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    }
    let app = router(state);
    let (status, first) = get_request(app.clone(), "/v1/claims?limit=1").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["claims"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_u64().unwrap();
    let (status, second) = get_request(
        app.clone(),
        &format!("/v1/claims?limit=1&after_index={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["claims"].as_array().unwrap().len(), 1);
    assert!(second["next_cursor"].is_null());

    let (status, descending) =
        get_request(app, "/v1/claims?limit=1&order=desc&before_index=3").await;
    assert_eq!(status, StatusCode::OK, "{descending}");
    assert_eq!(
        descending["claims"][0]["subject"].as_str(),
        Some("host/two")
    );
}

#[tokio::test]
async fn public_claim_admission_and_idempotency_use_the_exported_schema() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let reconcile_notify = state.notify.clone();
    let app = router(state);

    let (status, schema) = get_request(app.clone(), "/v1/schema").await;
    assert_eq!(status, StatusCode::OK, "{schema}");
    assert_eq!(schema["schema"], st3_schema::SCHEMA_NAME);
    assert_eq!(schema["digest"], st3_schema::registry().digest());

    let forbidden = serde_json::to_value(ClaimInput {
        subject: "host/node".into(),
        kind: "transport.observed".into(),
        actor: None,
        fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("caller-transport".into()),
    })
    .unwrap();
    let (status, body) = json_request(app.clone(), "/v1/claims", forbidden).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "claim-write-forbidden");

    let resource = ClaimInput {
        subject: "resource/example".into(),
        kind: "resource.observed".into(),
        actor: Some("person/alex".into()),
        fields: BTreeMap::from([
            (
                "kind".into(),
                Value::String("custom.example.measurement".into()),
            ),
            ("value".into(), Value::from(1)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("caller-resource".into()),
    };
    let (status, first) = json_request(
        app.clone(),
        "/v1/claims",
        serde_json::to_value(&resource).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    tokio::time::timeout(
        std::time::Duration::from_millis(50),
        reconcile_notify.notified(),
    )
    .await
    .expect("a new claim must wake reconciliation");
    let (status, retry) = json_request(
        app.clone(),
        "/v1/claims",
        serde_json::to_value(&resource).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retry}");
    assert_eq!(retry["id"], first["id"]);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            reconcile_notify.notified(),
        )
        .await
        .is_err(),
        "an idempotent claim replay must not wake reconciliation"
    );

    let mut changed = resource;
    changed.fields.insert("value".into(), Value::from(2));
    let (status, mismatch) =
        json_request(app, "/v1/claims", serde_json::to_value(changed).unwrap()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{mismatch}");
    assert_eq!(mismatch["code"], "idempotency-mismatch");
}

#[tokio::test]
async fn harness_diagnostic_has_a_dedicated_idempotent_local_operation() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = router(state.clone());
    let request = json!({
        "actor": "agent/recovery-owner",
        "code": "driver-failed",
        "reason": "the harness transport exited",
        "severity": "error",
        "status": "active",
        "incarnation_id": "runtime:one",
        "idempotency_key": "harness-diagnostic-test"
    });
    let (status, first) =
        json_request(app.clone(), "/v1/diagnostics/harness", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["kind"], "harness.diagnostic");
    assert_eq!(first["subject"], "agent/recovery-owner");
    let (status, replay) = json_request(app, "/v1/diagnostics/harness", request).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["id"], first["id"]);
    assert_eq!(
        state
            .store
            .claims_for("agent/recovery-owner", Some("harness.diagnostic"))
            .unwrap()
            .len(),
        1
    );

    let (status, denied) =
        json_request(fabric_router(state), "/v1/diagnostics/harness", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
}

#[tokio::test]
async fn an_invalid_gate_result_does_not_consume_its_capability() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let subject = "gate-operation/test/result";
    let (capability, _) = store
        .issue_capability("gate-result", subject, None, 60_000)
        .unwrap();
    let app = router(state);
    let request = |idempotency_key: String| {
        serde_json::to_value(GateResultRequest {
            operation_capability: capability.clone(),
            verdict: "pass".into(),
            reason: "the evidence passes".into(),
            evidence: Vec::new(),
            idempotency_key,
        })
        .unwrap()
    };

    let (status, invalid) =
        json_request(app.clone(), "/v1/gate-results", request("x".repeat(513))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["code"], "invalid-idempotency-key");
    assert!(!store.capability(&capability, "gate-result").unwrap().used);

    let (status, accepted) =
        json_request(app, "/v1/gate-results", request("valid-gate-result".into())).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["body"]["fields"]["verdict"], "pass");
    assert!(store.capability(&capability, "gate-result").unwrap().used);
}

#[tokio::test]
async fn human_review_list_and_decisions_use_exact_current_requests() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = r#"
version 2

  agent "worker" { workspace "/tmp"; command "true" }
  mission "review-api" state="ready" {
    goal "Complete mission review-api."
    step "approval" {
      goal "Submit the candidate."
      assigned-to "agent/worker"
      gate "human-review" type="human" { reviewer "person/alex" }
    }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "review-api-mission")
        .unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "review-api".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "review-api-run".into(),
        })
        .unwrap();
    let step = &run.steps[0];
    assert!(!step.agentless, "{step:?}");
    let claimant = step.assigned_to.clone().expect("the step has an assignee");
    state
        .store
        .set_step_state(&step.subject, "ready", None)
        .unwrap();
    state
        .store
        .work_action(
            &step.subject,
            "claim",
            &crate::model::WorkRequest {
                wait_ms: None,
                actor: Some(claimant.clone()),
                incarnation: Some("test-incarnation".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "review-api-claim".into(),
            },
        )
        .unwrap();
    state
        .store
        .work_action(
            &step.subject,
            "complete",
            &crate::model::WorkRequest {
                wait_ms: None,
                actor: Some(claimant.clone()),
                incarnation: Some("test-incarnation".into()),
                summary: Some("Candidate submitted for review".into()),
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "review-api-submit".into(),
            },
        )
        .unwrap();
    let request_fields = |owner: String, definition: String, operation: &str| {
        BTreeMap::from([
            ("owner".into(), Value::String(owner)),
            ("reviewer".into(), Value::String("person/alex".into())),
            (
                "question".into(),
                Value::String("Is the candidate ready?".into()),
            ),
            (
                "review_targets".into(),
                Value::Array(vec![
                    Value::String("resource/review/candidate".into()),
                    Value::String("doc/review/report@abc".into()),
                ]),
            ),
            (
                "decisions".into(),
                Value::Array(vec![
                    Value::String("approved".into()),
                    Value::String("rejected".into()),
                ]),
            ),
            ("operation".into(), Value::String(operation.into())),
            (
                "mission_revision".into(),
                Value::String(run.revision.clone()),
            ),
            ("step_definition".into(), Value::String(definition)),
            ("attempt".into(), Value::from(step.attempt)),
        ])
    };
    let step_request = state
        .store
        .append_claim(&ClaimInput {
            subject: "gate-operation/review-api/approval".into(),
            kind: "gate.requested".into(),
            actor: None,
            fields: request_fields(
                step.subject.clone(),
                step.definition_hash.clone(),
                "gate-operation/review-api/approval",
            ),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("step-review-request".into()),
        })
        .unwrap();
    let mission_request = state
        .store
        .append_claim(&ClaimInput {
            subject: "gate-operation/review-api/mission".into(),
            kind: "gate.requested".into(),
            actor: None,
            fields: request_fields(
                run.subject.clone(),
                run.revision.clone(),
                "gate-operation/review-api/mission",
            ),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("mission-review-request".into()),
        })
        .unwrap();
    let store = state.store.clone();
    let app = router(state);
    let (status, listed) = get_request(app.clone(), "/v1/reviews").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["request"], step_request.id);
    assert_eq!(listed[0]["owner"], step.subject);
    assert_eq!(listed[0]["step"], "approval");
    assert_eq!(listed[0]["review_targets"].as_array().unwrap().len(), 2);
    assert_eq!(listed[1]["request"], mission_request.id);
    assert_eq!(listed[1]["owner"], run.subject);
    assert!(listed[1].get("step").is_none());

    let (status, selected) =
        get_request(app.clone(), "/v1/reviews?reviewer=person%2Falex").await;
    assert_eq!(status, StatusCode::OK, "{selected}");
    assert_eq!(selected.as_array().unwrap().len(), 2);
    let (status, attention) =
        get_request(app.clone(), "/v1/attention?person=person%2Falex").await;
    assert_eq!(status, StatusCode::OK, "{attention}");
    assert_eq!(attention.as_array().unwrap().len(), 2);
    assert!(
        attention
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == "human-gate")
    );
    assert!(
        attention.as_array().unwrap().iter().all(|item| {
            item["actions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|action| action["argv"][1] == "alerts" && action["argv"][4] == "--as")
        }),
        "{attention}"
    );

    let (status, filtered) =
        get_request(app.clone(), "/v1/reviews?reviewer=person%2Fsomeone-else").await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert_eq!(filtered, json!([]));

    store
        .append_claim(&ClaimInput {
            subject: step_request.subject.clone(),
            kind: "gate.result".into(),
            actor: Some("person/someone-else".into()),
            fields: BTreeMap::from([
                ("verdict".into(), Value::String("pass".into())),
                ("request".into(), Value::String(step_request.id.clone())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("wrong-review-result".into()),
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: step_request.subject.clone(),
            kind: "gate.result".into(),
            actor: Some("person/alex".into()),
            fields: BTreeMap::from([("verdict".into(), Value::String("pass".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("unbound-review-result".into()),
        })
        .unwrap();
    let (_, still_pending) = get_request(app.clone(), "/v1/reviews").await;
    assert_eq!(still_pending.as_array().unwrap().len(), 2);

    // A newer build asks the mission gate again as a new operation. The reviewer sees one
    // review, aged from the first request, and the decision answers the request the gate
    // now waits on.
    let mut again_fields = request_fields(
        run.subject.clone(),
        run.revision.clone(),
        "gate-operation/review-api/mission-again",
    );
    again_fields.insert("mode".into(), Value::String("approve".into()));
    let mission_again = store
        .append_claim(&ClaimInput {
            subject: "gate-operation/review-api/mission-again".into(),
            kind: "gate.requested".into(),
            actor: None,
            fields: again_fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("mission-review-again".into()),
        })
        .unwrap();
    let (_, asked_again) = get_request(app.clone(), "/v1/reviews").await;
    assert_eq!(asked_again.as_array().unwrap().len(), 2, "{asked_again}");
    assert_eq!(asked_again[1]["request"], mission_again.id);
    assert_eq!(
        asked_again[1]["requested_at_unix_ms"],
        json!(mission_request.accepted_at_unix_ms)
    );

    let body = |actor: &str| {
        serde_json::to_value(ReviewRequest {
            delegation: None,
            decision: "approved".into(),
            reason: None,
            actor: Some(actor.into()),
            expected_subject: None,
        })
        .unwrap()
    };
    let (status, rejected) = json_request(
        app.clone(),
        &format!("/v1/reviews/{}", run.subject),
        body("person/someone-else"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
    assert_eq!(rejected["code"], "wrong-reviewer");

    let (status, accepted_mission) = json_request(
        app.clone(),
        &format!("/v1/reviews/{}", run.subject),
        body("person/alex"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted_mission}");
    assert_eq!(
        accepted_mission["body"]["fields"]["request"],
        mission_again.id
    );
    assert_eq!(accepted_mission["subject"], mission_again.subject);
    assert_eq!(accepted_mission["body"]["fields"]["verdict"], "pass");

    let missing_reason = serde_json::to_value(ReviewRequest {
        delegation: None,
        decision: "rejected".into(),
        reason: None,
        actor: Some("person/alex".into()),
        expected_subject: None,
    })
    .unwrap();
    let (status, invalid) = json_request(
        app.clone(),
        &format!("/v1/reviews/{}", step.subject),
        missing_reason,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["code"], "missing-review-reason");

    let reject = serde_json::to_value(ReviewRequest {
        delegation: None,
        decision: "rejected".into(),
        reason: Some("the evidence is incomplete".into()),
        actor: Some("person/alex".into()),
        expected_subject: None,
    })
    .unwrap();
    let (status, accepted_step) = json_request(
        app.clone(),
        &format!("/v1/reviews/{}", step.subject),
        reject,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted_step}");
    assert_eq!(accepted_step["body"]["fields"]["request"], step_request.id);
    assert_eq!(accepted_step["body"]["fields"]["verdict"], "fail");
    assert_eq!(accepted_step["body"]["evidence"][0], step_request.id);
    let messages = store.messages(Some(&claimant), false).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].content.contains("the evidence is incomplete"));

    store
        .set_mission_run_state(&run.id, "failed", "terminal", None)
        .unwrap();
    let feedback_run = store
        .create_mission_run(&MissionRunRequest {
            mission: "review-api".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "review-api-feedback-run".into(),
        })
        .unwrap();
    let feedback_step = &feedback_run.steps[0];
    let feedback_claimant = feedback_step.assigned_to.clone().unwrap();
    store
        .set_step_state(&feedback_step.subject, "ready", None)
        .unwrap();
    for action in ["claim", "complete"] {
        store
            .work_action(
                &feedback_step.subject,
                action,
                &crate::model::WorkRequest {
                    wait_ms: None,
                    actor: Some(feedback_claimant.clone()),
                    incarnation: Some("test-incarnation".into()),
                    summary: Some("Candidate submitted".into()),
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("feedback-api-{action}"),
                },
            )
            .unwrap();
    }
    let mut feedback_fields = request_fields(
        feedback_step.subject.clone(),
        feedback_step.definition_hash.clone(),
        "gate-operation/review-api/feedback",
    );
    feedback_fields.insert("mode".into(), Value::String("feedback".into()));
    feedback_fields.insert("decisions".into(), json!(["approved", "changes-requested"]));
    let feedback_request = store
        .append_claim(&ClaimInput {
            subject: "gate-operation/review-api/feedback".into(),
            kind: "gate.requested".into(),
            actor: None,
            fields: feedback_fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("feedback-api-request".into()),
        })
        .unwrap();
    let (_, attention) = get_request(app.clone(), "/v1/attention?person=person%2Falex").await;
    assert_eq!(attention[0]["review_mode"], "feedback");
    assert!(
        attention[0]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["label"] == "request changes")
    );
    let feedback_path = format!("/v1/reviews/{}", feedback_step.subject);
    let (status, invalid) = json_request(
        app.clone(),
        &feedback_path,
        json!({
            "decision": "rejected", "reason": "More detail", "actor": "person/alex"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["code"], "review-decision-not-offered");
    let (status, accepted) = json_request(
        app.clone(),
        &feedback_path,
        json!({
            "decision": "changes-requested", "reason": "Add a source.", "actor": "person/alex"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["body"]["fields"]["verdict"], "feedback");
    assert_eq!(accepted["body"]["fields"]["request"], feedback_request.id);

    let (_, empty) = get_request(app, "/v1/reviews?reviewer=person%2Falex").await;
    assert_eq!(empty, json!([]));
}

#[tokio::test]
async fn legacy_attention_mutations_report_migration_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    let app = router(state);
    for (path, body) in [
        (
            "/v1/attention",
            json!({"reviewer":"person/avery","title":"Decide","reason":"Choose a date","severity":"warning","targets":[],"actor":"agent/alder.asker","idempotency_key":"legacy-ask"}),
        ),
        (
            "/v1/attention/resolve/attention%2Flegacy",
            json!({"outcome":"resolved","actor":"person/avery","idempotency_key":"legacy-done"}),
        ),
        (
            "/v1/attention/withdraw/attention%2Flegacy",
            json!({"reason":"Ended","actor":"agent/alder.asker","idempotency_key":"legacy-cancel"}),
        ),
    ] {
        let (status, error) = json_request(app.clone(), path, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["code"], "attention-migrated");
    }
    assert!(store.attention_requests(None, true).unwrap().is_empty());
    assert!(store.messages(None, true).unwrap().is_empty());
}

#[tokio::test]
async fn client_now_never_lists_a_fault() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = state.store.clone();
    store
        .record_operational_failure(
            "daemon/high",
            &AttentionRequest {
                reviewer: "person/alex".into(),
                title: "Fault error".into(),
                reason: "The subscription needs a correction.".into(),
                severity: "error".into(),
                targets: vec!["daemon/high".into()],
                actor: "daemon/runtime".into(),
                idempotency_key: "daemon/high".into(),
            },
        )
        .unwrap();
    assert_eq!(store.fault_snapshot(client_now_ms()).unwrap().len(), 1);
    let app = router(state);
    for path in ["/v1/client/now?person=person%2Falex", "/v1/client/now"] {
        let (status, page) = get_request(app.clone(), path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"], json!([]), "{path}");
    }
}

#[test]
fn legacy_attention_from_live_and_retired_requesters_is_not_current() {
    let store = Store::open_memory("node").unwrap();
    let apply = |intent: &crate::model::NormalizedIntent, key: &str| {
        let planned = store
            .mission(
                intent,
                crate::model::IntentInput {
                    kdl: key.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(intent, &planned.subject_tokens, key).unwrap();
    };
    let parse = |source: &str| crate::graph::parse_intent(source, "node").unwrap();
    apply(
        &parse(
            r#"version 2
agent "stopped" { workspace "/tmp"; command "true" }
agent "live" { workspace "/tmp"; command "true" }
mission "standing" state="ready" {
  goal "Keep a seat."
  step "hold" { agentless }
}
"#,
        ),
        "requesters",
    );
    let run = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "standing".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "standing-run".into(),
        })
        .unwrap();
    // The run owns this seat, as a standing mission owns its agents.
    let mut owned = parse(
        r#"version 2
agent "seat" { workspace "/tmp"; command "true" }
"#,
    );
    let desired = owned.subjects.get_mut("agent/node.seat").unwrap();
    desired.owner_run = Some(run.subject.clone());
    desired.owner_generation = Some(run.generation.clone());
    apply(&owned, "owned-seat");
    let seat = "agent/node.seat".to_owned();
    for (subject, actor) in [
        ("attention/from-stopped", "agent/node.stopped"),
        ("attention/from-live", "agent/node.live"),
        ("attention/from-seat", seat.as_str()),
    ] {
        store
            .request_attention(
                subject,
                &AttentionRequest {
                    reviewer: "person/alex".into(),
                    title: format!("Fault from {actor}"),
                    reason: "a person must decide".into(),
                    severity: "warning".into(),
                    targets: Vec::new(),
                    actor: actor.into(),
                    idempotency_key: format!("{subject}:requested"),
                },
            )
            .unwrap();
    }
    let reasons = |store: &Store| {
        client_attention_resources(store, Some("person/alex"), false)
            .unwrap()
            .into_iter()
            .map(|resource| {
                (
                    resource["source_id"].as_str().unwrap().to_owned(),
                    resource["operational"]["reasons"].clone(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    assert!(
        reasons(&store)
            .values()
            .all(|reasons| reasons == &json!([]))
    );

    apply(
        &parse("version 2\nstop \"agent/node.stopped\"\n"),
        "stop-requester",
    );
    store
        .set_mission_run_state(&run.id, "cancelled", "terminal", Some("moved to a seat"))
        .unwrap();

    assert!(reasons(&store).is_empty(), "legacy rows are audit data");
}

#[test]
fn source_failure_priorities_order_faults_and_never_reach_attention() {
    let store = Store::open_memory("alder").unwrap();
    for severity in ["warning", "error"] {
        store
            .record_operational_failure(
                severity,
                &AttentionRequest {
                    reviewer: "person/avery".into(),
                    title: format!("{severity} fault"),
                    reason: "The disk needs attention.".into(),
                    severity: severity.into(),
                    targets: vec![format!("daemon/{severity}")],
                    actor: "daemon/runtime".into(),
                    idempotency_key: severity.into(),
                },
            )
            .unwrap();
    }
    let faults = store.fault_snapshot(client_now_ms()).unwrap();
    assert_eq!(faults[0].item.priority, "high");
    assert_eq!(faults[1].item.priority, "normal");
    for history in [false, true] {
        assert!(
            client_attention_resources(&store, Some("person/avery"), history)
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn delegated_person_work_api_preserves_identity_and_closes_person_message() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let intent = crate::graph::parse_internal_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "delegation-api-agent")
        .unwrap();
    let actor = format!("agent/{}.asker", state.store.origin());
    let app = router(state.clone());
    for (subject, from, to, content) in [
        (
            "message/instruction",
            "person/avery",
            actor.as_str(),
            "Friday; archive the notice; read the update",
        ),
        ("message/notice", actor.as_str(), "person/avery", "Notice"),
    ] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "message.sent".into(),
                actor: Some(from.into()),
                fields: BTreeMap::from([
                    ("from".into(), json!(from)),
                    ("to".into(), json!(to)),
                    ("content".into(), json!(content)),
                    ("status".into(), json!("sent")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(subject.into()),
            })
            .unwrap();
    }
    let (status,policy)=json_request(app.clone(),"/v1/work/delegation",json!({"person":"person/avery","actor":"person/avery","actions":["answer-ask","close-item","record-go-stop"],"evidence":[state.store.claims_for("message/instruction",Some("message.sent")).unwrap()[0].id],"idempotency_key":"allow"})).await;
    assert_eq!(status, StatusCode::OK, "{policy}");
    let (_,ask)=json_request(app.clone(),"/v1/work/ask",json!({"person":"person/avery","title":"Choose a date","reason":"A date","actor":actor,"new_run":"date","idempotency_key":"ask"})).await;
    let episode = state
        .store
        .claims_for(ask["subject"].as_str().unwrap(), Some("work.person-asked"))
        .unwrap()[0]
        .id
        .clone();
    let proof = json!({"person":"person/avery","policy":policy["id"],"message":"message/instruction","quote":"Friday","episode":episode});
    let body = json!({"subject":ask["subject"],"actor":actor,"summary":"Friday","episode":episode,"delegation":proof,"evidence":[],"idempotency_key":"answer"});
    let (status, done) = json_request(app.clone(), "/v1/work/done", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let projection = state
        .store
        .step_run(ask["subject"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(projection.person_answers[0].respondent, actor);
    assert_eq!(
        projection.person_answers[0].acted_for.as_deref(),
        Some("person/avery")
    );
    let (_, again) = json_request(app.clone(), "/v1/work/done", body).await;
    assert_eq!(again["status"], "completed");
    let mut close_proof = proof;
    close_proof["quote"] = json!("archive the notice");
    close_proof["episode"] = json!(
        state
            .store
            .claims_for("message/notice", Some("message.sent"))
            .unwrap()[0]
            .id
    );
    let (status,closed)=json_request(app.clone(),"/v1/messages/message%2Fnotice/claims",json!({"lifecycle":"closed","actor":actor,"delegation":close_proof,"evidence":[],"idempotency_key":"close"})).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["actor"], actor);
    assert_eq!(closed["body"]["fields"]["acted_for"], "person/avery");
    assert_eq!(
        state
            .store
            .message("message/notice")
            .unwrap()
            .unwrap()
            .status,
        "closed"
    );
    let (status, update) = json_request(app.clone(), "/v1/work/ask", json!({
        "person":"person/avery","title":"Release status","reason":"The release is ready","actor":actor,
        "request":{"version":1,"type":"update","about":"message/instruction"},"idempotency_key":"update"
    })).await;
    assert_eq!(status, StatusCode::OK, "{update}");
    let card = client_attention_resources(&state.store, Some("person/avery"), false)
        .unwrap()
        .into_iter()
        .find(|card| card["source_id"] == update["subject"])
        .unwrap();
    let mut read_proof = close_proof;
    read_proof["quote"] = json!("read the update");
    read_proof["episode"] = json!(
        state
            .store
            .claims_for(
                update["subject"].as_str().unwrap(),
                Some("work.person-asked")
            )
            .unwrap()[0]
            .id
    );
    let read_body = json!({"outcome":"resolved","actor":actor,"delegation":read_proof,"idempotency_key":"read-update"});
    let path = format!(
        "/v1/attention/resolve/{}",
        urlencoding::encode(card["id"].as_str().unwrap())
    );
    let (status, read) = json_request(app.clone(), &path, read_body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["actor"], actor);
    let (status, repeated) = json_request(app, &path, read_body).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
}

#[tokio::test]
async fn person_work_routes_share_snapshot_and_reject_stale_or_wrong_responses() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let intent = crate::graph::parse_internal_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "api-person-asker")
        .unwrap();
    let actor = format!("agent/{}.asker", state.store.origin());
    let app = router(state.clone());
    let body = json!({"person":"person/avery","title":"Choose a date","reason":"Reply with a date","actor":actor,"new_run":"release-date","idempotency_key":"api-ask"});
    let (status, ask) = json_request(app.clone(), "/v1/work/ask", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{ask}");
    let (_, duplicate) = json_request(app.clone(), "/v1/work/ask", body).await;
    assert_eq!(ask["subject"], duplicate["subject"]);
    let (_, attention) = get_request(app.clone(), "/v1/attention?person=person%2Favery").await;
    assert_eq!(attention[0]["subject"], ask["subject"]);
    let (_, now) = get_request(app.clone(), "/v1/client/now?person=person%2Favery").await;
    assert_eq!(now["items"][0]["source_id"], ask["subject"]);
    assert_eq!(now["items"][0]["actions"], json!(["work.done"]));
    let episode = attention[0]["episode"].clone();
    let mut done = json!({"subject":ask["subject"],"actor":"person/robin","summary":"Friday","episode":episode,"evidence":[],"idempotency_key":"api-done"});
    let (_, refused) = json_request(app.clone(), "/v1/work/done", done.clone()).await;
    assert_eq!(refused["code"], "forbidden");
    done["actor"] = json!("person/avery");
    done["episode"] = json!("old");
    let (_, stale) = json_request(app.clone(), "/v1/work/done", done.clone()).await;
    assert_eq!(stale["code"], "stale-fence");
    done["episode"] = episode;
    let (status, completed) = json_request(app.clone(), "/v1/work/done", done.clone()).await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["status"], "completed");
    let (_, again) = json_request(app.clone(), "/v1/work/done", done).await;
    assert_eq!(again["status"], "completed");
    let (_, empty) = get_request(app, "/v1/client/now?person=person%2Favery").await;
    assert_eq!(empty["items"], json!([]));
    // No step waits on a new-run ask; its requester hears the answer once, by message.
    let told = state.store.messages(Some(&actor), true).unwrap();
    assert_eq!(told.len(), 1);
    assert!(told[0].content.contains("Friday"));
}

#[tokio::test]
async fn message_pages_are_bounded_and_stable_across_new_writes() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    for index in 0..205 {
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("message/page-{index:03}"),
                kind: "message.sent".into(),
                actor: Some("agent/sender".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("agent/sender".into())),
                    ("to".into(), Value::String("agent/receiver".into())),
                    ("content".into(), Value::String(format!("body {index}"))),
                    ("status".into(), Value::String("sent".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let app = router(state.clone());
    let mut cursor: Option<String> = None;
    let mut seen = Vec::new();
    loop {
        let mut path = "/v1/messages/page?to=agent%2Freceiver&limit=100".to_owned();
        if let Some(cursor) = &cursor {
            path.push_str(&format!("&cursor={}", urlencoding::encode(cursor)));
        }
        let (status, page) = get_request(app.clone(), &path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 100);
        seen.extend(
            items
                .iter()
                .map(|item| item["subject"].as_str().unwrap().to_owned()),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if seen.len() == 100 {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "message/new-after-page".into(),
                    kind: "message.sent".into(),
                    actor: Some("agent/sender".into()),
                    fields: BTreeMap::from([
                        ("from".into(), Value::String("agent/sender".into())),
                        ("to".into(), Value::String("agent/receiver".into())),
                        ("content".into(), Value::String("late".into())),
                        ("status".into(), Value::String("sent".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 205);
    assert_eq!(seen.first().unwrap(), "message/page-000");
    assert_eq!(seen.last().unwrap(), "message/page-204");
    let (status, exact) = get_request(app, "/v1/messages/read/message%2Fpage-204").await;
    assert_eq!(status, StatusCode::OK, "{exact}");
    assert_eq!(exact["content"], "body 204");
}
#[tokio::test]
async fn harness_event_endpoint_requires_this_native_seats_local_peer_and_runtime() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let subject = "agent/example/event-api";
    state
        .store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("status".into(), json!("running")),
                ("incarnation_id".into(), json!("runtime-a")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let request = json!({"runtime_incarnation":"runtime-a", "sequence":1,
        "claim":{"subject":subject,"kind":"harness.observed","actor":subject,
            "fields":{"state":"idle","driver":"claude","incarnation_id":"runtime-a"},
            "evidence":[],"idempotency_key":"fixture-event"}});
    let app = router(state);
    let (status, _) = json_request(app.clone(), "/v1/harness-events", request.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let peer = NativeDeliveryPeer {
        agent: "agent/example/foreign".into(),
        transport: "claude-channel",
        pid: 7,
        archives_inbox: true,
    };
    let (status, _) = json_request(
        app.clone().layer(Extension(peer)),
        "/v1/harness-events",
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let peer = NativeDeliveryPeer {
        agent: subject.into(),
        transport: "claude-channel",
        pid: 7,
        archives_inbox: true,
    };
    let app = app.layer(Extension(peer));
    let (status, first) =
        json_request(app.clone(), "/v1/harness-events", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, replay) =
        json_request(app.clone(), "/v1/harness-events", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(first["body"]["fields"], replay["body"]["fields"]);
    let mut usage = request.clone();
    usage["sequence"] = json!(2);
    usage["claim"]["kind"] = json!("harness.timeline");
    usage["claim"]["fields"] = json!({
        "operation":"append", "entry_id":"response-a", "source_id":"source/response-a",
        "sequence":1, "revision":1, "role":"system", "entry_type":"usage", "final":true,
        "driver":"claude", "incarnation_id":"runtime-a", "observed_at_unix_ms":1,
        "body":{"semantics":"response", "driver":"claude", "model":"fixture-model",
            "input_tokens":10, "output_tokens":5, "cached_tokens":2, "total_tokens":17}
    });
    usage["claim"]["fields"]["observed_at_unix_ms"] = json!(client_now_ms() as u64);
    for _ in 0..2 {
        let (status, body) =
            json_request(app.clone(), "/v1/harness-events", usage.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, rollup) = get_request(
        app.clone(),
        &format!("/v1/usage?since_ms=0&until_ms={}", client_now_ms() + 60_000),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rollup}");
    assert_eq!(
        rollup["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["total_tokens"].as_u64().unwrap_or(0))
            .sum::<u64>(),
        17,
        "{rollup}"
    );
    let mut stale = request;
    stale["runtime_incarnation"] = json!("retired-runtime");
    let (status, body) = json_request(app, "/v1/harness-events", stale).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
