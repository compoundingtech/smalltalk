fn prepared_native_fixture(root: &Path) -> (AppState, String, std::path::PathBuf) {
    let mut state = test_state_named(root, "prepared-native");
    let home = root.join("home");
    let transcript = home.join(".codex/sessions/2026/10/05/prepared.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, format!("{}\n{}\n",
        json!({"type":"session_meta","timestamp":"2026-10-05T12:00:00Z","payload":{"id":"prepared-native","cwd":root,"source":"test"}}),
        json!({"type":"response_item","timestamp":"2026-10-05T12:00:01Z","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"Native answer one"}]}}),
    )).unwrap();
    state.native_session_home = Some(home);
    let owner = "agent/prepared-native";
    let directory = state.state_dir.join("drivers")
        .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24]).join("state");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("runtime.json"), serde_json::to_vec(&json!({"agent":"prepared-native","incarnation":"provider-one"})).unwrap()).unwrap();
    std::fs::write(directory.join("binding.json"), serde_json::to_vec(&json!({"agent":"prepared-native","runtimeIncarnation":"provider-one","threadId":"prepared-native"})).unwrap()).unwrap();
    for (kind, fields) in [
        ("runtime.observed", json!({"status":"running","runtime_id":"prepared-runtime","incarnation_id":"native-pty:one","terminal":true})),
        ("harness.observed", json!({"state":"working","driver":"codex","incarnation_id":"native-pty:one","evidence_incarnation":"provider-one"})),
    ] {
        state.store.append_claim(&ClaimInput {
            subject: owner.into(), kind: kind.into(), actor: Some(owner.into()),
            fields: fields.as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    }
    (state, super::managed_session_id(owner, "native-pty:one"), transcript)
}

#[tokio::test]
async fn prepared_initial_page_keeps_native_content_and_replays_a_later_message() {
    let root = tempfile::tempdir().unwrap();
    let (state, session_id, _) = prepared_native_fixture(root.path());
    let session = ClientSession::local(Some("person/alex")).unwrap();
    let first = conversation_changes_local(&state, &session, &session_id, None, 0).await.unwrap();
    let ready = conversation_changes_local(&state, &session, &session_id, None, 0).await.unwrap();
    assert_eq!(first["preparation"], "miss");
    assert_eq!(ready["preparation"], "ready");
    assert_eq!(first["initial_page"], ready["initial_page"]);
    assert!(ready["initial_page"]["items"].as_array().unwrap().iter().any(|entry| {
        entry["type"] == "content" && entry["body"]["text"] == "Native answer one"
    }));
    let cursor = ready["next_cursor"].as_str().unwrap();
    state.store.append_claim(&ClaimInput {
        subject: "message/prepared-later".into(), kind: "message.sent".into(), actor: Some("person/alex".into()),
        fields: json!({"from":"person/alex","to":"agent/prepared-native","session_id":session_id,"content":"A later human message","status":"sent"})
            .as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let changes = conversation_changes_local(&state, &session, &session_id, Some(cursor), 0).await.unwrap();
    assert!(changes.get("initial_page").is_none());
    assert!(changes["items"].as_array().unwrap().iter().any(|entry| {
        entry["type"] == "content" && entry["body"]["text"] == "A later human message"
    }));
    let refreshed = conversation_changes_local(&state, &session, &session_id, None, 0).await.unwrap();
    assert_eq!(refreshed["preparation"], "miss");
    assert!(refreshed["initial_page"]["items"].as_array().unwrap().iter().any(|entry| {
        entry["type"] == "content" && entry["body"]["text"] == "A later human message"
    }));
    let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let follower = tokio::spawn(follow_conversation(state, session, "native".into(), session_id, None, outbox));
    let (_, frame) = frames.recv().await.unwrap();
    follower.abort();
    assert_eq!(frame["replace"], true);
    assert_eq!(frame["preparation"], "ready");
    assert_eq!(frame["items"], refreshed["initial_page"]["items"]);
    assert_collection_frame_conforms(&frame);
}

#[test]
fn prepared_initial_page_rechecks_same_size_native_rewrites_and_authorization() {
    let root = tempfile::tempdir().unwrap();
    let (state, session_id, transcript) = prepared_native_fixture(root.path());
    let session = ClientSession::local(Some("person/alex")).unwrap();
    prepared_conversations::initial(&state, &session, &session_id).unwrap();
    let forbidden = ClientSession::for_tests("restricted", &session.authority_actor, "unix");
    assert_eq!(prepared_conversations::initial(&state, &forbidden, &session_id).unwrap_err().code, "forbidden");
    let modified = std::fs::metadata(&transcript).unwrap().modified().unwrap();
    let before = std::fs::read_to_string(&transcript).unwrap();
    let after = before.replace("Native answer one", "Native answer two");
    assert_eq!(before.len(), after.len());
    std::fs::write(&transcript, after).unwrap();
    std::fs::File::open(&transcript).unwrap().set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
    let refreshed = prepared_conversations::initial(&state, &session, &session_id).unwrap();
    assert_eq!(refreshed["preparation"], "miss");
    let entries = refreshed["initial_page"]["items"].as_array().unwrap();
    assert!(entries.iter().any(|entry| entry["type"] == "content" && entry["body"]["text"] == "Native answer two"));
    assert!(entries.iter().all(|entry| entry["body"]["text"] != "Native answer one"));
    let binding = state.state_dir.join("drivers")
        .join(&hex::encode(Sha256::digest(b"agent/prepared-native"))[..24])
        .join("state/binding.json");
    let mut rebound: Value = serde_json::from_slice(&std::fs::read(&binding).unwrap()).unwrap();
    rebound["threadId"] = json!("a-different-native-session");
    std::fs::write(binding, serde_json::to_vec(&rebound).unwrap()).unwrap();
    assert!(prepared_conversations::warm(&state, &session, &session_id).unwrap());
    let unavailable = prepared_conversations::initial(&state, &session, &session_id).unwrap();
    assert_eq!(unavailable["preparation"], "ready");
    assert!(unavailable["initial_page"]["items"].as_array().unwrap().iter()
        .any(|entry| entry["body"]["code"] == "transcript-not-bound"));
    assert!(unavailable["initial_page"]["items"].as_array().unwrap().iter()
        .all(|entry| entry["body"]["text"] != "Native answer two"));
    std::fs::write(transcript.with_file_name("a-different-native-session.jsonl"), format!("{}\n{}\n",
        json!({"type":"session_meta","timestamp":"2026-10-05T12:00:00Z","payload":{"id":"a-different-native-session","cwd":root.path(),"source":"test"}}),
        json!({"type":"response_item","timestamp":"2026-10-05T12:00:01Z","payload":{"type":"message","role":"assistant","id":"new-answer","content":[{"type":"output_text","text":"Now readable native transcript"}]}}),
    )).unwrap();
    let appeared = prepared_conversations::initial(&state, &session, &session_id).unwrap();
    assert_eq!(appeared["preparation"], "miss");
    assert!(appeared["initial_page"]["items"].as_array().unwrap().iter()
        .any(|entry| entry["body"]["text"] == "Now readable native transcript"));
    assert!(appeared["initial_page"]["items"].as_array().unwrap().iter()
        .all(|entry| entry["body"]["code"] != "transcript-not-bound"));
}

#[test]
fn prepared_initial_page_invalidates_replaced_and_finalized_claim_entries() {
    let root = tempfile::tempdir().unwrap();
    let state = test_state_named(root.path(), "prepared-claims");
    let owner = "agent/prepared-claims";
    let incarnation = "claims-runtime:one";
    let append = |kind: &str, fields: Value| {
        state.store.append_claim(&ClaimInput {
            subject: owner.into(), kind: kind.into(), actor: Some(owner.into()),
            fields: fields.as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    append("runtime.observed", json!({"status":"running","runtime_id":"claims-runtime","incarnation_id":incarnation,"terminal":false}));
    let session = ClientSession::local(Some("person/alex")).unwrap();
    let session_id = super::managed_session_id(owner, incarnation);
    for (operation, revision, final_entry, text) in [
        ("append", 1, false, "draft"),
        ("replace", 2, false, "revised"),
        ("finalize", 3, true, "final"),
    ] {
        append("harness.timeline", json!({
            "operation":operation, "entry_id":"timeline-entry/prepared-claims", "revision":revision,
            "role":"assistant", "entry_type":"content", "final":final_entry,
            "body":{"media_type":"text/plain","text":text}, "driver":"codex",
            "incarnation_id":incarnation, "sequence":100,
        }));
        let value = prepared_conversations::initial(&state, &session, &session_id).unwrap();
        let entry = value["initial_page"]["items"].as_array().unwrap().iter()
            .find(|entry| entry["id"] == "timeline-entry/prepared-claims").unwrap();
        assert_eq!(entry["revision"], revision);
        assert_eq!(entry["final"], final_entry);
        assert_eq!(entry["body"]["text"], text);
        let ready = prepared_conversations::initial(&state, &session, &session_id).unwrap();
        assert_eq!(ready["initial_page"]["items"], value["initial_page"]["items"]);
    }
}
