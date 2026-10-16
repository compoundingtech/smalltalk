//! Documents read through the client routes. Each test owns its store; no daemon, fleet or seat
//! is touched.
use super::client_documents::{
    ClientDocumentQuery, DocumentReferencesQuery, document_get, document_references, documents_list,
};
use super::client_v0::ClientSession;
use super::*;

fn session() -> Extension<ClientSession> {
    Extension(ClientSession::local(None).unwrap())
}

fn put(state: &AppState, name: &str, text: &str, head: &Option<String>) -> smallclaims::claim::DocumentVersion {
    state
        .store
        .put_document(name, text.as_bytes(), head, &format!("{name}:{}", text.len()))
        .unwrap()
}

async fn get(state: &AppState, name: &str) -> Result<Value, ApiError> {
    let query: ClientDocumentQuery = serde_json::from_value(json!({ "name": name })).unwrap();
    document_get(State(state.clone()), session(), Query(query)).await.map(|Json(value)| value)
}

async fn list(state: &AppState, query: Value) -> Value {
    let query: DocumentQuery = serde_json::from_value(query).unwrap();
    let Json(value) = documents_list(State(state.clone()), session(), Query(query)).await.unwrap();
    value
}

async fn references(state: &AppState, subject: &str) -> Result<Value, ApiError> {
    let query: DocumentReferencesQuery = serde_json::from_value(json!({ "subject": subject })).unwrap();
    document_references(State(state.clone()), session(), Query(query)).await.map(|Json(value)| value)
}

fn text(value: &Value) -> String {
    let bytes: Vec<u8> = serde_json::from_value(value["bytes"].clone()).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn long_document() -> String {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../fixtures/clients/documents-v1.json")).unwrap();
    fixture["links"][0]["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_document_reads_by_name_or_version_with_its_changed_time() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let first = put(&state, "doc/garden/plan", "# Plan\n\nFirst.\n", &None);
    let long = long_document();
    let second = put(&state, "doc/garden/plan", &long, &Some(first.binding_claim_id.clone()));

    let latest = get(&state, "doc/garden/plan").await.unwrap();
    assert_eq!(latest["reference"], format!("doc/garden/plan@{}", second.hash));
    assert_eq!(latest["name"], "doc/garden/plan");
    assert_eq!(latest["version"], second.hash);
    assert_eq!(latest["latest"], true);
    assert_eq!(latest["size"], long.len());
    assert_eq!(latest["changed_at_unix_ms"], second.created_at_unix_ms as u64);
    assert!(latest["changed_at_unix_ms"].as_u64().unwrap() > 0);
    // Tables, code blocks and nested lists come back byte for byte.
    assert_eq!(text(&latest), long);

    let pinned = get(&state, &format!("doc/garden/plan@{}", first.hash)).await.unwrap();
    assert_eq!(pinned["version"], first.hash);
    assert_eq!(pinned["latest"], false);
    assert_eq!(text(&pinned), "# Plan\n\nFirst.\n");
}

#[tokio::test]
async fn a_missing_document_says_whether_it_is_unknown_or_not_synced() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    put(&state, "doc/garden/plan", "# Plan", &None);

    let unknown = get(&state, "doc/garden/none").await.unwrap_err();
    assert_eq!((unknown.status, unknown.code.as_str()), (StatusCode::NOT_FOUND, "not-found"));
    let pinned = get(&state, &format!("doc/garden/plan@{}", "a".repeat(64))).await.unwrap_err();
    assert_eq!(
        (pinned.status, pinned.code.as_str()),
        (StatusCode::NOT_FOUND, "document-not-synced")
    );
    for invalid in ["garden/plan", "doc/", "doc/garden/plan@"] {
        let refused = get(&state, invalid).await.unwrap_err();
        assert_eq!(refused.code, "invalid-document-reference", "{invalid}");
    }
}

#[tokio::test]
async fn the_list_pages_under_a_prefix_with_versions_and_changed_times() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    for name in ["doc/garden/a", "doc/garden/b", "doc/garden/c", "doc/orchard/a"] {
        put(&state, name, &format!("# {name}"), &None);
    }
    let first = list(&state, json!({ "prefix": "doc/garden/", "limit": 2 })).await;
    let names = |page: &Value| {
        page["items"].as_array().unwrap().iter().map(|item| item["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(names(&first), ["doc/garden/a", "doc/garden/b"]);
    assert_eq!(first["has_more"], true);
    let item = &first["items"][0];
    assert_eq!(item["reference"], format!("doc/garden/a@{}", item["version"].as_str().unwrap()));
    assert!(item["changed_at_unix_ms"].as_u64().unwrap() > 0);
    assert_eq!(item["latest"], true);
    let page: st3_client::DocumentsPage = serde_json::from_value(first.clone()).unwrap();
    let second = list(
        &state,
        json!({ "prefix": "doc/garden/", "limit": 2, "cursor": page.next_cursor.unwrap() }),
    )
    .await;
    assert_eq!(names(&second), ["doc/garden/c"]);
    assert_eq!(second["has_more"], false);
    assert!(second["next_cursor"].is_null());
}

#[tokio::test]
async fn a_message_names_its_content_document_and_the_links_in_its_text() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    // A link cycle: each document links to the other.
    let a = put(&state, "doc/cycle/a", "Back to [b](doc/cycle/b).", &None);
    put(&state, "doc/cycle/b", "Back to [a](doc/cycle/a).", &None);
    let send = |key: &str, content: String| {
        accept_message_receipt(
            &state,
            MessageSendRequest {
                idempotency_key: key.into(),
                from: "person/alex".into(),
                to: "agent/garden/worker".into(),
                content,
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            },
            None,
            None,
        )
        .unwrap()
    };
    let linking = send("links", "Read doc/cycle/a, then doc/cycle/b and doc/cycle/a again.".into());
    let found = references(&state, &linking.message.subject).await.unwrap();
    assert_eq!(found["complete"], true);
    let refs: Vec<_> = found["references"].as_array().unwrap().iter().map(|r| (r["reference"].clone(), r["source"].clone())).collect();
    assert_eq!(refs, [(json!("doc/cycle/a"), json!("body")), (json!("doc/cycle/b"), json!("body"))]);

    let pinned = format!("doc/cycle/a@{}", a.hash);
    let document = send("document", pinned.clone());
    let found = references(&state, &document.message.subject).await.unwrap();
    assert_eq!(found["references"][0]["reference"], pinned);
    assert_eq!(found["references"][0]["source"], "content");
    assert_eq!(found["references"][0]["version"], a.hash);
    assert_eq!(found["references"].as_array().unwrap().len(), 1);

    let missing = references(&state, "message/none").await.unwrap_err();
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let refused = references(&state, "agent/garden/worker").await.unwrap_err();
    assert_eq!(refused.code, "validation-failed");
}

#[tokio::test]
async fn a_mission_run_and_step_name_their_pinned_documents() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let brief = put(&state, "doc/garden/brief", "# Brief", &None);
    let checklist = put(&state, "doc/garden/checklist", "# Checklist", &None);
    let source = format!(
        r#"version 2
mission "garden-docs" state="ready" {{
  goal "Plant the garden."
  step "plan" {{
    goal "Plan it."
    agentless
    document "doc/garden/brief@{brief}"
  }}
  step "plant" {{
    goal "Plant it."
    agentless
    document "doc/garden/checklist@{checklist}"
    document "doc/garden/brief@{brief}"
    depends-on {{ step "plan" completed }}
  }}
}}"#,
        brief = brief.hash,
        checklist = checklist.hash,
    );
    let intent = crate::graph::parse_intent(&source, "node").unwrap();
    let planned = state
        .store
        .mission(&intent, crate::model::IntentInput { kdl: source.clone(), source_name: None })
        .unwrap();
    state.store.apply(&intent, &planned.subject_tokens, "garden-docs").unwrap();
    let run = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: "garden-docs".into(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "garden-docs-run".into(),
        })
        .unwrap();

    let brief_ref = format!("doc/garden/brief@{}", brief.hash);
    let checklist_ref = format!("doc/garden/checklist@{}", checklist.hash);
    let pairs = |value: &Value| {
        value["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["reference"].as_str().unwrap().to_owned(), r["step"].as_str().unwrap().to_owned()))
            .collect::<Vec<_>>()
    };
    let expected = vec![(brief_ref.clone(), "plan".to_owned()), (checklist_ref.clone(), "plant".to_owned())];
    assert_eq!(pairs(&references(&state, "mission/garden-docs").await.unwrap()), expected);
    assert_eq!(pairs(&references(&state, &run.subject).await.unwrap()), expected);
    let plant = run.steps.iter().find(|step| step.step == "plant").expect("plant step").subject.clone();
    let step = references(&state, &plant).await.unwrap();
    assert_eq!(pairs(&step), [(checklist_ref, "plant".to_owned()), (brief_ref, "plant".to_owned())]);
    assert_eq!(step["references"][0]["source"], "step");

    let missing = references(&state, "mission/none").await.unwrap_err();
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}
