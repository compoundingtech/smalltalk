//! HTTP regressions for continuations issued to one authenticated grant policy.
use super::*;

fn cursor_fixture() -> Harness {
    let mut h = Harness::new();
    h.ask("grant-cursor-first", "Private continuation question one");
    h.ask("grant-cursor-second", "Private continuation question two");
    let source = format!(r#"version 2
agent "cursor.worker" {{ workspace {:?}; command "true"; restart always; }}
mission "cursor-mission-one" state="ready" {{
  goal "First cursor fixture mission"
  step "work" {{ assigned-to "{AGENT}" }}
}}
mission "cursor-mission-two" state="ready" {{
  goal "Second cursor fixture mission"
  step "work" {{ assigned-to "{AGENT}" }}
}}
"#, h.root.path());
    h.state.store.apply_internal(
        &st3::graph::parse_intent(&source, "assistant-native").unwrap(),
        "grant-cursor-fixture",
    ).unwrap();
    for agent in [AGENT, "agent/cursor.worker"] {
        h.state.store.append_claim(&ClaimInput {
            subject: agent.into(), kind: "runtime.observed".into(), actor: Some(agent.into()),
            fields: serde_json::from_value(json!({"status":"running","runtime_id":agent,
                "incarnation_id":format!("cursor-{}", agent.trim_start_matches("agent/")),
                "terminal":true})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    for mission in ["cursor-mission-one", "cursor-mission-two"] {
        let run = h.state.store.create_mission_run(&st3::model::MissionRunRequest {
            mission: mission.into(), revision: None,
            workspace: h.root.path().to_string_lossy().into_owned(),
            requester: Some(OWNER.into()), mode: Some("run".into()),
            inputs: Default::default(), idempotency_key: format!("grant-cursor-{mission}"),
        }).unwrap();
        h.state.store.set_step_state(&run.steps[0].subject, "ready", None).unwrap();
    }
    seed_native_conversation(&mut h);
    for number in 1..=2 {
        h.state.store.append_claim(&ClaimInput {
            subject: format!("message/grant-cursor-{number}"), kind: "message.sent".into(),
            actor: Some(AGENT.into()),
            fields: serde_json::from_value(json!({
                "from":AGENT, "to":OWNER, "content":format!("orchid cursor message {number}"), "status":"sent"
            })).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    h
}

fn seed_native_conversation(h: &mut Harness) {
    let home = h.root.path().join("home");
    let transcript = home.join(".codex/sessions/2026/10/11/assistant.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let native_id = "grant-cursor-native-thread";
    let incarnation = "native-pty:grant-cursor";
    let provider_incarnation = "grant-cursor-provider";
    let mut records = vec![json!({
        "type":"session_meta", "timestamp":"2026-10-11T12:00:00Z",
        "payload":{"id":native_id, "cwd":h.root.path(), "source":"test"}
    }).to_string()];
    for number in 1..=3 {
        records.push(json!({
            "type":"response_item", "timestamp":format!("2026-10-11T12:00:0{number}Z"),
            "payload":{"type":"message", "role":"assistant", "id":format!("answer-{number}"),
                "content":[{"type":"output_text", "text":format!("Cursor chat answer {number}")}]}
        }).to_string());
    }
    std::fs::write(transcript, format!("{}\n", records.join("\n"))).unwrap();
    let directory = h.state.state_dir.join("drivers")
        .join(&hex::encode(<sha2::Sha256 as sha2::Digest>::digest(AGENT.as_bytes()))[..24])
        .join("state");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("runtime.json"), serde_json::to_vec(&json!({
        "agent":AGENT.trim_start_matches("agent/"), "incarnation":provider_incarnation
    })).unwrap()).unwrap();
    std::fs::write(directory.join("binding.json"), serde_json::to_vec(&json!({
        "agent":AGENT.trim_start_matches("agent/"), "runtimeIncarnation":provider_incarnation,
        "threadId":native_id
    })).unwrap()).unwrap();
    for (kind, fields) in [
        ("runtime.observed", json!({"status":"running", "runtime_id":"grant-cursor-pty",
            "incarnation_id":incarnation, "terminal":true})),
        ("harness.observed", json!({"state":"working", "driver":"codex",
            "incarnation_id":incarnation, "evidence_incarnation":provider_incarnation})),
    ] {
        h.state.store.append_claim(&ClaimInput {
            subject: AGENT.into(), kind: kind.into(), actor: Some(AGENT.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    h.state.native_session_home = Some(home);
    h.local = st3::api::router(h.state.clone());
    h.fabric = st3::api::fabric_router(h.state.clone());
}

fn cursor_routes() -> Vec<(String, AssistantScope)> {
    vec![
        ("attention".into(), AssistantScope::Attention),
        ("attention-rich".into(), AssistantScope::Attention),
        ("missions".into(), AssistantScope::Missions),
        ("missions-rich".into(), AssistantScope::Missions),
        ("agents".into(), AssistantScope::Agents),
        ("agents-rich".into(), AssistantScope::Agents),
        ("work".into(), AssistantScope::Work),
        ("conversations".into(), AssistantScope::Conversation),
        (format!("agent-queue?agent_id={}", urlencoding::encode(AGENT)), AssistantScope::Agents),
        (format!("conversation-timeline?agent_id={}", urlencoding::encode(AGENT)), AssistantScope::Conversation),
        ("conversation-search?text=orchid".into(), AssistantScope::Conversation),
    ]
}

fn first_path(route: &str) -> String {
    let separator = if route.contains('?') { '&' } else { '?' };
    format!("/v1/client/assistant/{route}{separator}limit=1&max_response_bytes=16384")
}

async fn first_page(h: &Harness, paired: &AssistantPairedSession, route: &str) -> (Value, String) {
    let path = first_path(route);
    let (status, first) = h.assistant(paired, "GET", &path, &()).await;
    assert_eq!(status, StatusCode::OK, "{route}: {first}");
    assert_eq!(first["value"]["items"].as_array().unwrap().len(), 1, "{route}: {first}");
    assert_eq!(first["value"]["page"]["has_more"], true, "{route}: fixture must have multiple rows");
    let cursor = first["value"]["page"]["next_cursor"].as_str()
        .unwrap_or_else(|| panic!("{route}: fixture must issue a continuation: {first}")).to_owned();
    assert!(!cursor.is_empty(), "{route}");
    (first, cursor)
}

async fn continue_page(h: &Harness, paired: &AssistantPairedSession, route: &str, cursor: &str) -> (StatusCode, Value) {
    h.assistant(paired, "GET", &format!("{}&cursor={}", first_path(route), urlencoding::encode(cursor)), &()).await
}

fn assert_refused(status: StatusCode, value: &Value, route: &str) {
    assert!(!status.is_success(), "{route}: continuation must be refused: {value}");
    assert!(value.get("value").is_none(), "{route}: refusal must not release a cached page: {value}");
    assert!(value.get("code").and_then(Value::as_str).is_some(), "{route}: {value}");
    assert!(!value.to_string().contains("Private continuation question"), "{route}: {value}");
}

#[tokio::test]
async fn paginated_routes_allow_same_grant_but_refuse_an_equally_scoped_other_grant() {
    let h = cursor_fixture();
    let issuing = h.pair().await;
    let other = h.pair().await;
    assert_ne!(issuing.grant.id, other.grant.id);
    for (route, _) in cursor_routes() {
        let (first, cursor) = first_page(&h, &issuing, &route).await;
        let (status, continued) = continue_page(&h, &issuing, &route, &cursor).await;
        assert_eq!(status, StatusCode::OK, "{route}: same-grant continuation: {continued}");
        assert_eq!(continued["value"]["items"].as_array().unwrap().len(), 1, "{route}: {continued}");
        assert_ne!(continued["value"]["items"], first["value"]["items"], "{route}: cursor must advance");
        // Both policies authorize this same route; only the authenticated grant differs.
        first_page(&h, &other, &route).await;
        let (status, denied) = continue_page(&h, &other, &route, &cursor).await;
        assert_refused(status, &denied, &route);
        // Refusal under B must not consume or invalidate A's valid continuation.
        let (status, repeated) = continue_page(&h, &issuing, &route, &cursor).await;
        assert_eq!(status, StatusCode::OK, "{route}: {repeated}");
        assert_eq!(repeated["value"]["items"], continued["value"]["items"]);
    }
}

#[tokio::test]
async fn paginated_routes_refuse_cursors_after_same_scope_policy_revision() {
    let h = cursor_fixture();
    let paired = h.pair().await;
    let mut issued = Vec::new();
    for (route, _) in cursor_routes() {
        let (_, cursor) = first_page(&h, &paired, &route).await;
        issued.push((route, cursor));
    }
    let before = h.owner("GET", &grant_path(&paired, ""), &()).await;
    // No read scope changes: refusal must be revision binding, not scope admission.
    let revised = h.policy(&paired, scopes()).await;
    assert_ne!(serde_json::to_value(&revised).unwrap()["policy_revision"], before["value"]["policy_revision"]);
    for (route, cursor) in issued {
        let (status, denied) = continue_page(&h, &paired, &route, &cursor).await;
        assert_refused(status, &denied, &route);
        let (_, current_cursor) = first_page(&h, &paired, &route).await;
        let (status, current) = continue_page(&h, &paired, &route, &current_cursor).await;
        assert_eq!(status, StatusCode::OK, "{route}: current revision must continue: {current}");
    }
}

#[tokio::test]
async fn revoked_issuing_grant_cursors_cannot_be_laundered_through_a_live_grant() {
    let h = cursor_fixture();
    let issuing = h.pair().await;
    let other = h.pair().await;
    let mut issued = Vec::new();
    for (route, _) in cursor_routes() {
        let (_, cursor) = first_page(&h, &issuing, &route).await;
        issued.push((route, cursor));
    }
    h.owner("POST", &grant_path(&issuing, "/revoke"), &json!({})).await;
    for (route, cursor) in issued {
        let (status, denied) = continue_page(&h, &issuing, &route, &cursor).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{route}: {denied}");
        assert_refused(status, &denied, &route);
        let (_, live_cursor) = first_page(&h, &other, &route).await;
        let (status, denied) = continue_page(&h, &other, &route, &cursor).await;
        assert_refused(status, &denied, &route);
        let (status, live) = continue_page(&h, &other, &route, &live_cursor).await;
        assert_eq!(status, StatusCode::OK, "{route}: live grant's own cursor: {live}");
    }
}

#[tokio::test]
async fn cached_continuations_recheck_each_routes_read_scope() {
    let h = cursor_fixture();
    let paired = h.pair().await;
    for (route, scope) in cursor_routes() {
        let (_, cursor) = first_page(&h, &paired, &route).await;
        // Narrow the authoritative policy directly; never weaken pairing's minimum scopes.
        h.remove_read_scope(&paired, scope);
        let (status, denied) = continue_page(&h, &paired, &route, &cursor).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{route}: {denied}");
        assert_refused(status, &denied, &route);
        h.policy(&paired, scopes()).await;
        // Restoring the scope is another policy revision, not revival of the old cursor.
        let (status, denied) = continue_page(&h, &paired, &route, &cursor).await;
        assert_refused(status, &denied, &route);
        let (_, restored_cursor) = first_page(&h, &paired, &route).await;
        let (status, restored) = continue_page(&h, &paired, &route, &restored_cursor).await;
        assert_eq!(status, StatusCode::OK, "{route}: restored policy: {restored}");
    }
}
